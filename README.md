# densezip

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
macOS Intel/Apple Silicon, and Windows x86_64) and can be re-run any time to
update. Or build from source with Rust stable: `cargo build --release`.

## Usage

```
dnz a archive.dnz <files/dirs...>   # create (verifies bit-exact reconstruction)
dnz x archive.dnz -o <dir>          # extract
dnz t archive.dnz                   # verify integrity
dnz ls archive.dnz                  # list contents
```

Options: `--no-cm` disables the slow context-mixing backend (much faster,
still beats 7z on most container formats); `--no-verify` skips the post-pack
self-check; `--mem <GiB>` caps memory use.

**Memory budget:** by default densezip auto-detects available RAM and uses up
to 75% of it, sizing its model tables, LZMA dictionaries, and job concurrency
to fit — so it runs safely on an 8 GB laptop and simply uses bigger models on
a workstation. `--mem 2` forces a 2 GiB budget explicitly. The cost of small
budgets is tiny (measured on sample.pdf: 2 GiB costs +0.02% output size,
512 MiB +0.13%). Extraction needs roughly the model memory chosen at pack
time (at most ~3.3 GiB, less for archives packed with a small `--mem`), so
pack with `--mem` matched to the smallest machine that must read the archive.

## Why it wins

Three stacked ideas, each validated by measurement:

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
never meaningfully expands, even on random data.

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

Run `bench/bench.sh <corpus-dir>` to reproduce (also verifies every archive).
See `BENCHMARKS.md` for the full report; highlights vs the **best** of
gzip/bzip2/xz/zstd/7z per file:

| file | best competitor | densezip | margin |
|---|---:|---:|---:|
| sample.png (desktop screenshot) | 202,667 | 42,193 | −79.2% |
| sample.pdf (Et Lorem text) | 72,039 | 31,559 | −56.2% |
| SQLite database | 157,108 | 74,365 | −52.7% |
| photo.jpg | 325,424 | 257,170 | −21.0% |
| Silesia (text/xml/binary) | xz/7z | dzcm | −0.1% … −27.5% |

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
