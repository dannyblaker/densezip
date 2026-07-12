# densezip installer for Windows: downloads the latest stable dnz.exe from
# GitHub releases, verifies its checksum, installs it, and puts it on your
# user PATH. Re-run any time to update.
#
#   irm https://raw.githubusercontent.com/dannyblaker/densezip/master/install.ps1 | iex
#
# Environment overrides:
#   DNZ_INSTALL_DIR   install directory (default: %LOCALAPPDATA%\densezip\bin)
#   DNZ_VERSION       tag to install, e.g. v0.1.0 (default: latest stable)
$ErrorActionPreference = "Stop"

$repo = "dannyblaker/densezip"
$target = "x86_64-pc-windows-msvc"
if ($env:PROCESSOR_ARCHITECTURE -ne "AMD64") {
    throw "densezip install: only x86_64 Windows binaries are published. Build from source: cargo build --release"
}

$installDir = if ($env:DNZ_INSTALL_DIR) { $env:DNZ_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "densezip\bin" }
$base = if ($env:DNZ_VERSION) { "https://github.com/$repo/releases/download/$($env:DNZ_VERSION)" }
        else { "https://github.com/$repo/releases/latest/download" }
$asset = "dnz-$target.zip"

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("dnz-install-" + [System.Guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "downloading $asset ..."
    Invoke-WebRequest "$base/$asset" -OutFile (Join-Path $tmp $asset)
    Invoke-WebRequest "$base/$asset.sha256" -OutFile (Join-Path $tmp "$asset.sha256")

    $expected = ((Get-Content (Join-Path $tmp "$asset.sha256") -Raw).Trim() -split "\s+")[0].ToLower()
    $actual = (Get-FileHash (Join-Path $tmp $asset) -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $actual) { throw "densezip install: checksum verification failed" }

    Expand-Archive (Join-Path $tmp $asset) -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    Copy-Item (Join-Path $tmp "dnz.exe") (Join-Path $installDir "dnz.exe") -Force
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($userPath -split ";") -notcontains $installDir) {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$installDir", "User")
    Write-Host "added $installDir to your user PATH (open a new terminal to pick it up)"
}

$version = & (Join-Path $installDir "dnz.exe") --version
Write-Host "installed $version to $installDir"
