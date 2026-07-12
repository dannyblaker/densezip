//! Planning: scan a file's bytes for recompressible structures (deflate
//! streams, PNGs, JPEGs) and build a reversible segment tree.
//!
//! Every accepted transform is verified: preflate runs with
//! `verify_compression` on, lepton verifies decode-compare internally, and the
//! whole file is additionally re-rendered and compared before commit
//! (see archive.rs). A failed attempt rolls the channels back and the bytes
//! stay literal, so correctness never depends on scanner heuristics.

use crate::channels::{Channels, PixelBlob};
use crate::plan::{DeflateSeg, PixelEnc, PngIdatSeg, Seg};
use crate::rebuild::unfilter;
use crate::util::adler32;
use preflate_rs::{preflate_whole_deflate_stream, PreflateConfig};

const MAX_DEPTH: u32 = 3;
/// Don't bother recompressing streams whose plain text is smaller than this;
/// correction + metadata overhead eats the gain.
const MIN_PLAIN: usize = 64;
const MIN_JPEG: usize = 1024;

fn preflate_config() -> PreflateConfig {
    PreflateConfig {
        max_chain_length: 1 << 16,
        plain_text_limit: 1 << 30,
        verify_compression: true,
    }
}

#[derive(Debug)]
enum Cand {
    Zlib,
    /// Bare deflate stream at a known offset (e.g. zip entry body).
    RawDeflate,
    Gzip,
    Png(PngInfo),
    Jpeg,
}

#[derive(Debug)]
struct PngInfo {
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: u8,
    interlace: u8,
    /// Sizes of the contiguous IDAT chunk run starting at the candidate pos.
    chunk_lens: Vec<u32>,
}

pub fn plan_bytes(data: &[u8], depth: u32, ch: &mut Channels) -> Vec<Seg> {
    let mut segs: Vec<Seg> = Vec::new();
    let mut lit_start = 0usize;
    let mut cursor = 0usize;

    let cands = collect_candidates(data);
    for (pos, cand) in cands {
        if pos < cursor {
            continue;
        }
        let mark = ch.mark(segs.len());
        // Literal run before the candidate must hit the plain channel first.
        if pos > lit_start {
            ch.plain.extend_from_slice(&data[lit_start..pos]);
            segs.push(Seg::Raw { len: (pos - lit_start) as u64 });
        }
        match try_build(&cand, data, pos, depth, ch) {
            Some((seg, consumed)) => {
                segs.push(seg);
                cursor = pos + consumed;
                lit_start = cursor;
            }
            None => {
                ch.rollback(mark);
                segs.truncate(mark.segs);
            }
        }
    }
    if data.len() > lit_start {
        ch.plain.extend_from_slice(&data[lit_start..]);
        segs.push(Seg::Raw { len: (data.len() - lit_start) as u64 });
    }
    segs
}

fn collect_candidates(data: &[u8]) -> Vec<(usize, Cand)> {
    let mut out: Vec<(usize, Cand)> = Vec::new();
    let n = data.len();
    let mut i = 0usize;
    while i + 2 < n {
        match data[i] {
            0x89 => {
                if data[i..].starts_with(b"\x89PNG\r\n\x1a\n") {
                    if let Some((idat_pos, info)) = parse_png(&data[i..]) {
                        out.push((i + idat_pos, Cand::Png(info)));
                    }
                }
            }
            0x1f => {
                if data[i + 1] == 0x8b && data[i + 2] == 0x08 {
                    out.push((i, Cand::Gzip));
                }
            }
            0x50 => {
                // zip local file header with deflate method
                if data[i..].starts_with(b"PK\x03\x04") && i + 30 <= n {
                    let method = u16::from_le_bytes([data[i + 8], data[i + 9]]);
                    if method == 8 {
                        let nlen = u16::from_le_bytes([data[i + 26], data[i + 27]]) as usize;
                        let elen = u16::from_le_bytes([data[i + 28], data[i + 29]]) as usize;
                        let dstart = i + 30 + nlen + elen;
                        if dstart < n {
                            out.push((dstart, Cand::RawDeflate));
                        }
                    }
                }
            }
            0x78 => {
                let h = ((data[i] as u16) << 8) | data[i + 1] as u16;
                // valid zlib header: deflate method, window <= 32k, no preset dict
                if h % 31 == 0 && data[i + 1] & 0x20 == 0 {
                    out.push((i, Cand::Zlib));
                }
            }
            0xff => {
                if data[i + 1] == 0xd8 && data[i + 2] == 0xff {
                    out.push((i, Cand::Jpeg));
                }
            }
            _ => {}
        }
        i += 1;
        if out.len() > 200_000 {
            break; // pathological input; scanning only the prefix is still sound
        }
    }
    out.sort_by_key(|(p, _)| *p);
    out
}

/// Parse PNG chunks; returns (offset of first IDAT chunk relative to png
/// start, info). Requires valid chunk CRCs so reconstruction is exact.
fn parse_png(png: &[u8]) -> Option<(usize, PngInfo)> {
    let mut pos = 8usize;
    let mut info: Option<(u32, u32, u8, u8, u8)> = None;
    let mut idat_start: Option<usize> = None;
    let mut chunk_lens: Vec<u32> = Vec::new();
    while pos + 12 <= png.len() {
        let len = u32::from_be_bytes(png[pos..pos + 4].try_into().ok()?) as usize;
        if pos + 12 + len > png.len() {
            return None;
        }
        let ctype = &png[pos + 4..pos + 8];
        let crc = u32::from_be_bytes(png[pos + 8 + len..pos + 12 + len].try_into().ok()?);
        if crc32fast::hash(&png[pos + 4..pos + 8 + len]) != crc {
            return None; // non-standard CRC; leave this PNG alone
        }
        match ctype {
            b"IHDR" => {
                if len != 13 {
                    return None;
                }
                let d = &png[pos + 8..pos + 21];
                info = Some((
                    u32::from_be_bytes(d[0..4].try_into().ok()?),
                    u32::from_be_bytes(d[4..8].try_into().ok()?),
                    d[8],
                    d[9],
                    d[12],
                ));
            }
            b"IDAT" => {
                if idat_start.is_none() {
                    idat_start = Some(pos);
                }
                chunk_lens.push(len as u32);
            }
            b"IEND" => break,
            _ => {
                if idat_start.is_some() {
                    break; // IDAT run ended
                }
            }
        }
        pos += 12 + len;
    }
    let (width, height, bit_depth, color_type, interlace) = info?;
    let idat_start = idat_start?;
    if chunk_lens.is_empty() || width == 0 || height == 0 {
        return None;
    }
    Some((idat_start, PngInfo { width, height, bit_depth, color_type, interlace, chunk_lens }))
}

struct DeflateHit {
    seg: DeflateSeg,
    /// Bytes of deflate stream consumed from the input.
    deflate_len: usize,
    plain_adler: u32,
    plain_crc: u32,
    plain_isize: u32,
}

/// Attempt to preflate a deflate stream at `data[pos..]`. On success, pushes
/// corrections and recursively-scanned plain text into channels.
fn try_deflate(data: &[u8], pos: usize, depth: u32, ch: &mut Channels) -> Option<DeflateHit> {
    if pos >= data.len() {
        return None;
    }
    let (result, plain) = preflate_whole_deflate_stream(&data[pos..], &preflate_config()).ok()?;
    let plain = plain.text();
    if plain.len() < MIN_PLAIN || result.compressed_size < 16 {
        return None;
    }
    let plain_adler = adler32(plain);
    let plain_crc = crc32fast::hash(plain);
    let plain_isize = plain.len() as u32;
    ch.corrections.extend_from_slice(&result.corrections);
    let inner = if depth < MAX_DEPTH {
        plan_bytes(plain, depth + 1, ch)
    } else {
        ch.plain.extend_from_slice(plain);
        vec![Seg::Raw { len: plain.len() as u64 }]
    };
    Some(DeflateHit {
        seg: DeflateSeg {
            corrections_len: result.corrections.len() as u64,
            plain_len: plain.len() as u64,
            inner,
        },
        deflate_len: result.compressed_size,
        plain_adler,
        plain_crc,
        plain_isize,
    })
}

fn try_build(
    cand: &Cand,
    data: &[u8],
    pos: usize,
    depth: u32,
    ch: &mut Channels,
) -> Option<(Seg, usize)> {
    match cand {
        Cand::RawDeflate => {
            let hit = try_deflate(data, pos, depth, ch)?;
            let dlen = hit.deflate_len;
            Some((Seg::Deflate(hit.seg), dlen))
        }
        Cand::Zlib => {
            let hit = try_deflate(data, pos + 2, depth, ch)?;
            let at = pos + 2 + hit.deflate_len;
            // The adler32 trailer is recomputed on rebuild, so it must match here.
            if at + 4 <= data.len() && data[at..at + 4] == hit.plain_adler.to_be_bytes() {
                Some((
                    Seg::Zlib { header: [data[pos], data[pos + 1]], body: hit.seg },
                    2 + hit.deflate_len + 4,
                ))
            } else {
                None
            }
        }
        Cand::Gzip => {
            let hlen = gzip_header_len(&data[pos..])?;
            ch.plain.extend_from_slice(&data[pos..pos + hlen]);
            let hit = try_deflate(data, pos + hlen, depth, ch)?;
            let at = pos + hlen + hit.deflate_len;
            if at + 8 <= data.len()
                && data[at..at + 4] == hit.plain_crc.to_le_bytes()
                && data[at + 4..at + 8] == hit.plain_isize.to_le_bytes()
            {
                Some((
                    Seg::Gzip { header_len: hlen as u32, body: hit.seg },
                    hlen + hit.deflate_len + 8,
                ))
            } else {
                None // caller rolls back the header bytes we pushed
            }
        }
        Cand::Png(info) => try_png(info, data, pos, ch),
        Cand::Jpeg => try_jpeg(data, pos, ch),
    }
}

fn gzip_header_len(g: &[u8]) -> Option<usize> {
    if g.len() < 10 {
        return None;
    }
    let flg = g[3];
    let mut p = 10usize;
    if flg & 0x04 != 0 {
        // FEXTRA
        if p + 2 > g.len() {
            return None;
        }
        let xlen = u16::from_le_bytes([g[p], g[p + 1]]) as usize;
        p += 2 + xlen;
    }
    if flg & 0x08 != 0 {
        // FNAME
        p += g.get(p..)?.iter().position(|&b| b == 0)? + 1;
    }
    if flg & 0x10 != 0 {
        // FCOMMENT
        p += g.get(p..)?.iter().position(|&b| b == 0)? + 1;
    }
    if flg & 0x02 != 0 {
        // FHCRC
        p += 2;
    }
    if p >= g.len() {
        return None;
    }
    Some(p)
}

fn try_png(info: &PngInfo, data: &[u8], pos: usize, ch: &mut Channels) -> Option<(Seg, usize)> {
    // Total span of the IDAT run in the file.
    let run_len: usize = info.chunk_lens.iter().map(|&l| l as usize + 12).sum();
    if pos + run_len > data.len() {
        return None;
    }
    // Concatenate IDAT payloads => zlib stream.
    let mut zlib = Vec::new();
    let mut p = pos;
    for &l in &info.chunk_lens {
        zlib.extend_from_slice(&data[p + 8..p + 8 + l as usize]);
        p += 12 + l as usize;
    }
    if zlib.len() < 6 {
        return None;
    }
    let h = ((zlib[0] as u16) << 8) | zlib[1] as u16;
    if h % 31 != 0 || zlib[0] & 0x0f != 8 || zlib[1] & 0x20 != 0 {
        return None;
    }
    let (result, plain) = preflate_whole_deflate_stream(&zlib[2..], &preflate_config()).ok()?;
    let plain = plain.text();
    if plain.len() < MIN_PLAIN {
        return None;
    }
    let body_end = 2 + result.compressed_size;
    if body_end + 4 > zlib.len() {
        return None;
    }
    if zlib[body_end..body_end + 4] != adler32(plain).to_be_bytes() {
        return None;
    }
    let tail = zlib[body_end + 4..].to_vec();

    let mut seg = PngIdatSeg {
        chunk_lens: info.chunk_lens.clone(),
        zlib_header: [zlib[0], zlib[1]],
        tail,
        corrections_len: result.corrections.len() as u64,
        plain_len: plain.len() as u64,
        width: info.width,
        height: info.height,
        bit_depth: info.bit_depth,
        color_type: info.color_type,
        pixels: PixelEnc::Filtered,
    };

    ch.corrections.extend_from_slice(&result.corrections);

    // Prefer raw pixels when they compress better than filtered scanlines.
    let mut chose_unfiltered = false;
    if info.interlace == 0 {
        if let Some((filters, pixels)) = unfilter(&seg, plain) {
            let est_f = quick_estimate(plain);
            let est_u = quick_estimate(&pixels);
            if est_u < est_f {
                seg.pixels = PixelEnc::Unfiltered { blob: ch.pixel_blobs.len() as u32 };
                ch.filters.extend_from_slice(&filters);
                ch.pixel_blobs.push(PixelBlob {
                    data: pixels,
                    stride: seg.stride() as u32,
                    bpp: seg.filter_bpp() as u32,
                });
                chose_unfiltered = true;
            }
        }
    }
    if !chose_unfiltered {
        ch.plain.extend_from_slice(plain);
    }
    Some((Seg::PngIdat(seg), run_len))
}

fn quick_estimate(data: &[u8]) -> usize {
    zstd::bulk::compress(data, 12).map(|v| v.len()).unwrap_or(usize::MAX)
}

fn try_jpeg(data: &[u8], pos: usize, ch: &mut Channels) -> Option<(Seg, usize)> {
    // Find EOI candidates; entropy-coded data escapes 0xff, so the first
    // FFD9 is nearly always the real EOI. lepton verifies decode-compare
    // internally, so a wrong guess is rejected; try a couple more if so.
    let hay = &data[pos..];
    if hay.len() < MIN_JPEG {
        return None;
    }
    let mut tries = 0;
    let mut search_from = 2usize;
    while tries < 3 {
        let rel = find_marker(&hay[search_from..], &[0xff, 0xd9])?;
        let end = search_from + rel + 2;
        if end >= MIN_JPEG {
            let slice = &hay[..end];
            let features = lepton_jpeg::EnabledFeatures::compat_lepton_vector_write();
            let pool = lepton_jpeg::SingleThreadPool {};
            if let Ok((blob, _metrics)) = lepton_jpeg::encode_lepton_verify(slice, &features, &pool)
            {
                if blob.len() < slice.len() {
                    ch.lepton.extend_from_slice(&blob);
                    return Some((
                        Seg::Jpeg { lepton_len: blob.len() as u64, orig_len: slice.len() as u64 },
                        end,
                    ));
                }
            }
            tries += 1;
        }
        search_from = end;
        if search_from >= hay.len() {
            return None;
        }
    }
    None
}

fn find_marker(hay: &[u8], needle: &[u8; 2]) -> Option<usize> {
    hay.windows(2).position(|w| w == needle)
}
