//! `noel bun install`: download the bun runtime into the agent dir for
//! TypeScript extension hosts. The pattern is upstream's `ensureTool` (its
//! `rg`/`fd` manager): resolve the latest release through GitHub's
//! redirect, download the one archive for this platform, extract the single
//! binary, make it executable. Unlike upstream's silent auto-download, this
//! runs only when the user asked - the TUI offers, the CLI command does.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// Where `noel bun install` puts the binary: `<agent dir>/bin/bun`.
/// `find_bun_binary` (noel-core) prefers it over PATH, so the install is
/// private to noel and never shadows a system bun.
pub fn bun_install_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("bin").join(bun_binary_name())
}

#[cfg(unix)]
fn bun_binary_name() -> &'static str {
    "bun"
}

#[cfg(windows)]
fn bun_binary_name() -> &'static str {
    "bun.exe"
}

/// The release asset for this platform, or why none exists. Musl systems
/// (Alpine, the e2e containers) get the musl build; baseline variants are
/// for old CPUs and not chosen automatically.
fn release_asset_name() -> Result<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "aarch64",
        other => return Err(anyhow!("no bun release for architecture {other}")),
    };
    let os = match std::env::consts::OS {
        "macos" => "darwin".to_string(),
        "windows" => "windows".to_string(),
        "linux" => {
            let musl = libc_flavor()
                .context("could not tell musl from glibc (is this an unusual libc?)")?;
            if musl == "musl" {
                format!("linux-{arch}-musl")
            } else {
                format!("linux-{arch}")
            }
        }
        other => return Err(anyhow!("no bun release for platform {other}")),
    };
    Ok(format!("bun-{os}.zip"))
}

/// How the running noel was linked: musl or glibc. The release asset must
/// match, or the binary dies on loader errors.
#[cfg(target_os = "linux")]
fn libc_flavor() -> Option<&'static str> {
    // How *noel* was linked decides: the bun binary must load under the
    // same loader noel runs with (a musl noel runs where glibc bun dies).
    if cfg!(target_env = "musl") {
        Some("musl")
    } else {
        Some("glibc")
    }
}

#[cfg(not(target_os = "linux"))]
fn libc_flavor() -> Option<&'static str> {
    Some("")
}

/// The latest bun release version, resolved through the `releases/latest`
/// redirect (no API quota, like upstream's tools manager).
async fn latest_bun_version(client: &reqwest::Client) -> Result<String> {
    // Manual redirects: the Location header of the 302 *is* the answer
    // (the tagged release); following it would cost a full page fetch.
    let response = client
        .get("https://github.com/oven-sh/bun/releases/latest")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .context("could not reach github.com")?;
    let status = response.status();
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let Some(location) = location else {
        return Err(anyhow!(
            "resolving the latest bun release failed: HTTP {status} without a redirect"
        ));
    };
    let tag = location
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .strip_prefix("bun-v")
        .map(str::to_string)
        .ok_or_else(|| anyhow!("unexpected release redirect: {location}"))?;
    Ok(tag)
}

/// Install bun into `agent_dir/bin/bun`, replacing any previous download.
/// Returns the installed path. `confirm` runs before the download starts;
/// its `Err` cancels with the given message.
pub async fn install_bun(
    agent_dir: &Path,
    confirm: impl FnOnce(&str) -> std::io::Result<bool>,
    assume_yes: bool,
) -> Result<PathBuf> {
    let asset = release_asset_name()?;
    // The version resolve must NOT follow redirects: the 302's Location
    // header *is* the answer (the tagged release).
    let client = reqwest::Client::builder()
        .user_agent(concat!("noel/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    // The asset download MUST follow them: github.com 302s to a signed,
    // expiring release-assets.githubusercontent.com URL.
    let download_client = reqwest::Client::builder()
        .user_agent(concat!("noel/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let version = latest_bun_version(&client).await?;
    if !assume_yes {
        let proceed = confirm(&version).map_err(|error| anyhow!("prompt failed: {error}"))?;
        if !proceed {
            return Err(anyhow!("declined: bun was not installed"));
        }
    }
    let url = format!("https://github.com/oven-sh/bun/releases/download/bun-v{version}/{asset}");
    eprintln!("downloading bun {version} ({asset})...");
    let response = download_client
        .get(&url)
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await
        .with_context(|| format!("download failed: {url}"))?
        .error_for_status()
        .with_context(|| format!("download failed: {url}"))?;
    let bytes = response.bytes().await.context("the download ended early")?;
    let binary = extract_single_binary(&bytes).context("the release archive was unusable")?;
    let destination = bun_install_path(agent_dir);
    let bin_dir = destination
        .parent()
        .ok_or_else(|| anyhow!("no parent for {}", destination.display()))?;
    std::fs::create_dir_all(bin_dir).with_context(|| format!("create {}", bin_dir.display()))?;
    let temp = destination.with_extension("part");
    std::fs::write(&temp, &binary).with_context(|| format!("write {}", temp.display()))?;
    make_executable(&temp);
    std::fs::rename(&temp, &destination)
        .with_context(|| format!("install {}", destination.display()))?;
    eprintln!("installed bun {version} to {}", destination.display());
    Ok(destination)
}

/// The one binary inside the release zip (`bun-<target>/bun`).
fn extract_single_binary(archive: &[u8]) -> Result<Vec<u8>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))?;
    let mut names: Vec<String> = zip.file_names().map(str::to_string).collect();
    names.sort();
    let name = names
        .iter()
        .find(|name| name.rsplit('/').next() == Some(bun_binary_name()))
        .ok_or_else(|| anyhow!("no {} in the archive", bun_binary_name()))?;
    let mut file = zip.by_name(name)?;
    let mut buffer = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut buffer)?;
    Ok(buffer)
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(mut permissions) = std::fs::metadata(path).map(|meta| meta.permissions()) else {
        return;
    };
    permissions.set_mode(0o755);
    let _ = std::fs::set_permissions(path, permissions);
}

#[cfg(windows)]
fn make_executable(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn release_asset_matches_this_platform() {
        // Every platform noel builds for has an asset name; the shape is
        // bun-<os>[-<arch>-musl].zip with the binary inside.
        let asset = release_asset_name().expect("an asset for this platform");
        assert!(asset.starts_with("bun-"), "{asset}");
        assert!(asset.ends_with(".zip"), "{asset}");
        if std::env::consts::OS == "linux" {
            let musl = cfg!(target_env = "musl");
            assert_eq!(asset.contains("-musl"), musl, "{asset}");
        }
    }

    #[test]
    fn extracts_the_binary_from_a_release_shaped_zip() {
        // A zip like the release asset: one directory, one binary in it.
        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buffer);
            let options: zip::write::SimpleFileOptions = Default::default();
            zip.add_directory("bun-test", options).expect("dir");
            zip.start_file("bun-test/bun", options).expect("file");
            zip.write_all(b"#!/bin/sh\necho bun\n").expect("content");
            zip.finish().expect("close");
        }
        let bytes = buffer.into_inner();
        let binary = extract_single_binary(&bytes).expect("the binary");
        assert_eq!(binary, b"#!/bin/sh\necho bun\n");
    }

    #[test]
    fn install_path_is_private_to_the_agent_dir() {
        let path = bun_install_path(Path::new("/agent"));
        assert_eq!(path, Path::new("/agent/bin").join(bun_binary_name()));
    }
}
