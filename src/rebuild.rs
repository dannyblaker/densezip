//! Rendering: turn a segment tree + channels back into the original bytes.

use crate::channels::Cursors;
use crate::plan::{DeflateSeg, PixelEnc, PngIdatSeg, Seg};
use crate::util::adler32;
use anyhow::{Context, Result, ensure};
use preflate_rs::recreate_whole_deflate_stream;

pub fn render_segs(segs: &[Seg], cur: &mut Cursors, out: &mut Vec<u8>) -> Result<()> {
    for seg in segs {
        render_seg(seg, cur, out)?;
    }
    Ok(())
}

/// Renders a deflate segment; returns (deflate stream bytes, plain text bytes).
fn render_deflate(d: &DeflateSeg, cur: &mut Cursors) -> Result<(Vec<u8>, Vec<u8>)> {
    let corrections = cur.take_corrections(d.corrections_len as usize)?.to_vec();
    let mut plain = Vec::with_capacity(d.plain_len as usize);
    render_segs(&d.inner, cur, &mut plain)?;
    ensure!(
        plain.len() as u64 == d.plain_len,
        "deflate plain text length mismatch: {} != {}",
        plain.len(),
        d.plain_len
    );
    let deflate = recreate_whole_deflate_stream(&plain, &corrections)
        .map_err(|e| anyhow::anyhow!("preflate recreate failed: {:?}", e))?;
    Ok((deflate, plain))
}

fn render_seg(seg: &Seg, cur: &mut Cursors, out: &mut Vec<u8>) -> Result<()> {
    match seg {
        Seg::Raw { len } => {
            out.extend_from_slice(cur.take_plain(*len as usize)?);
        }
        Seg::Deflate(d) => {
            let (deflate, _) = render_deflate(d, cur)?;
            out.extend_from_slice(&deflate);
        }
        Seg::Zlib { header, body } => {
            let (deflate, plain) = render_deflate(body, cur)?;
            out.extend_from_slice(header);
            out.extend_from_slice(&deflate);
            out.extend_from_slice(&adler32(&plain).to_be_bytes());
        }
        Seg::Gzip { header_len, body } => {
            let header = cur.take_plain(*header_len as usize)?.to_vec();
            let (deflate, plain) = render_deflate(body, cur)?;
            out.extend_from_slice(&header);
            out.extend_from_slice(&deflate);
            out.extend_from_slice(&crc32fast::hash(&plain).to_le_bytes());
            out.extend_from_slice(&(plain.len() as u32).to_le_bytes());
        }
        Seg::PngIdat(p) => render_png(p, cur, out)?,
        Seg::Jpeg {
            lepton_len,
            orig_len,
        } => {
            let blob = cur.take_lepton(*lepton_len as usize)?;
            let mut jpeg = Vec::with_capacity(*orig_len as usize);
            lepton_jpeg::decode_lepton(
                &mut std::io::Cursor::new(blob),
                &mut jpeg,
                &lepton_jpeg::EnabledFeatures::compat_lepton_vector_read(),
                &lepton_jpeg::SingleThreadPool {},
            )
            .map_err(|e| anyhow::anyhow!("lepton decode failed: {:?}", e))?;
            ensure!(
                jpeg.len() as u64 == *orig_len,
                "lepton output length mismatch"
            );
            out.extend_from_slice(&jpeg);
        }
    }
    Ok(())
}

fn render_png(p: &PngIdatSeg, cur: &mut Cursors, out: &mut Vec<u8>) -> Result<()> {
    // 1. Filtered scanline data (the zlib plain text).
    let plain: Vec<u8> = match p.pixels {
        PixelEnc::Filtered => cur.take_plain(p.plain_len as usize)?.to_vec(),
        PixelEnc::Unfiltered { blob } => {
            let filters = cur.take_filters(p.height as usize)?.to_vec();
            let blob = p
                .ch_blob(cur)
                .with_context(|| format!("missing pixel blob {}", blob))?;
            refilter(p, &filters, blob)?
        }
    };
    ensure!(
        plain.len() as u64 == p.plain_len,
        "png plain length mismatch"
    );

    // 2. zlib stream.
    let corrections = cur.take_corrections(p.corrections_len as usize)?.to_vec();
    let deflate = recreate_whole_deflate_stream(&plain, &corrections)
        .map_err(|e| anyhow::anyhow!("preflate recreate (png) failed: {:?}", e))?;
    let mut zlib = Vec::with_capacity(deflate.len() + 6 + p.tail.len());
    zlib.extend_from_slice(&p.zlib_header);
    zlib.extend_from_slice(&deflate);
    zlib.extend_from_slice(&adler32(&plain).to_be_bytes());
    zlib.extend_from_slice(&p.tail);

    // 3. Re-emit IDAT chunk framing.
    let total: u64 = p.chunk_lens.iter().map(|&l| l as u64).sum();
    ensure!(
        total == zlib.len() as u64,
        "png chunk lengths mismatch zlib size"
    );
    let mut off = 0usize;
    for &l in &p.chunk_lens {
        let l = l as usize;
        out.extend_from_slice(&(l as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(b"IDAT");
        out.extend_from_slice(&zlib[off..off + l]);
        let crc = crc32fast::hash(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
        off += l;
    }
    Ok(())
}

impl PngIdatSeg {
    fn ch_blob<'a>(&self, cur: &Cursors<'a>) -> Option<&'a [u8]> {
        match self.pixels {
            PixelEnc::Unfiltered { blob } => cur
                .ch
                .pixel_blobs
                .get(blob as usize)
                .map(|b| b.data.as_slice()),
            PixelEnc::Filtered => None,
        }
    }
}

fn paeth(a: i32, b: i32, c: i32) -> i32 {
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Re-apply PNG filters to raw pixels, producing the filtered scanline data
/// (filter-type byte + filtered bytes per row).
pub fn refilter(p: &PngIdatSeg, filters: &[u8], pixels: &[u8]) -> Result<Vec<u8>> {
    let stride = p.stride();
    let bpp = p.filter_bpp();
    let h = p.height as usize;
    ensure!(filters.len() == h, "filter count mismatch");
    ensure!(pixels.len() == stride * h, "pixel data size mismatch");
    let mut out = Vec::with_capacity(h * (stride + 1));
    let zero_row = vec![0u8; stride];
    for y in 0..h {
        let ft = filters[y];
        out.push(ft);
        let row = &pixels[y * stride..(y + 1) * stride];
        let prev: &[u8] = if y == 0 {
            &zero_row
        } else {
            &pixels[(y - 1) * stride..y * stride]
        };
        match ft {
            0 => out.extend_from_slice(row),
            1 => {
                for i in 0..stride {
                    let a = if i >= bpp { row[i - bpp] as i32 } else { 0 };
                    out.push((row[i] as i32 - a) as u8);
                }
            }
            2 => {
                for i in 0..stride {
                    out.push((row[i] as i32 - prev[i] as i32) as u8);
                }
            }
            3 => {
                for i in 0..stride {
                    let a = if i >= bpp { row[i - bpp] as i32 } else { 0 };
                    out.push((row[i] as i32 - (a + prev[i] as i32) / 2) as u8);
                }
            }
            4 => {
                for i in 0..stride {
                    let a = if i >= bpp { row[i - bpp] as i32 } else { 0 };
                    let c = if i >= bpp { prev[i - bpp] as i32 } else { 0 };
                    out.push((row[i] as i32 - paeth(a, prev[i] as i32, c)) as u8);
                }
            }
            x => anyhow::bail!("bad filter type {}", x),
        }
    }
    Ok(out)
}

/// Remove PNG filters: filtered scanline data -> (filter bytes, raw pixels).
/// Returns None if the data doesn't have the expected shape.
pub fn unfilter(p: &PngIdatSeg, plain: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let stride = p.stride();
    let bpp = p.filter_bpp();
    let h = p.height as usize;
    if stride == 0 || h == 0 || plain.len() != h * (stride + 1) {
        return None;
    }
    let mut filters = Vec::with_capacity(h);
    let mut pixels = vec![0u8; h * stride];
    for y in 0..h {
        let ft = plain[y * (stride + 1)];
        if ft > 4 {
            return None;
        }
        filters.push(ft);
        let src = &plain[y * (stride + 1) + 1..(y + 1) * (stride + 1)];
        // Split pixels at row y so we can read the previous row while writing this one.
        let (prev_rows, rest) = pixels.split_at_mut(y * stride);
        let row = &mut rest[..stride];
        let zero_row = vec![0u8; stride];
        let prev: &[u8] = if y == 0 {
            &zero_row
        } else {
            &prev_rows[(y - 1) * stride..]
        };
        match ft {
            0 => row.copy_from_slice(src),
            1 => {
                for i in 0..stride {
                    let a = if i >= bpp { row[i - bpp] as i32 } else { 0 };
                    row[i] = (src[i] as i32 + a) as u8;
                }
            }
            2 => {
                for i in 0..stride {
                    row[i] = (src[i] as i32 + prev[i] as i32) as u8;
                }
            }
            3 => {
                for i in 0..stride {
                    let a = if i >= bpp { row[i - bpp] as i32 } else { 0 };
                    row[i] = (src[i] as i32 + (a + prev[i] as i32) / 2) as u8;
                }
            }
            4 => {
                for i in 0..stride {
                    let a = if i >= bpp { row[i - bpp] as i32 } else { 0 };
                    let c = if i >= bpp { prev[i - bpp] as i32 } else { 0 };
                    row[i] = (src[i] as i32 + paeth(a, prev[i] as i32, c)) as u8;
                }
            }
            _ => return None,
        }
    }
    Some((filters, pixels))
}
