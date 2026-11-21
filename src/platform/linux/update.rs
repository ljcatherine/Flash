//! Updating Flash itself on Linux. The AppImage and the tarball's binary replace themselves;
//! the .deb and .rpm go through the helper, which checks the digest again as root; Nix,
//! Flatpak and distribution packages update with their package manager.
use super::{cmd, helper_path};
use crate::platform::InstallKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Whether the installed .deb/.rpm is Flash's own release package (not a distribution's).
fn ours(url_or_homepage: &str) -> bool {
    url_or_homepage.contains("flash.liamjc.dev") || url_or_homepage.contains("github.com/ljcatherine/Flash")
}

pub fn install_kind(exe: &Path) -> InstallKind {
    if let Some(app) = std::env::var_os("APPIMAGE").map(PathBuf::from).filter(|p| p.is_file()) {
        return InstallKind::AppImage(app);
    }
    if exe.starts_with("/nix/store") {
        return InstallKind::Managed("Flash was installed with Nix: update it by updating your flake input (or channel), then rebuild.");
    }
    if std::env::var_os("FLATPAK_ID").is_some() || Path::new("/.flatpak-info").exists() {
        return InstallKind::Managed("Update Flash with Flatpak (your software center, or flatpak update).");
    }
    let exe_str = exe.to_string_lossy();
    if exe_str.starts_with("/usr/") {
        if let Ok(home) = cmd("dpkg-query", &["-W", "-f=${Status}\t${Homepage}", "flash"]) {
            if home.contains("install ok installed") {
                return if ours(&home) { InstallKind::Deb } else { InstallKind::Managed("Flash came from your distribution: update it with your package manager.") };
            }
        }
        if let Ok(url) = cmd("rpm", &["-qf", "--qf", "%{URL}", &exe_str]) {
            return if ours(&url) { InstallKind::Rpm } else { InstallKind::Managed("Flash came from your distribution: update it with your package manager.") };
        }
        return InstallKind::Managed("Flash is installed in a system folder: update it the way you installed it.");
    }
    // The tarball: replace in place when you can write to its folder.
    let writable = exe.parent().is_some_and(|d| std::fs::metadata(d).is_ok_and(|m| !m.permissions().readonly()) && unsafe { libc::access(path_c(d).as_ptr(), libc::W_OK) } == 0);
    if writable { InstallKind::Tarball(exe.to_path_buf()) } else { InstallKind::Managed("Flash's folder isn't writable: update it the way you installed it.") }
}

fn path_c(p: &Path) -> std::ffi::CString {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap_or_default()
}

/// The running exe is the one an update replaces (the AppImage path comes with its kind).
pub fn installed_exe(exe: &Path) -> PathBuf {
    exe.to_path_buf()
}

/// A private folder for the download: $XDG_RUNTIME_DIR/flash (only you can read it).
pub fn update_dir() -> std::io::Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_dir()).unwrap_or_else(std::env::temp_dir);
    let dir = base.join("flash");
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// Copies `from` next to `target` and renames it over `target` (a running binary can be
/// replaced this way; the old one stays in memory until Flash closes).
fn replace(from: &Path, target: &Path) -> std::io::Result<()> {
    let dir = target.parent().ok_or_else(|| std::io::Error::other("no folder"))?;
    let name = target.file_name().ok_or_else(|| std::io::Error::other("no file name"))?;
    let tmp = dir.join(format!(".{}.new", name.to_string_lossy()));
    std::fs::copy(from, &tmp)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, target)
}

/// Installs the verified download. Flash closes afterwards; open it again to use the new version.
pub fn apply_update(_exe: &Path, path: &Path, digest: &str, kind: &InstallKind) -> std::io::Result<()> {
    match kind {
        InstallKind::AppImage(target) => replace(path, target),
        InstallKind::Tarball(target) => {
            // flash-<version>-<arch>/flash and flash-helper inside the archive.
            let out = path.with_extension("extracted");
            std::fs::create_dir_all(&out)?;
            let status = Command::new("tar").arg("-xzf").arg(path).arg("-C").arg(&out).arg("--no-same-owner").arg("--no-same-permissions").status()?;
            if !status.success() {
                return Err(std::io::Error::other("couldn't unpack the update"));
            }
            let top = std::fs::read_dir(&out)?.flatten().map(|e| e.path()).find(|p| p.is_dir()).ok_or_else(|| std::io::Error::other("unexpected archive layout"))?;
            let bin = top.join("flash");
            if !std::fs::symlink_metadata(&bin).is_ok_and(|m| m.is_file()) {
                return Err(std::io::Error::other("the archive has no flash binary"));
            }
            replace(&bin, target)?;
            let helper = top.join("flash-helper");
            let beside = target.with_file_name("flash-helper");
            if std::fs::symlink_metadata(&helper).is_ok_and(|m| m.is_file()) && beside.exists() {
                replace(&helper, &beside)?;
            }
            std::fs::remove_dir_all(&out).ok();
            Ok(())
        }
        InstallKind::Deb | InstallKind::Rpm => {
            // The helper copies the file somewhere root-only, checks the digest and that it's
            // the flash package, then installs it. It keeps running after Flash closes.
            let helper = helper_path().map_err(std::io::Error::other)?;
            Command::new("pkexec")
                .arg(helper)
                .args(["manage", "install-self"])
                .arg(path)
                .arg(digest)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            Ok(())
        }
        _ => Err(std::io::Error::other("Flash can't update this installation itself")),
    }
}
