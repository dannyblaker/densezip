//! The plan tree: a reversible decomposition of a file's bytes.
//!
//! Each file in the archive is described by a sequence of segments. Rendering
//! the segments in order reproduces the original file byte-for-byte. Segment
//! payloads live in shared "channels" (plain text, preflate corrections, PNG
//! filter bytes, lepton blobs, per-image pixel blobs) which are compressed
//! independently with whichever backend wins.

use crate::util::{write_varint, Reader};
use anyhow::{bail, Result};

#[derive(Debug, Clone)]
pub enum Seg {
    /// `len` verbatim bytes taken from the plain channel.
    Raw { len: u64 },
    /// A bare DEFLATE stream, reconstructed with preflate.
    Deflate(DeflateSeg),
    /// zlib wrapper: 2-byte header + deflate body + adler32 (recomputed).
    Zlib { header: [u8; 2], body: DeflateSeg },
    /// gzip member: header (from plain channel) + deflate body + crc32/isize trailer (recomputed).
    Gzip { header_len: u32, body: DeflateSeg },
    /// The contiguous run of IDAT chunks in a PNG file (chunk framing + zlib + filters).
    PngIdat(PngIdatSeg),
    /// A JPEG, stored as a lepton blob.
    Jpeg { lepton_len: u64, orig_len: u64 },
}

#[derive(Debug, Clone)]
pub struct DeflateSeg {
    /// Bytes to take from the corrections channel (preflate reconstruction data).
    pub corrections_len: u64,
    /// Total length of the decompressed plain text.
    pub plain_len: u64,
    /// Decomposition of the plain text (recursively scanned).
    pub inner: Vec<Seg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelEnc {
    /// Filtered scanline data stored as-is in the plain channel.
    Filtered,
    /// Filter-type bytes (one per row) in the filters channel; unfiltered
    /// pixels in pixel blob `blob`.
    Unfiltered { blob: u32 },
}

#[derive(Debug, Clone)]
pub struct PngIdatSeg {
    /// Original sizes of each IDAT chunk (framing is re-emitted, CRCs recomputed).
    pub chunk_lens: Vec<u32>,
    pub zlib_header: [u8; 2],
    /// Bytes inside the IDAT run after the zlib stream ended (rare; stored inline).
    pub tail: Vec<u8>,
    pub corrections_len: u64,
    /// Length of the filtered scanline data (the zlib plain text).
    pub plain_len: u64,
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub color_type: u8,
    pub pixels: PixelEnc,
}

impl PngIdatSeg {
    /// Bytes per pixel unit used by PNG filters (>= 1).
    pub fn filter_bpp(&self) -> usize {
        let channels = match self.color_type {
            0 => 1, // grayscale
            2 => 3, // rgb
            3 => 1, // palette
            4 => 2, // gray+alpha
            6 => 4, // rgba
            _ => 1,
        };
        let bits = self.bit_depth as usize * channels;
        bits.div_ceil(8)
    }

    /// Bytes per scanline, excluding the filter-type byte.
    pub fn stride(&self) -> usize {
        let channels = match self.color_type {
            0 => 1,
            2 => 3,
            3 => 1,
            4 => 2,
            6 => 4,
            _ => 1,
        };
        let bits = self.width as usize * self.bit_depth as usize * channels;
        bits.div_ceil(8)
    }
}

const TAG_RAW: u8 = 0;
const TAG_DEFLATE: u8 = 1;
const TAG_ZLIB: u8 = 2;
const TAG_GZIP: u8 = 3;
const TAG_PNG: u8 = 4;
const TAG_JPEG: u8 = 5;

pub fn write_segs(out: &mut Vec<u8>, segs: &[Seg]) {
    write_varint(out, segs.len() as u64);
    for s in segs {
        write_seg(out, s);
    }
}

fn write_deflate_body(out: &mut Vec<u8>, d: &DeflateSeg) {
    write_varint(out, d.corrections_len);
    write_varint(out, d.plain_len);
    write_segs(out, &d.inner);
}

fn write_seg(out: &mut Vec<u8>, seg: &Seg) {
    match seg {
        Seg::Raw { len } => {
            out.push(TAG_RAW);
            write_varint(out, *len);
        }
        Seg::Deflate(d) => {
            out.push(TAG_DEFLATE);
            write_deflate_body(out, d);
        }
        Seg::Zlib { header, body } => {
            out.push(TAG_ZLIB);
            out.extend_from_slice(header);
            write_deflate_body(out, body);
        }
        Seg::Gzip { header_len, body } => {
            out.push(TAG_GZIP);
            write_varint(out, *header_len as u64);
            write_deflate_body(out, body);
        }
        Seg::PngIdat(p) => {
            out.push(TAG_PNG);
            write_varint(out, p.chunk_lens.len() as u64);
            for &l in &p.chunk_lens {
                write_varint(out, l as u64);
            }
            out.extend_from_slice(&p.zlib_header);
            write_varint(out, p.tail.len() as u64);
            out.extend_from_slice(&p.tail);
            write_varint(out, p.corrections_len);
            write_varint(out, p.plain_len);
            write_varint(out, p.width as u64);
            write_varint(out, p.height as u64);
            out.push(p.bit_depth);
            out.push(p.color_type);
            match p.pixels {
                PixelEnc::Filtered => out.push(0),
                PixelEnc::Unfiltered { blob } => {
                    out.push(1);
                    write_varint(out, blob as u64);
                }
            }
        }
        Seg::Jpeg { lepton_len, orig_len } => {
            out.push(TAG_JPEG);
            write_varint(out, *lepton_len);
            write_varint(out, *orig_len);
        }
    }
}

pub fn read_segs(r: &mut Reader) -> Result<Vec<Seg>> {
    let n = r.varint()? as usize;
    if n > 1 << 24 {
        bail!("implausible segment count {}", n);
    }
    let mut segs = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        segs.push(read_seg(r)?);
    }
    Ok(segs)
}

fn read_deflate_body(r: &mut Reader) -> Result<DeflateSeg> {
    let corrections_len = r.varint()?;
    let plain_len = r.varint()?;
    let inner = read_segs(r)?;
    Ok(DeflateSeg { corrections_len, plain_len, inner })
}

fn read_seg(r: &mut Reader) -> Result<Seg> {
    let tag = r.byte()?;
    Ok(match tag {
        TAG_RAW => Seg::Raw { len: r.varint()? },
        TAG_DEFLATE => Seg::Deflate(read_deflate_body(r)?),
        TAG_ZLIB => {
            let h = r.bytes(2)?;
            let header = [h[0], h[1]];
            Seg::Zlib { header, body: read_deflate_body(r)? }
        }
        TAG_GZIP => {
            let header_len = r.varint()? as u32;
            Seg::Gzip { header_len, body: read_deflate_body(r)? }
        }
        TAG_PNG => {
            let n = r.varint()? as usize;
            if n > 1 << 22 {
                bail!("implausible chunk count");
            }
            let mut chunk_lens = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                chunk_lens.push(r.varint()? as u32);
            }
            let h = r.bytes(2)?;
            let zlib_header = [h[0], h[1]];
            let tail_len = r.varint()? as usize;
            let tail = r.bytes(tail_len)?.to_vec();
            let corrections_len = r.varint()?;
            let plain_len = r.varint()?;
            let width = r.varint()? as u32;
            let height = r.varint()? as u32;
            let bit_depth = r.byte()?;
            let color_type = r.byte()?;
            let pixels = match r.byte()? {
                0 => PixelEnc::Filtered,
                1 => PixelEnc::Unfiltered { blob: r.varint()? as u32 },
                x => bail!("bad pixel mode {}", x),
            };
            Seg::PngIdat(PngIdatSeg {
                chunk_lens,
                zlib_header,
                tail,
                corrections_len,
                plain_len,
                width,
                height,
                bit_depth,
                color_type,
                pixels,
            })
        }
        TAG_JPEG => Seg::Jpeg { lepton_len: r.varint()?, orig_len: r.varint()? },
        x => bail!("bad segment tag {}", x),
    })
}

/// Offset every pixel-blob index in the tree (used when committing a per-file
/// plan into the global channel set).
pub fn remap_blobs(segs: &mut [Seg], offset: u32) {
    for s in segs {
        match s {
            Seg::PngIdat(p) => {
                if let PixelEnc::Unfiltered { blob } = &mut p.pixels {
                    *blob += offset;
                }
            }
            Seg::Deflate(d) => remap_blobs(&mut d.inner, offset),
            Seg::Zlib { body, .. } => remap_blobs(&mut body.inner, offset),
            Seg::Gzip { body, .. } => remap_blobs(&mut body.inner, offset),
            _ => {}
        }
    }
}
