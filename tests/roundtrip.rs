//! The sacred invariant: pack then extract must reproduce every input
//! byte-for-byte, for every kind of input.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

fn densezip() -> &'static str {
    env!("CARGO_BIN_EXE_dnz")
}

/// Pack `files`, extract, and compare every file byte-for-byte.
fn roundtrip(dir: &Path, names: &[&str]) {
    let arch = dir.join("t.dnz");
    let mut cmd = Command::new(densezip());
    cmd.arg("a").arg(&arch);
    for n in names {
        cmd.arg(dir.join(n));
    }
    // CM backend is exercised in its own unit tests; keep integration fast.
    cmd.arg("--no-cm");
    let out = cmd.output().expect("run densezip a");
    assert!(
        out.status.success(),
        "pack failed: {}\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );

    let xdir = dir.join("extracted");
    fs::create_dir_all(&xdir).unwrap();
    let out = Command::new(densezip())
        .args(["x"])
        .arg(&arch)
        .arg("-o")
        .arg(&xdir)
        .arg("--overwrite")
        .output()
        .expect("run densezip x");
    assert!(out.status.success(), "extract failed: {}", String::from_utf8_lossy(&out.stderr));

    for n in names {
        let orig = fs::read(dir.join(n)).unwrap();
        let back = fs::read(xdir.join(n)).unwrap();
        assert_eq!(orig.len(), back.len(), "length mismatch for {}", n);
        assert!(orig == back, "content mismatch for {}", n);
    }
}

fn deterministic_bytes(len: usize, seed: u64) -> Vec<u8> {
    // xorshift; avoids a rand dependency
    let mut x = seed.wrapping_mul(0x9e3779b97f4a7c15) | 1;
    let mut v = Vec::with_capacity(len);
    while v.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(len);
    v
}

#[test]
fn empty_and_tiny_files() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("empty"), b"").unwrap();
    fs::write(dir.path().join("one"), b"x").unwrap();
    fs::write(dir.path().join("small"), b"hello world").unwrap();
    roundtrip(dir.path(), &["empty", "one", "small"]);
}

#[test]
fn random_data_does_not_break() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("rand.bin"), deterministic_bytes(1 << 20, 42)).unwrap();
    roundtrip(dir.path(), &["rand.bin"]);
}

#[test]
fn text_file() {
    let dir = tempfile::tempdir().unwrap();
    let text = "the quick brown fox jumps over the lazy dog\n".repeat(20_000);
    fs::write(dir.path().join("text.txt"), &text).unwrap();
    roundtrip(dir.path(), &["text.txt"]);
}

#[test]
fn gzip_member_recompression() {
    let dir = tempfile::tempdir().unwrap();
    let payload = "some very compressible payload ".repeat(10_000);
    let mut enc =
        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(payload.as_bytes()).unwrap();
    fs::write(dir.path().join("file.gz"), enc.finish().unwrap()).unwrap();
    roundtrip(dir.path(), &["file.gz"]);
}

#[test]
fn zlib_stream_recompression() {
    let dir = tempfile::tempdir().unwrap();
    let payload = deterministic_bytes(100_000, 7)
        .iter()
        .map(|b| b % 16) // compressible-ish
        .collect::<Vec<u8>>();
    let mut enc =
        flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&payload).unwrap();
    let z = enc.finish().unwrap();
    // Embed the zlib stream mid-file, surrounded by other bytes (PDF-style).
    let mut f = b"HEADER JUNK ".to_vec();
    f.extend_from_slice(&z);
    f.extend_from_slice(b" TRAILING JUNK");
    fs::write(dir.path().join("embedded.bin"), &f).unwrap();
    roundtrip(dir.path(), &["embedded.bin"]);
}

/// Build a minimal valid PNG by hand (grayscale, deterministic content).
fn synth_png(w: u32, h: u32) -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(ty);
        out.extend_from_slice(data);
        let crc = crc32fast::hash(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, 0]); // 8-bit grayscale, no interlace
    let mut scanlines = Vec::new();
    for y in 0..h {
        scanlines.push(0u8); // filter: none
        for x in 0..w {
            scanlines.push(((x * 7 + y * 13) % 251) as u8);
        }
    }
    let mut enc =
        flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&scanlines).unwrap();
    let idat = enc.finish().unwrap();

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &ihdr);
    // split IDAT into two chunks to exercise chunk reassembly
    let mid = idat.len() / 2;
    chunk(&mut png, b"IDAT", &idat[..mid]);
    chunk(&mut png, b"IDAT", &idat[mid..]);
    chunk(&mut png, b"IEND", b"");
    png
}

#[test]
fn png_recompression() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("img.png"), synth_png(200, 100)).unwrap();
    roundtrip(dir.path(), &["img.png"]);
}

#[test]
fn directory_tree() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tree");
    fs::create_dir_all(root.join("sub/deep")).unwrap();
    fs::write(root.join("a.txt"), "aaa".repeat(1000)).unwrap();
    fs::write(root.join("sub/b.bin"), deterministic_bytes(5000, 3)).unwrap();
    fs::write(root.join("sub/deep/c"), b"c").unwrap();

    let arch = dir.path().join("tree.dnz");
    let out = Command::new(densezip())
        .arg("a")
        .arg(&arch)
        .arg(&root)
        .arg("--no-cm")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let xdir = dir.path().join("x");
    let out = Command::new(densezip())
        .arg("x")
        .arg(&arch)
        .arg("-o")
        .arg(&xdir)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    for rel in ["tree/a.txt", "tree/sub/b.bin", "tree/sub/deep/c"] {
        let orig = fs::read(dir.path().join(rel)).unwrap();
        let back = fs::read(xdir.join(rel)).unwrap();
        assert!(orig == back, "mismatch: {}", rel);
    }
}

#[test]
fn archive_test_command() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("f"), deterministic_bytes(10_000, 9)).unwrap();
    let arch = dir.path().join("f.dnz");
    assert!(Command::new(densezip())
        .arg("a")
        .arg(&arch)
        .arg(dir.path().join("f"))
        .arg("--no-cm")
        .output()
        .unwrap()
        .status
        .success());
    let out = Command::new(densezip()).arg("t").arg(&arch).output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("OK"));
}

#[test]
fn unicode_filename() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("héllo wörld — 你好.txt"), "unicode content".repeat(100)).unwrap();
    roundtrip(dir.path(), &["héllo wörld — 你好.txt"]);
}

#[test]
fn many_small_files_solid() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("many");
    fs::create_dir_all(&root).unwrap();
    for i in 0..200 {
        fs::write(root.join(format!("f{:03}.txt", i)), format!("shared prefix content {}\n", i).repeat(20)).unwrap();
    }
    let arch = dir.path().join("many.dnz");
    let out = Command::new(densezip()).arg("a").arg(&arch).arg(&root).arg("--no-cm").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let xdir = dir.path().join("x");
    assert!(Command::new(densezip()).arg("x").arg(&arch).arg("-o").arg(&xdir).output().unwrap().status.success());
    for i in 0..200 {
        let rel = format!("many/f{:03}.txt", i);
        assert_eq!(fs::read(dir.path().join(&rel)).unwrap(), fs::read(xdir.join(&rel)).unwrap());
    }
}

#[test]
fn compressing_own_archive() {
    // an archive of an archive: high-entropy input, must round-trip and not blow up
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("data.txt"), "text ".repeat(10_000)).unwrap();
    let a1 = dir.path().join("inner.dnz");
    assert!(Command::new(densezip()).arg("a").arg(&a1).arg(dir.path().join("data.txt")).arg("--no-cm").output().unwrap().status.success());
    let inner = fs::read(&a1).unwrap();
    fs::write(dir.path().join("inner.dnz.copy"), &inner).unwrap();
    roundtrip(dir.path(), &["inner.dnz.copy"]);
}

#[test]
fn truncated_archive_fails_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("f.txt"), "hello ".repeat(5000)).unwrap();
    let arch = dir.path().join("t.dnz");
    assert!(Command::new(densezip()).arg("a").arg(&arch).arg(dir.path().join("f.txt")).arg("--no-cm").output().unwrap().status.success());
    let data = fs::read(&arch).unwrap();
    for cut in [data.len() / 2, data.len() - 5, 10] {
        fs::write(&arch, &data[..cut]).unwrap();
        let out = Command::new(densezip()).arg("t").arg(&arch).output().unwrap();
        assert!(!out.status.success(), "truncated archive (cut={}) must fail", cut);
    }
}

#[test]
fn corrupted_archive_fails_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("f.txt"), "hello ".repeat(5000)).unwrap();
    let arch = dir.path().join("t.dnz");
    assert!(Command::new(densezip()).arg("a").arg(&arch).arg(dir.path().join("f.txt")).arg("--no-cm").output().unwrap().status.success());
    let mut data = fs::read(&arch).unwrap();
    let mid = data.len() / 2;
    data[mid] ^= 0xff; // flip bits in the middle (payload or TOC)
    fs::write(&arch, &data).unwrap();
    let out = Command::new(densezip()).arg("t").arg(&arch).output().unwrap();
    assert!(!out.status.success(), "corrupted archive must fail verification");
}

#[test]
fn memory_budget_mode() {
    // pack with a 1 GiB budget: must still round-trip, just with smaller models
    let dir = tempfile::tempdir().unwrap();
    let mut data = Vec::new();
    for i in 0..400_000u32 {
        data.extend_from_slice(format!("row-{},value-{}\n", i % 977, i % 31).as_bytes());
    }
    fs::write(dir.path().join("big.txt"), &data).unwrap();
    let arch = dir.path().join("b.dnz");
    let out = Command::new(densezip())
        .arg("a")
        .arg(&arch)
        .arg(dir.path().join("big.txt"))
        .args(["--mem", "1"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("memory budget"));
    let xdir = dir.path().join("x");
    assert!(Command::new(densezip()).arg("x").arg(&arch).arg("-o").arg(&xdir).output().unwrap().status.success());
    assert_eq!(fs::read(dir.path().join("big.txt")).unwrap(), fs::read(xdir.join("big.txt")).unwrap());
}
