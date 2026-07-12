//! densezip: an archiver tuned for the smallest possible output.
//!
//! Pipeline: scan for embedded compressed streams (deflate, PNG IDAT, JPEG),
//! losslessly recompress them (preflate / lepton), apply reversible
//! transforms, then race every stream through store/zstd/brotli/LZMA/dzcm
//! and keep the smallest verified result. See README.md for the format
//! overview and BENCHMARKS.md for measured results.

pub mod archive;
pub mod backends;
pub mod channels;
pub mod cm;
pub mod plan;
pub mod progress;
pub mod rebuild;
pub mod scan;
pub mod util;
