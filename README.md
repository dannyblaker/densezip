# Dense Zip

[![CI](https://github.com/dannyblaker/densezip/actions/workflows/ci.yml/badge.svg)](https://github.com/dannyblaker/densezip/actions/workflows/ci.yml)

An archiver whose only goal is the **smallest possible output** — smaller than
`gzip -9`, `xz -9e`, `zstd --ultra -22`, and `7z -mx=9` on real-world files.
Speed and memory are explicitly sacrificed for ratio.

The CLI command is `dnz` and archives use the `.dnz` extension.

## Install

Linux / macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/dannyblaker/densezip/master/install.sh | bash
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/dannyblaker/densezip/master/install.ps1 | iex
```

Both install the latest stable release (prebuilt for Linux x86_64/arm64,
macOS Intel/Apple Silicon, and Windows x86_64). Or build from source with
Rust stable: `cargo build --release`.

### Updating

```sh
dnz update
```

This checks GitHub for the latest release and, if you're behind, replaces the
installed binary in place (`--force` reinstalls even when already current).
Re-running the install one-liner above does the same thing. If you built from
source, update with `git pull && cargo build --release` instead — `dnz update`
detects source builds and won't overwrite them.

To pin or roll back to a specific version, pass a tag to the installer:

```sh
DNZ_VERSION=v0.1.0 curl -fsSL https://raw.githubusercontent.com/dannyblaker/densezip/master/install.sh | bash
```

(on Windows: `$env:DNZ_VERSION="v0.1.0"` before running the PowerShell
one-liner). Check what you have with `dnz --version`.

## Usage

```
dnz a archive.dnz <files/dirs...>   # create (verifies bit-exact reconstruction)
dnz x archive.dnz -o <dir>          # extract
dnz t archive.dnz                   # verify integrity
dnz ls archive.dnz                  # list contents
```

Options: `--no-cm` disables the slow context-mixing backend (much faster,
still beats 7z on most container formats); `--no-verify` skips the post-pack
self-check; `--mem <GiB>` caps memory use; `--progress` shows a live
progress bar with an ETA on stderr (works on `a`, `x`, and `t`)

**Memory budget:** by default densezip auto-detects available RAM and uses up
to 75% of it, sizing its model tables, LZMA dictionaries, and job concurrency
to fit — so it runs safely on an 8 GB laptop and simply uses bigger models on
a workstation. `--mem 2` forces a 2 GiB budget explicitly. The cost of small
budgets is tiny (measured on sample.pdf: 2 GiB costs +0.02% output size,
512 MiB +0.13%). Extraction needs roughly the model memory chosen at pack
time (at most ~3.3 GiB, less for archives packed with a small `--mem`), so
pack with `--mem` matched to the smallest machine that must read the archive.

## Why it wins

Three stacked ideas, each validated by measurement (see
[WHITEPAPER.md](WHITEPAPER.md) for the full technical treatment with
diagrams and the underlying math):

**1. Recompression.** Most "hard to compress" files are already-compressed
containers: PDFs, PNGs, docx/xlsx, jar, gz — all deflate inside. densezip
finds every embedded deflate stream ([preflate-rs](https://github.com/microsoft/preflate-rs)),
losslessly unpacks it, compresses the *raw* content with far stronger codecs,
and stores a small correction record so the original bytes are reconstructed
**bit-exactly**. JPEGs (standalone or inside PDFs) get the same treatment via
[lepton](https://github.com/microsoft/lepton_jpeg_rust) (~20% smaller,
lossless). PNG pixels are additionally unfiltered and color-decorrelated when
that helps.

**2. A context-mixing compressor (`dzcm`).** The strongest known general
compressors (PAQ family) predict one bit at a time from many context models
blended by an online-trained mixer. dzcm is a clean-room Rust implementation:
orders 0–8, word, sparse, and record/2D-image context models with bit-history
states, an ISSE refinement chain, a two-bank logistic mixer, three APM/SSE
stages, plus an autodetected E8/E9 x86 branch transform and record-stride
detection. Pure integer math — output is bit-identical across platforms.
On the Silesia corpus it beats `xz -9e` by 13–24%.

**3. Backend racing.** No single codec wins everywhere, and we don't care
about time — so every stream is compressed with zstd, brotli, LZMA, *and*
dzcm in parallel (plus alternate pixel representations for images), each
round-trip verified, and the smallest wins. A stored fallback means output
never meaningfully expands, even on random data. Since v0.1.5 the alternate
pixel representations and the LZMA parameter variants race concurrently too
instead of back-to-back — about 1.5× faster on image-heavy archives and
nearly 3× with `--no-cm`, with byte-identical output.

## Correctness

The format never trusts heuristics:

* every transform is verified at pack time — each file is re-rendered and
  byte-compared before it is committed (mismatch ⇒ that file is stored raw);
* every backend output is decompressed and compared before being accepted;
* after writing, the whole archive is read back and every file verified
  against its xxh3 hash (`--no-verify` to skip);
* `dnz t` re-verifies everything, and truncated/corrupted archives fail
  cleanly (tested).

## Architecture

```
input file
  └─ scan: deflate streams (zlib/gzip/zip/PDF), PNG IDAT runs, JPEGs
       └─ L1 recompression: preflate / lepton  → raw content + corrections
            └─ L2 transforms: PNG unfilter, sub-green decorrelation,
                              E8/E9 x86, record-stride detection
                 └─ L3 racing: store | zstd-22 | brotli-11 | LZMA | dzcm
                               → smallest verified output per channel
```

Streams are grouped into solid channels (literals, corrections, filters,
lepton blobs, per-image pixels) shared across all files in the archive, so
similar content compresses together. The TOC stores a reversible "plan tree"
per file; extraction replays it bottom-up.

## Benchmarks

densezip wins on **all 20 files** across both corpora — against the *best*
of gzip/bzip2/xz/zstd/7z chosen per file, a stricter baseline than any
single tool:

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/benchmarks-dark-chart.svg">
  <img alt="Bar chart: how much smaller densezip's output is than the best competitor's for each of 20 files. Reductions range from 0.1% (silesia/nci) to 79.2% (sample.png)." src="assets/benchmarks-light-chart.svg">
</picture>

Machine: 2× Xeon E5-2683 v4 (32 cores / 64 threads), 125 GiB RAM.
Competitors: `gzip -9`, `bzip2 -9`, `xz -9e`, `zstd --ultra -22 --long=27`,
`7z -mx=9` (LZMA2). The **"vs best" column compares densezip against the
best competitor for each individual file**. Every densezip archive in these
tables was verified (`dnz t`) to reconstruct all inputs bit-exactly.
Run `bench/bench.sh <corpus-dir>` to reproduce (also verifies every
archive); the chart above is generated from the tables by
`bench/readme_chart.py`.

### Real-world files

Mixed real-world formats: two synthetic samples (`sample.pdf` — 80 pages of
"Et Lorem" text with FlateDecode streams; `sample.png` — a 2200×1160
desktop-screenshot-style image), plus a SQLite database, a JPEG photo, a
noisy photographic PNG, a Word document, CSV data, and a gzipped source
tarball.

| file | original | gzip-9 | bzip2-9 | xz-9e | zstd-22 | 7z-mx9 | densezip | vs best |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| inventory.db | 1,409,024 | 342,242 | 212,311 | 157,108 | 195,934 | 158,418 | **74,365** | **−52.7%** |
| photo.jpg | 345,670 | 339,686 | 325,424 | 339,356 | 337,526 | 338,996 | **257,170** | **−21.0%** |
| photo.png | 3,722,509 | 3,723,107 | 3,741,754 | 3,722,756 | 3,722,609 | 3,722,863 | **3,686,156** | **−1.0%** |
| report.docx | 21,679 | 4,951 | 5,732 | 4,400 | 4,647 | 4,485 | **3,226** | **−26.7%** |
| sales.csv | 3,142,709 | 341,330 | 85,526 | 7,656 | 9,966 | 10,206 | **6,008** | **−21.5%** |
| sample.pdf | 91,775 | 73,141 | 73,531 | 72,100 | 72,039 | 72,304 | **31,559** | **−56.2%** |
| sample.png | 236,301 | 208,494 | 212,466 | 202,784 | 203,126 | 202,643 | **42,193** | **−79.2%** |
| src.tar.gz | 31,956 | 31,948 | 32,436 | 32,016 | 31,970 | 32,090 | **21,015** | **−34.2%** |
| **TOTAL** | 9,001,623 | | | | | best-of: 4,523,827 | **4,121,692** | **−8.9%** |

densezip wins on **every file**. Notes:

* `sample.png` (−79%): screenshot-style image — IDAT recompression +
  unfiltering + backend racing crush flat UI regions and rendered text.
* `sample.pdf` (−56%): FlateDecode streams un-deflated, then the text inside
  is compressed by the dzcm context-mixing engine.
* `inventory.db` (−53%): dzcm's record model locks onto SQLite page/row structure.
* `src.tar.gz` / `report.docx` (−27…−34%): preflate deflate recompression —
  competitors see only high-entropy bytes here.
* `photo.jpg` (−21%): lossless lepton JPEG recompression.
* `photo.png` (−1%): worst case — photographic noise is near-incompressible;
  the win comes from unfiltering + sub-green decorrelation. Never a loss,
  because racing includes the identity path.

### Silesia corpus (general-purpose data)

Standard 12-file compression corpus (~212 MB): text, XML, executables,
databases, medical imaging, etc.

| file | original | gzip-9 | bzip2-9 | xz-9e | zstd-22 | 7z-mx9 | densezip | vs best |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| dickens | 10,192,446 | 3,851,823 | 2,799,520 | 2,831,212 | 2,849,381 | 2,831,111 | **2,201,856** | **−21.3%** |
| mozilla | 51,220,480 | 18,994,142 | 17,914,392 | 13,376,240 | 14,967,572 | 13,344,686 | **12,005,293** | **−10.0%** |
| mr | 9,970,564 | 3,673,940 | 2,441,280 | 2,751,892 | 3,105,643 | 2,748,257 | **2,310,959** | **−5.3%** |
| nci | 33,553,445 | 2,987,533 | 1,812,734 | 1,449,272 | 1,610,427 | 1,741,410 | **1,447,354** | **−0.1%** |
| ooffice | 6,152,192 | 3,090,442 | 2,862,526 | 2,427,224 | 2,598,777 | 2,425,568 | **1,846,011** | **−23.9%** |
| osdb | 10,085,684 | 3,716,342 | 2,802,792 | 2,844,556 | 3,098,444 | 2,851,796 | **2,313,439** | **−17.5%** |
| reymont | 6,627,202 | 1,820,834 | 1,246,230 | 1,315,592 | 1,347,556 | 1,318,394 | **999,824** | **−19.8%** |
| samba | 21,606,400 | 5,408,272 | 4,549,759 | 3,739,524 | 3,876,634 | 3,759,770 | **2,991,390** | **−20.0%** |
| sao | 7,251,944 | 5,327,041 | 4,940,524 | 4,425,664 | 5,000,515 | 4,413,926 | **3,847,341** | **−12.8%** |
| webster | 41,458,703 | 12,061,624 | 8,644,714 | 8,368,672 | 8,458,469 | 8,388,839 | **6,067,846** | **−27.5%** |
| xml | 5,345,280 | 662,284 | 441,186 | 434,892 | 453,173 | 455,003 | **362,768** | **−16.6%** |
| x-ray | 8,474,240 | 6,037,713 | 4,051,112 | 4,491,264 | 5,155,752 | 4,479,871 | **3,796,802** | **−6.3%** |
| **TOTAL** | 211,938,580 | | | | | best-of: 47,517,474 | **40,190,883** | **−15.4%** |

densezip wins on **all 12 files**: 15.4% smaller in total than the best
competitor chosen per file. The closest call is `nci` (−0.1%, decided by the
multi-parameter LZMA race); the largest margins are on text (webster −27.5%,
dickens −21.3%) and executables (ooffice −23.9%, samba −20.0%) where the
dzcm context-mixing engine wins outright.

### dzcm engine vs zpaq -m5 (context-mixing reference)

Measured on raw files with the dev command `dnz raw <file> --backend cm`:

| file | xz -9e | zpaq -m5 | dzcm | dzcm vs xz |
|---|---:|---:|---:|---:|
| silesia/xml | 434,892 | 326,956 | 362,684 | −16.6% |
| silesia/dickens | 2,831,212 | 2,094,756 | 2,201,767 | −22.2% |
| silesia/ooffice | 2,427,224 | 1,766,563 | 1,845,923 | −24.0% |
| silesia/sao | 4,425,664 | 3,899,267 | **3,847,256** | −13.1% |

dzcm beats xz everywhere by double digits and already edges out zpaq -m5 on
record-structured data; zpaq -m5 keeps a mid-single-digit lead on pure text.
The archive-level results above don't depend on winning that race: backend
racing takes whichever codec is smallest per stream.

## Building & testing

```
cargo build --release
cargo test --release        # 14 integration + 9 unit tests, all round-trip based
```

Requires Rust stable. The compressor allocates large hash tables (hundreds of
MB to a few GB depending on input size) and uses all cores for backend racing.

## Status

The format is young (v0.1) and may still change between versions — don't use
`.dnz` as your only copy of anything yet. Every archive is self-checked at
pack time, and `dnz t` verifies bit-exact reconstruction at any time.

## License

AGPL-3.0-or-later. If you want to use densezip in a proprietary product,
contact me about a commercial license.

densezip builds on excellent open-source work:
[preflate-rs](https://github.com/microsoft/preflate-rs) and
[lepton_jpeg_rust](https://github.com/microsoft/lepton_jpeg_rust) (Microsoft,
Apache-2.0), plus the zstd, brotli, and lzma-rust2 crates. The dzcm
context-mixing engine is an original implementation inspired by the published
PAQ/ZPAQ architecture.
