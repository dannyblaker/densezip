//! The .dnz archive container: pack, extract, verify, list.
//!
//! Layout:
//! ```text
//! [8B magic "DNZA" v1]
//! [channel payloads in order: plain, corrections, filters, lepton, pixel blobs...]
//! [zstd-compressed TOC]
//! [u64 toc_offset][u64 toc_comp_len][8B magic "DNZENDv1"]
//! ```

use crate::backends;
use crate::channels::{Channels, Cursors, PixelBlob};
use crate::plan::{read_segs, remap_blobs, write_segs, Seg};
use crate::rebuild::render_segs;
use crate::scan::plan_bytes;
use crate::util::{human, write_varint, xxh3, Reader};
use anyhow::{bail, ensure, Context, Result};
use rayon::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"DNZA\x01\x00\x00\x00";
const END_MAGIC: &[u8; 8] = b"DNZENDv1";

pub struct Entry {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub hash: u64,
    pub segs: Vec<Seg>,
}

pub struct Archive {
    pub entries: Vec<Entry>,
    pub channels: Channels,
    /// (backend, comp_len, raw_len) per channel, for reporting.
    pub channel_info: Vec<(u8, u64, u64)>,
}

struct PlannedFile {
    path: String,
    is_dir: bool,
    size: u64,
    hash: u64,
    segs: Vec<Seg>,
    delta: Channels,
}

/// Collect (absolute path, archive path) pairs from CLI inputs.
fn collect_inputs(inputs: &[PathBuf]) -> Result<Vec<(PathBuf, String, bool)>> {
    let mut out: Vec<(PathBuf, String, bool)> = Vec::new();
    for input in inputs {
        let input = input
            .canonicalize()
            .with_context(|| format!("cannot access {}", input.display()))?;
        let base = input.parent().map(Path::to_path_buf).unwrap_or_else(|| input.clone());
        let mut stack = vec![input.clone()];
        while let Some(p) = stack.pop() {
            let rel = p
                .strip_prefix(&base)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            let meta = fs::symlink_metadata(&p)?;
            if meta.is_dir() {
                out.push((p.clone(), rel, true));
                let mut children: Vec<_> =
                    fs::read_dir(&p)?.collect::<std::io::Result<Vec<_>>>()?;
                children.sort_by_key(|e| e.file_name());
                for c in children {
                    stack.push(c.path());
                }
            } else if meta.is_file() {
                out.push((p, rel, false));
            }
            // symlinks and special files are skipped in v1
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out.dedup_by(|a, b| a.1 == b.1);
    Ok(out)
}

fn plan_file(abs: &Path, rel: &str, is_dir: bool) -> Result<PlannedFile> {
    if is_dir {
        return Ok(PlannedFile {
            path: rel.to_string(),
            is_dir: true,
            size: 0,
            hash: 0,
            segs: Vec::new(),
            delta: Channels::default(),
        });
    }
    let data = fs::read(abs).with_context(|| format!("reading {}", abs.display()))?;
    let hash = xxh3(&data);
    let mut delta = Channels::default();
    let mut segs = plan_bytes(&data, 0, &mut delta);

    // The sacred invariant: render the plan and compare. On any mismatch,
    // fall back to storing the file as a single literal.
    let ok = {
        let mut cur = Cursors::new(&delta);
        let mut rendered = Vec::with_capacity(data.len());
        render_segs(&segs, &mut cur, &mut rendered).is_ok() && rendered == data
    };
    if !ok {
        eprintln!("warning: transform verification failed for {}, storing raw", rel);
        delta = Channels::default();
        delta.plain.extend_from_slice(&data);
        segs = vec![Seg::Raw { len: data.len() as u64 }];
    }
    Ok(PlannedFile { path: rel.to_string(), is_dir: false, size: data.len() as u64, hash, segs, delta })
}

pub fn pack(
    archive_path: &Path,
    inputs: &[PathBuf],
    use_cm: bool,
    verify: bool,
    mem_gib: Option<f64>,
) -> Result<()> {
    let files = collect_inputs(inputs)?;
    ensure!(!files.is_empty(), "no input files");

    let budget_bytes = match mem_gib {
        Some(g) => {
            ensure!(g > 0.0, "--mem must be positive");
            (g * (1u64 << 30) as f64) as u64
        }
        // auto: leave headroom for the OS and other processes
        None => crate::util::available_ram_bytes() * 3 / 4,
    };

    // Plan all files in parallel.
    let planned: Vec<PlannedFile> = files
        .par_iter()
        .map(|(abs, rel, is_dir)| plan_file(abs, rel, *is_dir))
        .collect::<Result<Vec<_>>>()?;

    // Commit into global channels in deterministic order.
    let mut ch = Channels::default();
    let mut entries: Vec<Entry> = Vec::new();
    for mut pf in planned {
        let blob_offset = ch.pixel_blobs.len() as u32;
        remap_blobs(&mut pf.segs, blob_offset);
        ch.plain.extend_from_slice(&pf.delta.plain);
        ch.corrections.extend_from_slice(&pf.delta.corrections);
        ch.filters.extend_from_slice(&pf.delta.filters);
        ch.lepton.extend_from_slice(&pf.delta.lepton);
        ch.pixel_blobs.append(&mut pf.delta.pixel_blobs);
        entries.push(Entry {
            path: pf.path,
            is_dir: pf.is_dir,
            size: pf.size,
            hash: pf.hash,
            segs: pf.segs,
        });
    }

    // Compress all channels in parallel; each picks its best backend, and
    // pixel blobs additionally race color-decorrelation transforms.
    // (data, use_cm, stride, bpp, is_pixels)
    let mut jobs: Vec<(&[u8], bool, u32, u32, bool)> = vec![
        (ch.plain.as_slice(), use_cm, 0, 0, false),
        (ch.corrections.as_slice(), false, 0, 0, false), // high-entropy cabac data
        (ch.filters.as_slice(), use_cm, 0, 0, false),
        (ch.lepton.as_slice(), false, 0, 0, false), // already arithmetic-coded
    ];
    for b in &ch.pixel_blobs {
        jobs.push((b.data.as_slice(), use_cm, b.stride, b.bpp, true));
    }
    // Memory budget: channels are held in RAM while compressing, so what's
    // left funds the compressors. Split it across a bounded number of
    // concurrently running jobs (~4 GiB each when RAM allows).
    let held: u64 = jobs.iter().map(|(d, ..)| d.len() as u64).sum();
    let usable = budget_bytes.saturating_sub(held * 2).max(budget_bytes / 4);
    let concurrent = ((usable >> 32).max(1) as usize).min(jobs.len().max(1));
    let per_job = backends::MemBudget { bytes: usable / concurrent as u64 };
    eprintln!(
        "memory budget: {} ({}) -> {} concurrent job(s), {} each",
        human(budget_bytes),
        if mem_gib.is_some() { "--mem" } else { "auto-detected" },
        concurrent,
        human(per_job.bytes)
    );

    // (backend, payload, pixel transform)
    let mut payloads: Vec<(u8, Vec<u8>, u8)> = Vec::with_capacity(jobs.len());
    for batch in jobs.chunks(concurrent) {
        payloads.extend(batch.par_iter().map(|(data, cm, stride, bpp, is_pixels)| {
            let (id, comp) = backends::compress_best_hint(data, *cm, *stride, *bpp, per_job);
            if *is_pixels && (*bpp == 3 || *bpp == 4) {
                let alt = crate::channels::sub_green(data, *bpp as usize, true);
                let (id2, comp2) = backends::compress_best_hint(&alt, *cm, *stride, *bpp, per_job);
                if comp2.len() < comp.len() {
                    return (id2, comp2, crate::channels::PIXEL_SUB_GREEN);
                }
            }
            (id, comp, crate::channels::PIXEL_IDENTITY)
        }).collect::<Vec<_>>());
    }

    // Serialize TOC.
    let mut toc = Vec::new();
    write_varint(&mut toc, entries.len() as u64);
    for e in &entries {
        toc.push(if e.is_dir { 1 } else { 0 });
        write_varint(&mut toc, e.path.len() as u64);
        toc.extend_from_slice(e.path.as_bytes());
        if !e.is_dir {
            write_varint(&mut toc, e.size);
            toc.extend_from_slice(&e.hash.to_le_bytes());
            write_segs(&mut toc, &e.segs);
        }
    }
    write_varint(&mut toc, ch.pixel_blobs.len() as u64);
    for (i, b) in ch.pixel_blobs.iter().enumerate() {
        write_varint(&mut toc, b.stride as u64);
        write_varint(&mut toc, b.bpp as u64);
        toc.push(payloads[4 + i].2); // pixel transform
    }
    let raw_lens: Vec<u64> = {
        let mut v = vec![
            ch.plain.len() as u64,
            ch.corrections.len() as u64,
            ch.filters.len() as u64,
            ch.lepton.len() as u64,
        ];
        v.extend(ch.pixel_blobs.iter().map(|b| b.data.len() as u64));
        v
    };
    for (i, (backend, payload, _)) in payloads.iter().enumerate() {
        toc.push(*backend);
        write_varint(&mut toc, payload.len() as u64);
        write_varint(&mut toc, raw_lens[i]);
    }
    let toc_comp = zstd::bulk::compress(&toc, 19)?;

    // Assemble the archive.
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    for (_, payload, _) in &payloads {
        out.extend_from_slice(payload);
    }
    let toc_offset = out.len() as u64;
    out.extend_from_slice(&toc_comp);
    out.extend_from_slice(&toc_offset.to_le_bytes());
    out.extend_from_slice(&(toc_comp.len() as u64).to_le_bytes());
    out.extend_from_slice(END_MAGIC);
    fs::write(archive_path, &out)
        .with_context(|| format!("writing {}", archive_path.display()))?;

    let total_in: u64 = entries.iter().map(|e| e.size).sum();
    println!(
        "{} file(s), {} -> {} ({:.2}%)",
        entries.iter().filter(|e| !e.is_dir).count(),
        human(total_in),
        human(out.len() as u64),
        out.len() as f64 * 100.0 / total_in.max(1) as f64
    );
    for (i, (backend, payload, _)) in payloads.iter().enumerate() {
        let label = match i {
            0 => "plain".to_string(),
            1 => "corrections".to_string(),
            2 => "filters".to_string(),
            3 => "lepton".to_string(),
            n => format!("pixels#{}", n - 4),
        };
        if raw_lens[i] > 0 {
            println!(
                "  channel {:<12} {:>12} -> {:>12}  [{}]",
                label,
                human(raw_lens[i]),
                human(payload.len() as u64),
                backends::name(*backend)
            );
        }
    }

    if verify {
        let archive = read_archive(archive_path)?;
        let mut cur = Cursors::new(&archive.channels);
        for e in &archive.entries {
            if e.is_dir {
                continue;
            }
            let mut rendered = Vec::with_capacity(e.size as usize);
            render_segs(&e.segs, &mut cur, &mut rendered)
                .with_context(|| format!("verify: render failed for {}", e.path))?;
            ensure!(
                rendered.len() as u64 == e.size && xxh3(&rendered) == e.hash,
                "verify: content mismatch for {}",
                e.path
            );
        }
        println!("verified: all files reconstruct bit-exactly");
    }
    Ok(())
}

pub fn read_archive(path: &Path) -> Result<Archive> {
    let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    ensure!(data.len() >= 32 && &data[..8] == MAGIC, "not a densezip archive");
    ensure!(&data[data.len() - 8..] == END_MAGIC, "archive trailer missing/corrupt");
    let toc_offset =
        u64::from_le_bytes(data[data.len() - 24..data.len() - 16].try_into().unwrap()) as usize;
    let toc_comp_len =
        u64::from_le_bytes(data[data.len() - 16..data.len() - 8].try_into().unwrap()) as usize;
    ensure!(toc_offset + toc_comp_len <= data.len() - 24, "bad TOC location");
    let toc = zstd::stream::decode_all(&data[toc_offset..toc_offset + toc_comp_len])?;

    let mut r = Reader::new(&toc);
    let n_entries = r.varint()? as usize;
    ensure!(n_entries <= 1 << 24, "implausible entry count");
    let mut entries = Vec::with_capacity(n_entries.min(1024));
    for _ in 0..n_entries {
        let flags = r.byte()?;
        let is_dir = flags & 1 != 0;
        let plen = r.varint()? as usize;
        let path = String::from_utf8(r.bytes(plen)?.to_vec()).context("bad path encoding")?;
        if is_dir {
            entries.push(Entry { path, is_dir, size: 0, hash: 0, segs: Vec::new() });
        } else {
            let size = r.varint()?;
            let hash = u64::from_le_bytes(r.bytes(8)?.try_into().unwrap());
            let segs = read_segs(&mut r)?;
            entries.push(Entry { path, is_dir, size, hash, segs });
        }
    }
    let n_blobs = r.varint()? as usize;
    ensure!(n_blobs <= 1 << 24, "implausible blob count");
    let mut blob_meta = Vec::with_capacity(n_blobs.min(1024));
    for _ in 0..n_blobs {
        let stride = r.varint()? as u32;
        let bpp = r.varint()? as u32;
        let transform = r.byte()?;
        blob_meta.push((stride, bpp, transform));
    }
    let n_channels = 4 + n_blobs;
    let mut infos = Vec::with_capacity(n_channels);
    for _ in 0..n_channels {
        let backend = r.byte()?;
        let comp_len = r.varint()?;
        let raw_len = r.varint()?;
        infos.push((backend, comp_len, raw_len));
    }

    // Decompress channel payloads (in parallel).
    let mut offsets = Vec::with_capacity(n_channels);
    let mut off = 8usize;
    for &(_, comp_len, _) in &infos {
        offsets.push(off);
        off += comp_len as usize;
    }
    ensure!(off <= toc_offset, "channel payloads overrun TOC");
    let raws: Vec<Vec<u8>> = infos
        .par_iter()
        .zip(offsets.par_iter())
        .map(|(&(backend, comp_len, raw_len), &o)| {
            backends::decompress(backend, &data[o..o + comp_len as usize], raw_len as usize)
        })
        .collect::<Result<Vec<_>>>()?;

    let mut it = raws.into_iter();
    let channels = Channels {
        plain: it.next().unwrap(),
        corrections: it.next().unwrap(),
        filters: it.next().unwrap(),
        lepton: it.next().unwrap(),
        pixel_blobs: it
            .zip(blob_meta)
            .map(|(data, (stride, bpp, transform))| {
                let data = match transform {
                    crate::channels::PIXEL_SUB_GREEN => {
                        crate::channels::sub_green(&data, bpp as usize, false)
                    }
                    _ => data,
                };
                PixelBlob { data, stride, bpp }
            })
            .collect(),
    };
    Ok(Archive { entries, channels, channel_info: infos })
}

fn safe_join(out_dir: &Path, rel: &str) -> Result<PathBuf> {
    let p = Path::new(rel);
    ensure!(
        !p.is_absolute()
            && !p.components().any(|c| matches!(c, std::path::Component::ParentDir)),
        "unsafe path in archive: {}",
        rel
    );
    Ok(out_dir.join(p))
}

pub fn extract(archive_path: &Path, out_dir: &Path, overwrite: bool) -> Result<()> {
    let archive = read_archive(archive_path)?;
    let mut cur = Cursors::new(&archive.channels);
    for e in &archive.entries {
        let dest = safe_join(out_dir, &e.path)?;
        if e.is_dir {
            fs::create_dir_all(&dest)?;
            continue;
        }
        let mut rendered = Vec::with_capacity(e.size as usize);
        render_segs(&e.segs, &mut cur, &mut rendered)
            .with_context(|| format!("rendering {}", e.path))?;
        ensure!(
            rendered.len() as u64 == e.size && xxh3(&rendered) == e.hash,
            "content mismatch for {} (archive corrupt?)",
            e.path
        );
        if dest.exists() && !overwrite {
            bail!("{} exists (use --overwrite)", dest.display());
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, &rendered)?;
        println!("{} ({})", e.path, human(e.size));
    }
    Ok(())
}

pub fn test(archive_path: &Path) -> Result<()> {
    let archive = read_archive(archive_path)?;
    let mut cur = Cursors::new(&archive.channels);
    let mut n = 0u64;
    let mut total = 0u64;
    for e in &archive.entries {
        if e.is_dir {
            continue;
        }
        let mut rendered = Vec::with_capacity(e.size as usize);
        render_segs(&e.segs, &mut cur, &mut rendered)
            .with_context(|| format!("rendering {}", e.path))?;
        ensure!(
            rendered.len() as u64 == e.size && xxh3(&rendered) == e.hash,
            "FAILED: {}",
            e.path
        );
        n += 1;
        total += e.size;
    }
    println!("OK: {} file(s), {} verified bit-exact", n, human(total));
    Ok(())
}

pub fn list(archive_path: &Path) -> Result<()> {
    let archive = read_archive(archive_path)?;
    for e in &archive.entries {
        if e.is_dir {
            println!("{:>12}  {}/", "dir", e.path);
        } else {
            println!("{:>12}  {}", human(e.size), e.path);
        }
    }
    Ok(())
}
