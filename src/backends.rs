//! Compression backends. Every channel is compressed with all applicable
//! backends in parallel and the smallest verified output wins.

use anyhow::{Result, bail};
use lzma_rust2::{LzmaOptions, LzmaReader, LzmaWriter};
use rayon::prelude::*;
use std::io::{Read, Write};

pub const STORE: u8 = 0;
pub const ZSTD: u8 = 1;
pub const BROTLI: u8 = 2;
pub const LZMA: u8 = 3;
pub const CM: u8 = 4;

pub fn name(id: u8) -> &'static str {
    match id {
        STORE => "store",
        ZSTD => "zstd",
        BROTLI => "brotli",
        LZMA => "lzma",
        CM => "dzcm",
        _ => "?",
    }
}

/// Memory budget for one compression job. Splits between the CM model (the
/// biggest consumer) and the LZMA encoder (~11.5x its dictionary size).
#[derive(Debug, Clone, Copy)]
pub struct MemBudget {
    pub bytes: u64,
}

impl MemBudget {
    pub const UNLIMITED: MemBudget = MemBudget { bytes: u64::MAX };

    pub fn cm_cap(&self) -> u64 {
        if self.bytes == u64::MAX {
            u64::MAX
        } else {
            self.bytes / 2
        }
    }

    pub fn lzma_dict_cap(&self) -> u64 {
        if self.bytes == u64::MAX {
            1 << 30
        } else {
            (self.bytes / 24).max(1 << 20)
        }
    }
}

fn lzma_options(len: usize, dict_cap: u64) -> LzmaOptions {
    let mut opts = LzmaOptions::with_preset(9);
    let dict = (len.max(1 << 16).next_power_of_two() as u64)
        .min(1 << 30)
        .min(dict_cap) as u32;
    opts.dict_size = dict;
    opts.nice_len = 273;
    opts
}

/// The .lzma header is self-describing (props + dict size), so multiple
/// parameter sets can race under the same backend id. pb=0 often wins on
/// text; lc=0,pb=0 on some binary/record data.
fn lzma_compress(data: &[u8], dict_cap: u64) -> Result<Vec<u8>> {
    let mut best: Option<Vec<u8>> = None;
    for (lc, lp, pb) in [(3, 0, 2), (3, 0, 0), (0, 0, 0)] {
        let mut opts = lzma_options(data.len(), dict_cap);
        opts.lc = lc;
        opts.lp = lp;
        opts.pb = pb;
        let mut w = LzmaWriter::new_use_header(Vec::new(), &opts, Some(data.len() as u64))?;
        w.write_all(data)?;
        let out = w.finish()?;
        crate::progress::add(data.len() as u64 * crate::progress::W_LZMA);
        if best.as_ref().is_none_or(|b| out.len() < b.len()) {
            best = Some(out);
        }
    }
    Ok(best.unwrap())
}

fn lzma_decompress(comp: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    let mut r = LzmaReader::new_mem_limit(comp, u32::MAX, None)?;
    let mut out = Vec::with_capacity(raw_len);
    r.read_to_end(&mut out)?;
    Ok(out)
}

fn zstd_compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 22)?;
    enc.set_parameter(zstd::zstd_safe::CParameter::EnableLongDistanceMatching(
        true,
    ))?;
    enc.set_parameter(zstd::zstd_safe::CParameter::WindowLog(27))?;
    enc.write_all(data)?;
    let out = enc.finish()?;
    crate::progress::add(data.len() as u64 * crate::progress::W_ZSTD);
    Ok(out)
}

fn zstd_decompress(comp: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    let mut dec = zstd::stream::read::Decoder::new(comp)?;
    dec.window_log_max(31)?;
    let mut out = Vec::with_capacity(raw_len);
    dec.read_to_end(&mut out)?;
    Ok(out)
}

fn brotli_compress(data: &[u8]) -> Result<Vec<u8>> {
    let params = brotli::enc::BrotliEncoderParams {
        quality: 11,
        lgwin: 24,
        size_hint: data.len(),
        ..Default::default()
    };
    let mut out = Vec::new();
    brotli::BrotliCompress(&mut std::io::Cursor::new(data), &mut out, &params)?;
    crate::progress::add(data.len() as u64 * crate::progress::W_BROTLI);
    Ok(out)
}

fn brotli_decompress(comp: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(raw_len);
    brotli::BrotliDecompress(&mut std::io::Cursor::new(comp), &mut out)?;
    Ok(out)
}

/// Compress with one backend id.
fn compress_one_hint(
    id: u8,
    data: &[u8],
    stride: u32,
    bpp: u32,
    mem: MemBudget,
) -> Result<Vec<u8>> {
    match id {
        STORE => Ok(data.to_vec()),
        ZSTD => zstd_compress(data),
        BROTLI => brotli_compress(data),
        LZMA => lzma_compress(data, mem.lzma_dict_cap()),
        CM => crate::cm::compress_with_stride(data, stride, bpp, mem.cm_cap()),
        _ => bail!("unknown backend {}", id),
    }
}

/// Exposed for the dev `raw` benchmarking subcommand.
pub fn compress_one_public_hint(id: u8, data: &[u8], stride: u32, bpp: u32) -> Result<Vec<u8>> {
    compress_one_hint(id, data, stride, bpp, MemBudget::UNLIMITED)
}

pub fn decompress(id: u8, comp: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    let out = match id {
        STORE => comp.to_vec(),
        ZSTD => zstd_decompress(comp, raw_len)?,
        BROTLI => brotli_decompress(comp, raw_len)?,
        LZMA => lzma_decompress(comp, raw_len)?,
        CM => crate::cm::decompress(comp, raw_len)?, // reports its own progress
        _ => bail!("unknown backend {}", id),
    };
    if id != CM {
        crate::progress::add(raw_len as u64 * crate::progress::W_FAST_D);
    }
    if out.len() != raw_len {
        bail!(
            "backend {} produced {} bytes, expected {}",
            name(id),
            out.len(),
            raw_len
        );
    }
    Ok(out)
}

/// Race all candidate backends; return the smallest output whose round-trip
/// verifies. Store is the floor, so channels never meaningfully expand.
pub fn compress_best(data: &[u8], use_cm: bool) -> (u8, Vec<u8>) {
    compress_best_hint(data, use_cm, 0, 0, MemBudget::UNLIMITED)
}

/// Like `compress_best`, with an image stride/bpp hint for the CM backend
/// and a per-job memory budget.
pub fn compress_best_hint(
    data: &[u8],
    use_cm: bool,
    stride: u32,
    bpp: u32,
    mem: MemBudget,
) -> (u8, Vec<u8>) {
    if data.is_empty() {
        return (STORE, Vec::new());
    }
    let mut ids = vec![ZSTD, BROTLI, LZMA];
    if use_cm {
        ids.push(CM);
    }
    let best = ids
        .into_par_iter()
        .filter_map(|id| {
            let comp = compress_one_hint(id, data, stride, bpp, mem).ok()?;
            // Trust nothing: verify the round trip before accepting.
            let back = decompress(id, &comp, data.len()).ok()?;
            if back == data { Some((id, comp)) } else { None }
        })
        .min_by_key(|(_, c)| c.len());
    match best {
        Some((id, comp)) if comp.len() < data.len() => (id, comp),
        _ => (STORE, data.to_vec()),
    }
}
