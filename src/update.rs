//! `dnz update`: self-update by re-running the platform installer, pointed
//! at the directory the current binary lives in.

use anyhow::{Context, Result, bail, ensure};
use std::env;
use std::path::Path;
use std::process::Command;

const REPO: &str = "dannyblaker/densezip";

pub fn update(force: bool) -> Result<()> {
    let exe = env::current_exe().context("cannot locate the running dnz binary")?;
    let exe = exe.canonicalize().unwrap_or(exe);

    if let Some(hint) = source_build_hint(&exe) {
        bail!(
            "this dnz was not installed from a release ({}).\n{}",
            exe.display(),
            hint
        );
    }

    let current = env!("CARGO_PKG_VERSION");
    match latest_release_version() {
        Ok(latest) if latest == current && !force => {
            println!("dnz {current} is already the latest release (use --force to reinstall)");
            return Ok(());
        }
        Ok(latest) => println!("updating dnz {current} -> {latest} ..."),
        // The installer resolves "latest" itself, so a failed check is not fatal.
        Err(e) => println!("could not check the latest version ({e:#}); reinstalling latest ..."),
    }

    let dir = exe
        .parent()
        .context("cannot determine the install directory")?;
    run_installer(dir, &exe)?;
    Ok(())
}

/// Detect binaries that came from `cargo build` / `cargo install` rather than
/// a release download; overwriting those would surprise the user.
fn source_build_hint(exe: &Path) -> Option<&'static str> {
    let has = |name: &str| exe.components().any(|c| c.as_os_str() == name);
    if has("target") && (has("release") || has("debug")) {
        Some("Update it by rebuilding: git pull && cargo build --release")
    } else if has(".cargo") {
        Some("Update it with: cargo install --git https://github.com/dannyblaker/densezip")
    } else {
        None
    }
}

fn latest_release_version() -> Result<String> {
    let api = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let out = if cfg!(windows) {
        Command::new("powershell")
            .args(["-NoProfile", "-Command"])
            .arg(format!("(Invoke-RestMethod '{api}').tag_name"))
            .output()
    } else {
        Command::new("curl").args(["-fsSL", &api]).output()
    }
    .context("failed to run the release check")?;
    ensure!(out.status.success(), "release check failed");
    let body = String::from_utf8_lossy(&out.stdout);
    let tag = if cfg!(windows) {
        body.trim().to_string()
    } else {
        parse_tag_name(&body).context("no tag_name in the GitHub API response")?
    };
    Ok(tag.trim_start_matches('v').to_string())
}

fn parse_tag_name(json: &str) -> Option<String> {
    let rest = &json[json.find("\"tag_name\"")? + "\"tag_name\"".len()..];
    let rest = &rest[rest.find('"')? + 1..];
    Some(rest[..rest.find('"')?].to_string())
}

fn run_installer(dir: &Path, exe: &Path) -> Result<()> {
    let status = if cfg!(windows) {
        // Windows locks a running executable against overwrite, but allows
        // renaming it; move ourselves aside so the installer can replace us.
        let old = exe.with_extension("exe.old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old).context("failed to move the running dnz.exe aside")?;
        let url = format!("https://raw.githubusercontent.com/{REPO}/master/install.ps1");
        let status = Command::new("powershell")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command"])
            .arg(format!("irm '{url}' | iex"))
            .env("DNZ_INSTALL_DIR", dir)
            .status();
        if !matches!(&status, Ok(s) if s.success()) {
            let _ = std::fs::rename(&old, exe); // roll back so dnz keeps working
        }
        status
    } else {
        let url = format!("https://raw.githubusercontent.com/{REPO}/master/install.sh");
        Command::new("bash")
            .arg("-c")
            .arg(format!("curl -fsSL '{url}' | bash"))
            .env("DNZ_INSTALL_DIR", dir)
            .status()
    }
    .context("failed to run the installer")?;
    ensure!(status.success(), "installer failed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_name_parsing() {
        let json = r#"{"url":"...","tag_name":"v0.2.1","name":"v0.2.1"}"#;
        assert_eq!(parse_tag_name(json).as_deref(), Some("v0.2.1"));
        assert_eq!(parse_tag_name("{}"), None);
    }

    #[test]
    fn source_build_detection() {
        assert!(source_build_hint(Path::new("/src/densezip/target/release/dnz")).is_some());
        assert!(source_build_hint(Path::new("/home/u/.cargo/bin/dnz")).is_some());
        assert!(source_build_hint(Path::new("/home/u/.local/bin/dnz")).is_none());
    }
}
