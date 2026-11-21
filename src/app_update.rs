//! Updates to Flash itself, independent of the driver update sources.
use crate::platform::InstallKind;
use ring::digest::{Context, SHA256};
use serde::Deserialize;
use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;

const REPOSITORY: &str = "https://github.com/ljcatherine/Flash";
const LATEST: &str = "https://api.github.com/repos/ljcatherine/Flash/releases/latest";
const MAX_ASSET: u64 = 128 * 1024 * 1024;

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    size: u64,
    digest: Option<String>,
    state: String,
}

#[derive(Clone, Debug)]
pub struct Available {
    pub version: String,
    pub notes: String,
    package: Option<Package>,
    kind: InstallKind,
}

impl Available {
    pub fn can_install(&self) -> bool {
        self.package.is_some()
    }

    /// Why Flash can't install this release itself.
    pub fn manual_note(&self) -> String {
        match &self.kind {
            InstallKind::Managed(how) => (*how).to_string(),
            _ => "This release has no verifiable package for your installation. Download it from the release page.".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Package {
    url: String,
    pub(crate) filename: String,
    size: u64,
    pub(crate) digest: String,
    pub(crate) kind: InstallKind,
}

/// The release asset for an installation of this kind and CPU architecture.
pub fn asset_name(kind: &InstallKind, version: &str, arch: &str) -> Option<String> {
    let deb_arch = if arch == "aarch64" { "arm64" } else { "amd64" };
    match kind {
        InstallKind::Msi => Some(format!("Flash-{version}-x64.msi")),
        InstallKind::Exe => Some(format!("Flash-{version}-x64.exe")),
        InstallKind::AppImage(_) => Some(format!("Flash-{version}-{arch}.AppImage")),
        InstallKind::Deb => Some(format!("flash_{version}_{deb_arch}.deb")),
        InstallKind::Rpm => Some(format!("flash-{version}.{arch}.rpm")),
        InstallKind::Tarball(_) => Some(format!("flash-{version}-{arch}.tar.gz")),
        InstallKind::Managed(_) => None,
    }
}

fn version(text: &str) -> Option<[u16; 3]> {
    let mut parts = text.split('.');
    let mut numbers = [0; 3];
    for number in &mut numbers {
        let part = parts.next()?;
        if part.is_empty()
            || (part.len() > 1 && part.starts_with('0'))
            || !part.bytes().all(|c| c.is_ascii_digit())
        {
            return None;
        }
        *number = part.parse().ok()?;
    }
    parts.next().is_none().then_some(numbers)
}

fn parse_release(
    release: Release,
    current: &str,
    kind: InstallKind,
) -> Result<Option<Available>, String> {
    let current = version(current).ok_or("This build does not use a stable version number.")?;
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let tag = &release.tag_name;
    let published = tag.strip_prefix('v').ok_or("Invalid Flash release tag.")?;
    if version(published).ok_or("Invalid Flash release version.")? <= current {
        return Ok(None);
    }
    let filename = asset_name(&kind, published, std::env::consts::ARCH).unwrap_or_default();
    let package = release.assets.into_iter().find_map(|asset| {
        if filename.is_empty()
            || asset.name != filename
            || asset.state != "uploaded"
            || !(1..=MAX_ASSET).contains(&asset.size)
        {
            return None;
        }
        let digest = asset.digest.as_deref()?.strip_prefix("sha256:")?;
        if digest.len() != 64 || !digest.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        Some(Package {
            url: format!("{REPOSITORY}/releases/download/{tag}/{filename}"),
            filename: filename.clone(),
            size: asset.size,
            digest: digest.to_ascii_lowercase(),
            kind: kind.clone(),
        })
    });
    Ok(Some(Available {
        version: published.into(),
        notes: format!("{REPOSITORY}/releases/tag/{tag}"),
        package,
        kind,
    }))
}

fn agent(timeout: u64) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .user_agent(concat!("Flash/", env!("CARGO_PKG_VERSION")))
        .redirects(0)
        .timeout_connect(Duration::from_secs(8))
        .timeout(Duration::from_secs(timeout))
        .build()
}

pub fn check() -> Result<Option<Available>, String> {
    let exe = std::env::current_exe().map_err(|err| err.to_string())?;
    let response = agent(15)
        .get(LATEST)
        .set("Accept", "application/vnd.github+json")
        .call();
    let response = match response {
        Err(ureq::Error::Status(404, _)) => return Ok(None),
        other => other.map_err(|err| format!("Could not check GitHub for updates: {err}"))?,
    };
    if response.status() != 200 {
        return Err("GitHub returned an unexpected response.".into());
    }
    let release = serde_json::from_reader(response.into_reader().take(2 * 1024 * 1024))
        .map_err(|_| "GitHub returned unreadable release information.")?;
    parse_release(release, env!("CARGO_PKG_VERSION"), crate::platform::install_kind(&exe))
}

fn asset_response(agent: &ureq::Agent, url: &str) -> Result<ureq::Response, String> {
    let mut response = agent
        .get(url)
        .call()
        .map_err(|err| format!("Download request failed: {err}"))?;
    for _ in 0..4 {
        if response.status() == 200 {
            return Ok(response);
        }
        let next = response.header("Location").unwrap_or_default();
        if !matches!(response.status(), 301 | 302 | 303 | 307 | 308)
            || !(next.starts_with("https://release-assets.githubusercontent.com/")
                || next.starts_with("https://objects.githubusercontent.com/"))
        {
            return Err("GitHub redirected the download to an unexpected location.".into());
        }
        response = agent
            .get(next)
            .call()
            .map_err(|_| "Could not download the GitHub release asset.")?;
    }
    Err("Too many redirects while downloading the release asset.".into())
}

fn verify_download(
    mut source: impl Read,
    mut dest: impl Write,
    size: u64,
    expected: &str,
) -> std::io::Result<()> {
    let mut hash = Context::new(&SHA256);
    let mut buf = [0_u8; 32 * 1024];
    let mut remaining = size;
    loop {
        let read = source.read(&mut buf)?;
        if read == 0 {
            break;
        }
        remaining = remaining
            .checked_sub(read as u64)
            .ok_or_else(|| std::io::Error::other("The download exceeded the published size."))?;
        hash.update(&buf[..read]);
        dest.write_all(&buf[..read])?;
    }
    let actual: String = hash
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if remaining != 0 || actual != expected {
        return Err(std::io::Error::other(
            "The download did not match the published SHA-256 digest and size.",
        ));
    }
    Ok(())
}

pub struct Downloaded {
    directory: TempDir,
    path: PathBuf,
    exe: PathBuf,
    package: Package,
}

pub fn download(update: &Available) -> Result<Downloaded, String> {
    let package = update
        .package
        .clone()
        .ok_or("This release has no verifiable package.")?;
    let exe = std::env::current_exe().map_err(|err| err.to_string())?;
    if package.kind != crate::platform::install_kind(&exe) {
        return Err("The installation changed. Check for updates again.".into());
    }
    let exe = crate::platform::installed_exe(&exe);
    let response = asset_response(&agent(120), &package.url)?;
    // Windows: under Program Files, so a standard user cannot replace it between
    // verification and installation. Linux: a private folder of the user's; a package
    // install is checked against the digest again by the root helper.
    let directory = tempfile::Builder::new()
        .prefix("Flash-update-")
        .tempdir_in(crate::platform::update_dir().map_err(|err| err.to_string())?)
        .map_err(|err| format!("Could not create the update directory: {err}"))?;
    let path = directory.path().join(&package.filename);
    let mut file = File::create_new(&path).map_err(|err| err.to_string())?;
    verify_download(
        response.into_reader(),
        &mut file,
        package.size,
        &package.digest,
    )
    .and_then(|()| file.sync_all())
    .map_err(|err| format!("Could not verify the update: {err}"))?;
    Ok(Downloaded {
        directory,
        path,
        exe,
        package,
    })
}

impl Downloaded {
    /// Starts installing the verified download. Flash closes afterwards.
    pub fn apply(self) -> std::io::Result<()> {
        crate::platform::apply_update(&self.exe, &self.path, &self.package.digest, &self.package.kind)?;
        // The helper owns cleanup after this process closes.
        let _ = self.directory.keep();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str) -> Release {
        let arch = std::env::consts::ARCH;
        let names = ["Flash-1.10.0-x64.exe".to_string(), "Flash-1.10.0-x64.msi".to_string(), format!("Flash-1.10.0-{arch}.AppImage"), "flash_1.10.0_amd64.deb".into(), format!("flash-1.10.0.{arch}.rpm"), format!("flash-1.10.0-{arch}.tar.gz")];
        let assets = names.map(|name| {
            serde_json::json!({
                "name": name,
                "size": 8,
                "state": "uploaded",
                "digest": format!("sha256:{}", "a".repeat(64)),
            })
        });
        serde_json::from_value(serde_json::json!({
            "tag_name": tag, "draft": false, "prerelease": false,
            "assets": assets,
        }))
        .unwrap()
    }

    #[test]
    fn chooses_the_matching_package_and_compares_versions_numerically() {
        let arch = std::env::consts::ARCH;
        for (kind, name) in [
            (InstallKind::Msi, "Flash-1.10.0-x64.msi".to_string()),
            (InstallKind::Exe, "Flash-1.10.0-x64.exe".to_string()),
            (InstallKind::AppImage("/home/u/Flash.AppImage".into()), format!("Flash-1.10.0-{arch}.AppImage")),
            (InstallKind::Rpm, format!("flash-1.10.0.{arch}.rpm")),
            (InstallKind::Tarball("/opt/flash/flash".into()), format!("flash-1.10.0-{arch}.tar.gz")),
        ] {
            let update = parse_release(release("v1.10.0"), "1.9.9", kind).unwrap().unwrap();
            assert_eq!(update.package.unwrap().filename, name);
        }
        assert_eq!(asset_name(&InstallKind::Deb, "1.2.0", "x86_64").as_deref(), Some("flash_1.2.0_amd64.deb"));
        assert_eq!(asset_name(&InstallKind::Deb, "1.2.0", "aarch64").as_deref(), Some("flash_1.2.0_arm64.deb"));
        assert!(parse_release(release("v1.10.0"), "1.10.0", InstallKind::Msi).unwrap().is_none());
        assert!(parse_release(release("v1.10.0"), "2.0.0", InstallKind::Msi).unwrap().is_none());
        assert!(parse_release(release("v1.10.0/other"), "1.9.9", InstallKind::Msi).is_err());
        // Distribution, Flatpak and Nix installs are told to use their package manager.
        let managed = parse_release(release("v1.10.0"), "1.9.9", InstallKind::Managed("Update Flash with your package manager.")).unwrap().unwrap();
        assert!(!managed.can_install());
        assert_eq!(managed.manual_note(), "Update Flash with your package manager.");
    }

    #[test]
    fn skips_prereleases_and_requires_a_digest_for_in_app_install() {
        let mut preview = release("v1.10.0");
        preview.prerelease = true;
        assert!(parse_release(preview, "1.0.0", InstallKind::Msi).unwrap().is_none());
        let mut missing_digest = release("v1.10.0");
        missing_digest.assets[1].digest = None;
        assert!(!parse_release(missing_digest, "1.0.0", InstallKind::Msi).unwrap().unwrap().can_install());
    }

    /// Talks to GitHub: `cargo test --lib live_release -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_release() {
        let response = agent(15).get(LATEST).set("Accept", "application/vnd.github+json").call().unwrap();
        let release: Release = serde_json::from_reader(response.into_reader()).unwrap();
        let update = parse_release(release, "0.9.0", InstallKind::Exe).unwrap().expect("a release newer than 0.9.0");
        let package = update.package.expect("an exe with a published digest");
        let response = asset_response(&agent(120), &package.url).unwrap();
        verify_download(response.into_reader(), std::io::sink(), package.size, &package.digest).unwrap();
        println!("verified {} ({} bytes) against {}", package.filename, package.size, package.digest);
    }

    #[test]
    fn rejects_corrupt_short_and_oversized_downloads() {
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let mut file = Vec::new();
        verify_download(&b"abc"[..], &mut file, 3, expected).unwrap();
        assert_eq!(file, b"abc");
        for bytes in [&b"bad"[..], &b"ab"[..], &b"abcd"[..]] {
            assert!(verify_download(bytes, std::io::sink(), 3, expected).is_err());
        }
    }
}
