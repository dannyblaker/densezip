mod update;

use densezip::archive;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "dnz",
    version,
    about = "Dense Zip: An archiver that produces small archives."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create an archive
    #[command(alias = "add")]
    A {
        archive: PathBuf,
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
    #[command(alias = "extract")]
    X {
        archive: PathBuf,
        /// Output directory
        #[arg(short, long, default_value = ".")]
        out: PathBuf,
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
    match cli.cmd {
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
    }
}
