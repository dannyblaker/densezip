mod update;

use densezip::archive;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "dnz",
    version,
    about = "Dense Zip: an archiver tuned for the smallest possible output \
             (speed is deliberately sacrificed for size)",
    after_help = "\
Examples:
  dnz a backup.dnz documents/          archive a folder (recursive)
  dnz a backup.dnz notes.txt photos/   mix files and folders freely
  dnz a --progress backup.dnz data/    show a progress bar with ETA
  dnz a --no-cm backup.dnz data/       much faster, slightly larger output
  dnz x backup.dnz -o restored/        extract into restored/
  dnz t backup.dnz                     verify every file reconstructs
  dnz ls backup.dnz                    list contents
  dnz update                           update dnz to the latest release

Every archive is self-checked: after packing, it is read back and every
file is verified to reconstruct bit-exactly (--no-verify to skip)."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Show a progress bar with ETA on stderr (pack/extract/verify)
    #[arg(long, global = true)]
    progress: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create an archive (directories are added recursively)
    #[command(visible_alias = "add")]
    A {
        /// The .dnz archive to create
        archive: PathBuf,
        /// Files and/or directories to add; directories recurse
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// Skip the post-pack verification pass
        #[arg(long)]
        no_verify: bool,
        /// Disable the (slow) context-mixing backend
        #[arg(long)]
        no_cm: bool,
        /// Memory budget in GiB (default: auto-detect 75% of available RAM)
        #[arg(long)]
        mem: Option<f64>,
    },
    /// Extract an archive
    #[command(visible_alias = "extract")]
    X {
        archive: PathBuf,
        /// Output directory (created if missing)
        #[arg(short, long, default_value = ".")]
        out: PathBuf,
        /// Overwrite existing files instead of stopping
        #[arg(long)]
        overwrite: bool,
    },
    /// Verify archive integrity (full reconstruction check)
    T { archive: PathBuf },
    /// List archive contents
    Ls { archive: PathBuf },
    /// Update dnz to the latest release
    Update {
        /// Reinstall even if already on the latest version
        #[arg(long)]
        force: bool,
    },
    /// (dev) Compress one file with a single backend, report size, verify roundtrip
    #[command(hide = true)]
    Raw {
        file: PathBuf,
        /// store|zstd|brotli|lzma|cm
        #[arg(long, default_value = "cm")]
        backend: String,
        /// record/row stride hint for the CM backend
        #[arg(long, default_value_t = 0)]
        stride: u32,
        /// bytes-per-pixel hint for the CM backend
        #[arg(long, default_value_t = 0)]
        bpp: u32,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.progress {
        densezip::progress::start();
    }
    let result = match cli.cmd {
        Cmd::A {
            archive,
            inputs,
            no_verify,
            no_cm,
            mem,
        } => archive::pack(&archive, &inputs, !no_cm, !no_verify, mem),
        Cmd::X {
            archive,
            out,
            overwrite,
        } => archive::extract(&archive, &out, overwrite),
        Cmd::T { archive } => archive::test(&archive),
        Cmd::Ls { archive } => archive::list(&archive),
        Cmd::Update { force } => update::update(force),
        Cmd::Raw {
            file,
            backend,
            stride,
            bpp,
        } => {
            use densezip::backends;
            let id = match backend.as_str() {
                "store" => backends::STORE,
                "zstd" => backends::ZSTD,
                "brotli" => backends::BROTLI,
                "lzma" => backends::LZMA,
                "cm" => backends::CM,
                x => anyhow::bail!("unknown backend {}", x),
            };
            let data = std::fs::read(&file)?;
            let t = std::time::Instant::now();
            let comp = backends::compress_one_public_hint(id, &data, stride, bpp)?;
            let ct = t.elapsed();
            let t = std::time::Instant::now();
            let back = backends::decompress(id, &comp, data.len())?;
            anyhow::ensure!(back == data, "ROUNDTRIP FAILED");
            println!(
                "{}: {} -> {} ({:.3}%)  comp {:.1}s ({:.2} MB/s)  dec {:.1}s",
                backend,
                data.len(),
                comp.len(),
                comp.len() as f64 * 100.0 / data.len().max(1) as f64,
                ct.as_secs_f64(),
                data.len() as f64 / 1e6 / ct.as_secs_f64(),
                t.elapsed().as_secs_f64()
            );
            Ok(())
        }
    };
    densezip::progress::finish();
    result
}
