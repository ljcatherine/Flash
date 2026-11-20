//! Updates to Flash itself, independent of the driver update sources.
use base64::Engine;
use ring::digest::{Context, SHA256};
use serde::Deserialize;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::windows::{ffi::OsStringExt, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tempfile::TempDir;
use windows_sys::Win32::{
    System::{
        Com::CoTaskMemFree,
        Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY, RegGetValueW},
        SystemInformation::GetSystemDirectoryW,
    },
    UI::Shell::{FOLDERID_ProgramFiles, SHGetKnownFolderPath},
};
use windows_sys::w;

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
}

impl Available {
    pub fn can_install(&self) -> bool {
        self.package.is_some()
    }
}

#[derive(Clone, Debug)]
struct Package {
    url: String,
    filename: String,
    size: u64,
    digest: String,
    installed: bool,
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
    installed: bool,
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
    let extension = if installed { "msi" } else { "exe" };
    let filename = format!("Flash-{published}-x64.{extension}");
    let package = release.assets.into_iter().find_map(|asset| {
        if asset.name != filename
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
            installed,
        })
    });
    Ok(Some(Available {
        version: published.into(),
        notes: format!("{REPOSITORY}/releases/tag/{tag}"),
        package,
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
    parse_release(
        release,
        env!("CARGO_PKG_VERSION"),
        installed_path(&exe).is_some(),
    )
}

fn installed_path(exe: &Path) -> Option<PathBuf> {
    let mut value = vec![0_u16; 32768];
    let mut size = (value.len() * size_of::<u16>()) as u32;
    // The per-machine MSI writes this value in the 64-bit registry view.
    let result = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("Software\\Flash"),
            w!("InstalledExe"),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            std::ptr::null_mut(),
            value.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if result != 0 || size < 2 {
        return None;
    }
    let registered = OsString::from_wide(&value[..size as usize / 2 - 1]);
    let registered = PathBuf::from(registered);
    (fs::canonicalize(&registered).ok()? == fs::canonicalize(exe).ok()?).then_some(registered)
}

fn program_files() -> std::io::Result<PathBuf> {
    let mut path = std::ptr::null_mut();
    // Windows allocates the returned NUL-terminated path with CoTaskMemAlloc.
    let result =
        unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFiles, 0, std::ptr::null_mut(), &mut path) };
    if result < 0 || path.is_null() {
        return Err(std::io::Error::other(
            "Windows could not locate Program Files.",
        ));
    }
    let folder = unsafe {
        let len = (0..).find(|&index| *path.add(index) == 0).unwrap();
        let folder = OsString::from_wide(std::slice::from_raw_parts(path, len));
        CoTaskMemFree(path.cast());
        folder
    };
    Ok(folder.into())
}

fn system_directory() -> std::io::Result<PathBuf> {
    let mut path = [0_u16; 260];
    let len = unsafe { GetSystemDirectoryW(path.as_mut_ptr(), path.len() as u32) } as usize;
    if len == 0 || len >= path.len() {
        return Err(std::io::Error::other(
            "Windows could not locate its system directory.",
        ));
    }
    Ok(PathBuf::from(OsString::from_wide(&path[..len])))
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
    let installed = installed_path(&exe);
    if package.installed != installed.is_some() {
        return Err("The installation changed. Check for updates again.".into());
    }
    let exe = installed.unwrap_or(exe);
    let response = asset_response(&agent(120), &package.url)?;
    // Flash already runs elevated. Keep the payload under Program Files so a
    // standard user cannot replace it between verification and installation.
    let directory = tempfile::Builder::new()
        .prefix("Flash-update-")
        .tempdir_in(program_files().map_err(|err| err.to_string())?)
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
    pub fn apply(self) -> std::io::Result<()> {
        let system = system_directory()?;
        let powershell = system.join(r"WindowsPowerShell\v1.0\powershell.exe");
        let script: Vec<u8> = include_str!("apply_update.ps1")
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        // Only fixed source code goes on the command line. Paths and release
        // data are passed as values, never interpolated into the script.
        Command::new(powershell)
            .args(["-NoProfile", "-NonInteractive", "-EncodedCommand"])
            .arg(base64::engine::general_purpose::STANDARD.encode(script))
            .env(
                "PSModulePath",
                system.join(r"WindowsPowerShell\v1.0\Modules"),
            )
            .env("FLASH_UPDATE_EXE", &self.exe)
            .env("FLASH_UPDATE_PATH", &self.path)
            .env("FLASH_UPDATE_HASH", &self.package.digest)
            .env("FLASH_UPDATE_VERSION", env!("CARGO_PKG_VERSION"))
            .env(
                "FLASH_UPDATE_KIND",
                if self.package.installed { "msi" } else { "exe" },
            )
            .env("FLASH_UPDATE_PARENT", std::process::id().to_string())
            .env("FLASH_UPDATE_SYSTEM", &system)
            .current_dir(system)
            .creation_flags(crate::NO_WINDOW)
            .spawn()?;
        // The helper owns cleanup after this process closes.
        let _ = self.directory.keep();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str) -> Release {
        let assets = ["exe", "msi"].map(|ext| {
            serde_json::json!({
                "name": format!("Flash-1.10.0-x64.{ext}"),
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
        for (installed, ext) in [(true, "msi"), (false, "exe")] {
            let update = parse_release(release("v1.10.0"), "1.9.9", installed)
                .unwrap()
                .unwrap();
            assert_eq!(
                update.package.unwrap().filename,
                format!("Flash-1.10.0-x64.{ext}")
            );
        }
        assert!(
            parse_release(release("v1.10.0"), "1.10.0", true)
                .unwrap()
                .is_none()
        );
        assert!(
            parse_release(release("v1.10.0"), "2.0.0", true)
                .unwrap()
                .is_none()
        );
        assert!(parse_release(release("v1.10.0/other"), "1.9.9", true).is_err());
    }

    #[test]
    fn skips_prereleases_and_requires_a_digest_for_in_app_install() {
        let mut preview = release("v1.10.0");
        preview.prerelease = true;
        assert!(parse_release(preview, "1.0.0", true).unwrap().is_none());
        let mut missing_digest = release("v1.10.0");
        missing_digest.assets[1].digest = None;
        assert!(
            !parse_release(missing_digest, "1.0.0", true)
                .unwrap()
                .unwrap()
                .can_install()
        );
    }

    /// Talks to GitHub: `cargo test --lib live_release -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_release() {
        let response = agent(15).get(LATEST).set("Accept", "application/vnd.github+json").call().unwrap();
        let release: Release = serde_json::from_reader(response.into_reader()).unwrap();
        let update = parse_release(release, "0.9.0", false).unwrap().expect("a release newer than 0.9.0");
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
