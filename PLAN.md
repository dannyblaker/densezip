# densezip — Plan

> **Status (2026-07-12): built.** All milestones M0–M5 are complete; see
> `README.md` for usage and `BENCHMARKS.md` for the final measured results.
> Gate outcomes: M1 beat 7z on both samples (PNG −63%, PDF −33% at final);
> M2 dzcm beats xz -9e on all tested Silesia files by 13–24%; M3 record/2D
> models put dzcm ahead of zpaq -m5 on `sao`; M4 racing/solid/multi-file
> shipped; M5 added 13 integration tests incl. corruption/truncation handling.
> Note: the original `sample_files/` were later replaced with synthetic
> equivalents (random-colour PNG, "Et Lorem" PDF); the sample measurements
> in this document describe the original files and are kept for history.

**Goal:** a Rust archiver whose output is smaller than `gzip -9`, `7z -mx=9`, and `xz -9e` on real-world files. Speed and memory are explicitly *not* goals (we have 64 threads / 125 GB RAM to spend).

---

## 1. Research findings (validated by experiment on this machine)

### 1.1 The samples are already-compressed data — and that's the big opportunity

`sample.pdf` and `sample.png` both contain **deflate streams inside** (PDF `FlateDecode` objects, PNG `IDAT`). Generic compressors can't do much with already-compressed bytes — that's why 7z only shaves ~7% off them. The winning move is **recompression**: losslessly undo the internal deflate, compress the *raw* content with something far stronger, and store a tiny correction record so the original file is reconstructed **bit-exactly**.

Measured on your samples (best competitor = min of gzip/bzip2/xz/zstd‑22/7z):

| File | Original | Best competitor | Experiment | Result |
|---|---|---|---|---|
| sample.png | 461,035 | 428,409 (7z) | un-deflate IDAT → unfilter to raw pixels → xz | **146,612 (−66%)** |
| sample.pdf | 547,598 | 460,137 (zstd) | un-deflate FlateDecode streams (434,561 B) → zpaq | streams: 434,561 → **274,773**; est. whole file ≈ **390 KB (−15%)** |

The PNG number will improve further with a 2D image model; the PDF further with JPEG recompression of its `DCTDecode` objects.

### 1.2 Context mixing (CM) beats LZMA on general data by 12–27%

The strongest known lossless compressors (PAQ family, cmix, zpaq — top of the [Large Text Compression Benchmark](https://mattmahoney.net/dc/text.html)) use context mixing: many probability models voting per **bit**, blended by a learned mixer, fed to an arithmetic coder. They are slow — which we don't care about. Measured on the Silesia corpus:

| File (type) | gzip -9 | xz -9e | zpaq -m5 (CM) | CM vs xz |
|---|---|---|---|---|
| dickens (English text) | 3,851,823 | 2,831,212 | **2,094,756** | −26% |
| xml (structured text) | 662,284 | 434,892 | **326,956** | −25% |
| ooffice (x86 binary) | 3,090,442 | 2,427,224 | **1,766,563** | −27% |
| sao (astronomical, high-entropy) | 5,327,041 | 4,425,664 | **3,899,267** | −12% |

One caveat found: on the PNG's raw pixels, xz (146 KB) beat zpaq -m5 (160 KB) — no single backend wins everywhere. Since speed is free, **densezip will try multiple backends per stream and keep the smallest**.

### 1.3 Key building blocks already exist as production Rust crates

- [**preflate-rs**](https://github.com/microsoft/preflate-rs) (Microsoft, Apache-2.0) — losslessly splits any deflate stream into raw data + a small correction record (~0.01–0.1% overhead for zlib-produced streams), and *scans containers (zip, png, pdf, docx…) for embedded deflate automatically*. Built for cloud storage, i.e. bit-exact guarantees.
- [**lepton_jpeg**](https://github.com/microsoft/lepton_jpeg_rust) (Microsoft port of Dropbox Lepton, Apache-2.0) — lossless JPEG recompression, ~22% smaller, bit-exact recovery. Covers `.jpg` files *and* JPEGs embedded in PDFs.
- `lzma-rust2`, `brotli`, `zstd` crates — mature backends for the "try them all" stage.

The one thing that does **not** exist as a good Rust crate is a PAQ-class CM compressor. **That is the core thing we build.**

---

## 2. Architecture

Three-layer pipeline; every layer is perfectly reversible and records what it did in the archive metadata.

```
input files
   │
   ▼
┌─────────────────────────────────────────────────────────┐
│ L1  RECOMPRESSION  (undo prior compression, bit-exact)  │
│  • deflate streams anywhere (preflate-rs): zip, gz,     │
│    png IDAT, pdf FlateDecode, docx/xlsx, jar…           │
│  • JPEG (lepton_jpeg): standalone + inside PDF          │
│  → emits raw substreams + correction records            │
├─────────────────────────────────────────────────────────┤
│ L2  CONTENT TRANSFORMS  (reshape raw data to compress   │
│     better)                                             │
│  • PNG: unfilter scanlines → raw pixels (measured 2×    │
│    better than compressing filtered data)               │
│  • type detection & grouping: put similar streams       │
│    together (solid mode) so the model learns once       │
│  • later: x86 call-transform (E8/E9), numeric delta     │
├─────────────────────────────────────────────────────────┤
│ L3  BACKENDS — compress each stream with ALL of:        │
│  • dzcm  — our own CM engine (the centerpiece)          │
│  • LZMA (xz-level, via lzma-rust2)                      │
│  • brotli -q11, zstd -22 (cheap extra candidates)       │
│  → keep the smallest; 64 threads make this nearly free  │
└─────────────────────────────────────────────────────────┘
   │
   ▼
.dnz archive  =  header + per-stream records (backend id,
                transform chain, correction data) + TOC
```

Decompression walks the layers backwards; L1 correction records guarantee the output is byte-identical to the input (enforced by tests, see §5).

### 2.1 The CM engine (`dzcm`) — what we actually build

Bit-level arithmetic coder + mixed predictors, paq8-lite in architecture, pure safe Rust:

1. **Models** (each outputs P(next bit = 1), keyed by a hashed context):
   - order-0…6 byte contexts (hash tables, budget ~1–4 GB — we have RAM)
   - **match model** — finds the longest previous occurrence of the current context, predicts its next bit (this is what makes CM crush LZ on text)
   - word model (whitespace-delimited contexts) for text/XML
   - sparse contexts (skip-grams) for binary/records
   - 2D image model (pixel above / left / above-left as context) for L2 pixel streams — this is how we get the PNG well below xz's 146 KB
2. **Mixer** — logistic mixing (online-trained neural layer, weights selected by context), as in paq8/zpaq -m5.
3. **SSE/APM stages** — refine the mixed probability against recent outcomes.
4. **Coder** — 32-bit binary arithmetic coder (well-understood, ~50 lines).

Target: **≥ zpaq -m5** on Silesia (i.e. 12–27% under xz). This is achievable: zpaq -m5's model set is documented and modest; we can afford bigger contexts and more models since we're not bound by its 850 MB-era budgets.

**Fallback if dzcm misses the target** (go/no-go at M2): temporarily link `libzpaq` (C++, MIT) via FFI as the strong backend so the product still wins, and keep improving dzcm behind the trial-all selector. The archive format doesn't care which backend won.

---

## 3. Archive format (v1 sketch)

```
magic "BZ01" | format version | flags
stream table: for each stream —
    source (file id, byte range) | transform chain (L1/L2 ops + correction blob) |
    backend id | compressed size | raw size | checksum (xxh3)
file table: path, mode, mtime, list of stream ids
trailer: TOC offset, whole-archive checksum
```

- Solid compression: streams of the same class (text-like, pixels, binary) are concatenated per group before hitting L3, so one model context serves many files.
- Every stream carries a checksum; `dnz t` verifies, `dnz x` restores bit-exact originals.

## 4. CLI

```
dnz a archive.dnz files...     # create   (default: maximum effort)
dnz x archive.dnz [-o dir]     # extract
dnz t archive.dnz              # test/verify
dnz bench <corpus dir>        # run the benchmark harness vs gzip/7z/xz/zstd
--effort 1..9                       # 9 = try every backend (default), lower prunes
```

## 5. Testing & benchmarking

- **Round-trip property test (the sacred invariant):** every `a` in the test suite is immediately followed by `x` + byte-for-byte compare. Applies to unit tests (per model, per transform), integration tests, and a fuzz target (cargo-fuzz) on the decoder.
- **Corpus:** `sample_files/` + Silesia (already downloaded) + a folder of extra realistic files (docx, jpg, source-code tree, CSV, SQLite db — I'll assemble it).
- **Benchmark harness** (`dnz bench`, plus a standalone script for CI): produces a markdown table — file × {gzip -9, bzip2, xz -9e, zstd --ultra -22, 7z -mx=9, densezip} with sizes, ratios, and densezip's margin vs the best competitor. This is the acceptance report we regenerate at every milestone.

## 6. Milestones

| # | Deliverable | Gate to pass |
|---|---|---|
| M0 | Cargo workspace, archive format read/write, CLI skeleton, benchmark harness + corpus | round-trip green on corpus |
| M1 | L1+L2 via preflate-rs, lepton_jpeg, PNG unfilter; LZMA backend | **beats 7z on both sample files** (expected: PNG −60%+, PDF −10%+) |
| M2 | dzcm v1: orders 0–6 + match model + mixer + APM | ≥ xz on all Silesia files; ≥ zpaq -m5 on text (go/no-go: else FFI fallback) |
| M3 | Specialized models: 2D image, word/text, sparse; x86 transform | beats zpaq -m5 on Silesia total; PNG < 140 KB |
| M4 | Multi-backend racing, solid grouping, multi-file archives, parallelism | beats every competitor on every corpus file |
| M5 | Hardening: fuzzing, edge cases (0-byte, huge, non-UTF8 names), docs, final report | clean fuzz run + final benchmark table |

## 7. Risks

| Risk | Mitigation |
|---|---|
| Homegrown CM underperforms zpaq | Trial-all selector means we never do *worse* than LZMA; libzpaq FFI fallback keeps the "beat 7z" claim while dzcm matures |
| preflate can't reconstruct some deflate variants (rare encoders) | It reports failure per stream → we store that stream untouched; correctness never at risk |
| Correction-record overhead eats the gain on tiny streams | Only recompress a stream when trial shows net win (we measure both, keep smaller) |
| CM is slow (minutes for MBs) | Accepted per requirements; parallelize across streams/backends over 64 threads |
| Pathological inputs (random data) can't shrink | Store-raw fallback caps overhead at a few bytes per stream — we never expand meaningfully |

## 8. Expected outcome (from measurements above)

| File | 7z -mx=9 | densezip (projected) |
|---|---|---|
| sample.png | 428,409 | **≈130–147 KB** (−65%+) |
| sample.pdf | 461,811 | **≈370–395 KB** (−15%+, more with lepton on embedded JPEGs) |
| Silesia text/binary | xz/7z baseline | **−12–27%** (CM backend) |
| Already-zipped (docx, jar, gz) | ~0% for competitors | **−20–60%** via preflate recompression |
