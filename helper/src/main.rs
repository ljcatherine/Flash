//! flash-helper: the only part of Flash for Linux that runs as root.
//!
//! Flash itself runs as the user. For each privileged action it starts
//! `pkexec flash-helper <group> <verb> [args...]`, and polkit decides from the group
//! (`read`, `logs` or `manage`, see packaging/linux/dev.liamjc.flash.policy) whether to
//! ask for a password. This program then:
//!
//! - accepts only the fixed verbs below, and checks every argument against a strict
//!   pattern before using it;
//! - runs only fixed programs from fixed system directories, with arguments passed as
//!   argv, never through a shell, and with a clean environment;
//! - copies user-supplied package files into a root-owned directory before installing
//!   them, so they can't be swapped out halfway.
//!
//! Firmware isn't handled here: fwupd checks its own polkit rules, so Flash runs
//! `fwupdmgr` as the user.
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

const SAFE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/run/current-system/sw/bin";
const TOOL_DIRS: [&str; 7] = ["/usr/sbin", "/usr/bin", "/sbin", "/bin", "/usr/local/sbin", "/usr/local/bin", "/run/current-system/sw/bin"];
const SNAPSHOT_NOTE: &str = "Flash driver update";

fn main() -> ExitCode {
    let mut args = Vec::new();
    for a in std::env::args_os().skip(1) {
        match a.into_string() {
            Ok(s) => args.push(s),
            Err(_) => {
                eprintln!("flash-helper: arguments must be UTF-8");
                return ExitCode::from(2);
            }
        }
    }
    if !is_root() {
        eprintln!("flash-helper: must run as root (Flash starts it with pkexec)");
        return ExitCode::from(2);
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match run(&args) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{}", e.trim_end());
            ExitCode::FAILURE
        }
    }
}

fn is_root() -> bool {
    // The effective UID is the second field of the Uid: line.
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("Uid:")).and_then(|l| l.split_whitespace().nth(2)).map(|u| u == "0"))
        .unwrap_or(false)
}

type Res = Result<String, String>;

fn run(args: &[&str]) -> Res {
    match args {
        // ---- read: safe to allow for the active local user without a password ----
        ["read", "smart", dev] => smartctl(dev),
        ["read", "dmi-memory"] => exec("dmidecode", &["-t", "17"], &[]),
        // Only file times and a "was it a panic" flag, never the crash log's contents.
        ["read", "pstore"] => Ok(pstore()),
        // ---- logs: system logs, which can hold private data ----
        ["logs", "kernel-log", days] => {
            let since = format!("-{}d", number(days, 1, 366)?);
            exec("journalctl", &["-k", "--since", &since, "-o", "short-iso", "--no-pager", "-q"], &[])
        }
        ["logs", "boots", days] => boots(number(days, 1, 366)?),
        ["logs", "ras-errors"] => exec("ras-mc-ctl", &["--errors"], &[]),
        // ---- manage: changes the system, always asks for an administrator ----
        ["manage", "pkg-upgrade", names @ ..] => pkg_upgrade(names),
        ["manage", "pkg-install-file", path] => pkg_install_file(path, None),
        ["manage", "pkg-downgrade", pairs @ ..] => pkg_downgrade(pairs),
        ["manage", "snapshot"] => snapshot(),
        ["manage", "module-prefer", new, old] => module_prefer(module(new)?, module(old)?),
        ["manage", "module-unprefer", new] => module_unprefer(module(new)?),
        ["manage", "module-load", name] => module_load(module(name)?),
        ["manage", "module-unload", name] => module_unload(module(name)?),
        ["manage", "memtest-next-boot"] => memtest_next_boot(),
        ["manage", "clean", what] => clean(what),
        ["manage", "remove-kernel", version] => remove_kernel(version),
        ["manage", "install-self", path, sha256] => pkg_install_file(path, Some(sha256)),
        _ => Err(format!("flash-helper: unknown or malformed request: {}", args.join(" "))),
    }
}

// ---------------- validation ----------------

fn number(s: &str, min: u32, max: u32) -> Result<u32, String> {
    s.parse::<u32>().ok().filter(|n| (min..=max).contains(n)).ok_or_else(|| format!("bad number: {s}"))
}

/// Debian, RPM, Arch and openSUSE package names: letters, digits and `+-._@`, not starting with `-`.
fn pkg_name(s: &str) -> Result<&str, String> {
    let ok = !s.is_empty() && s.len() <= 128 && !s.starts_with(['-', '.']) && s.chars().all(|c| c.is_ascii_alphanumeric() || "+-._@".contains(c));
    if ok { Ok(s) } else { Err(format!("bad package name: {s}")) }
}

/// Package versions, including Debian epochs (`1:2.3-1ubuntu1`) and RPM releases (`24.2.4-1.fc41`).
fn pkg_version(s: &str) -> Result<&str, String> {
    let ok = !s.is_empty() && s.len() <= 128 && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_alphanumeric() || "+-._:~".contains(c));
    if ok { Ok(s) } else { Err(format!("bad package version: {s}")) }
}

fn module(s: &str) -> Result<&str, String> {
    let ok = !s.is_empty() && s.len() <= 64 && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok { Ok(s) } else { Err(format!("bad module name: {s}")) }
}

fn block_device(s: &str) -> Result<&str, String> {
    let name = s.strip_prefix("/dev/").ok_or_else(|| format!("bad device: {s}"))?;
    let ok = !name.is_empty() && name.len() <= 32 && name.chars().all(|c| c.is_ascii_alphanumeric()) && ["nvme", "sd", "hd", "vd", "xvd", "mmcblk"].iter().any(|p| name.starts_with(p));
    if !ok {
        return Err(format!("bad device: {s}"));
    }
    let meta = fs::metadata(s).map_err(|e| format!("{s}: {e}"))?;
    use std::os::unix::fs::FileTypeExt;
    if meta.file_type().is_block_device() || meta.file_type().is_char_device() { Ok(s) } else { Err(format!("bad device: {s}")) }
}

fn sha256_hex(s: &str) -> Result<String, String> {
    if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) { Ok(s.to_ascii_lowercase()) } else { Err("bad SHA-256 digest".into()) }
}

fn path_str(p: &Path) -> Result<&str, String> {
    p.to_str().ok_or_else(|| "path is not UTF-8".to_string())
}

// ---------------- running programs ----------------

fn tool(name: &str) -> Option<PathBuf> {
    TOOL_DIRS.iter().map(|d| Path::new(d).join(name)).find(|p| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// Runs a system tool with a clean environment and returns its stdout and stderr.
fn exec(name: &str, args: &[&str], env: &[(&str, &str)]) -> Res {
    let path = tool(name).ok_or_else(|| format!("{name} isn't installed"))?;
    let out = Command::new(&path)
        .args(args)
        .env_clear()
        .env("PATH", SAFE_PATH)
        .env("LANG", "C.UTF-8")
        .env("HOME", "/root")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot start {name}: {e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() { Ok(text) } else { Err(if text.trim().is_empty() { format!("{name} failed ({})", out.status) } else { text }) }
}

// ---------------- distribution ----------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum Pm {
    Apt,
    Dnf,
    Pacman,
    Zypper,
}

fn os_release() -> BTreeMap<String, String> {
    let text = fs::read_to_string("/etc/os-release").or_else(|_| fs::read_to_string("/usr/lib/os-release")).unwrap_or_default();
    text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.trim_matches('"').to_string())).collect()
}

fn package_manager() -> Result<Pm, String> {
    if Path::new("/etc/NIXOS").exists() {
        return Err("NixOS manages packages declaratively: update with nixos-rebuild instead.".into());
    }
    if Path::new("/run/ostree-booted").exists() {
        return Err("This is an image-based system: update with your system's updater (rpm-ostree or bootc) instead.".into());
    }
    let os = os_release();
    let ids = format!("{} {}", os.get("ID").map_or("", String::as_str), os.get("ID_LIKE").map_or("", String::as_str));
    let has = |t: &str| tool(t).is_some();
    let pick = |family: &str| ids.split_whitespace().any(|i| i == family);
    if (pick("debian") || pick("ubuntu")) && has("apt-get") {
        Ok(Pm::Apt)
    } else if (pick("fedora") || pick("rhel") || pick("centos")) && has("dnf") {
        Ok(Pm::Dnf)
    } else if pick("arch") && has("pacman") {
        Ok(Pm::Pacman)
    } else if (pick("suse") || pick("opensuse")) && has("zypper") {
        Ok(Pm::Zypper)
    } else if has("apt-get") {
        Ok(Pm::Apt)
    } else if has("dnf") {
        Ok(Pm::Dnf)
    } else if has("pacman") {
        Ok(Pm::Pacman)
    } else if has("zypper") {
        Ok(Pm::Zypper)
    } else {
        Err("No supported package manager (apt, dnf, pacman or zypper) found.".into())
    }
}

const APT_ENV: [(&str, &str); 1] = [("DEBIAN_FRONTEND", "noninteractive")];

fn pkg_upgrade(names: &[&str]) -> Res {
    if names.is_empty() || names.len() > 64 {
        return Err("no packages given".into());
    }
    let names: Vec<&str> = names.iter().map(|n| pkg_name(n)).collect::<Result<_, _>>()?;
    match package_manager()? {
        Pm::Apt => {
            let mut log = exec("apt-get", &["update", "-q"], &APT_ENV)?;
            let mut a = vec!["install", "-y", "-q", "--only-upgrade", "-o", "Dpkg::Options::=--force-confold"];
            a.extend(&names);
            log += &exec("apt-get", &a, &APT_ENV)?;
            Ok(log)
        }
        Pm::Dnf => {
            let mut a = vec!["upgrade", "-y"];
            a.extend(&names);
            exec("dnf", &a, &[])
        }
        // Arch doesn't support partial upgrades: upgrading one package against a newer
        // repository can break the system, so the only safe choice is a full upgrade.
        Pm::Pacman => exec("pacman", &["-Syu", "--noconfirm"], &[]).map(|o| format!("Arch Linux doesn't support partial upgrades, so this ran a full system upgrade.\n{o}")),
        Pm::Zypper => {
            let mut a = vec!["--non-interactive", "update"];
            a.extend(&names);
            exec("zypper", &a, &[])
        }
    }
}

/// Downgrades packages together: each argument is `name=version`.
fn pkg_downgrade(pairs: &[&str]) -> Res {
    if pairs.is_empty() || pairs.len() > 64 {
        return Err("no packages given".into());
    }
    let pairs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|p| p.split_once('=').ok_or_else(|| format!("expected name=version: {p}")).and_then(|(n, v)| Ok((pkg_name(n)?, pkg_version(v)?))))
        .collect::<Result<_, _>>()?;
    match package_manager()? {
        Pm::Apt => {
            let specs: Vec<String> = pairs.iter().map(|(n, v)| format!("{n}={v}")).collect();
            let mut a = vec!["install", "-y", "-q", "--allow-downgrades"];
            a.extend(specs.iter().map(String::as_str));
            exec("apt-get", &a, &APT_ENV)
        }
        Pm::Dnf => {
            let specs: Vec<String> = pairs.iter().map(|(n, v)| format!("{n}-{v}")).collect();
            let mut a = vec!["downgrade", "-y"];
            a.extend(specs.iter().map(String::as_str));
            exec("dnf", &a, &[])
        }
        Pm::Zypper => {
            let specs: Vec<String> = pairs.iter().map(|(n, v)| format!("{n}={v}")).collect();
            let mut a = vec!["--non-interactive", "install", "--oldpackage"];
            a.extend(specs.iter().map(String::as_str));
            exec("zypper", &a, &[])
        }
        Pm::Pacman => {
            // pacman keeps old packages in its cache; reinstall the exact files from there.
            let cache: Vec<PathBuf> = fs::read_dir("/var/cache/pacman/pkg").map_err(|e| format!("pacman cache: {e}"))?.flatten().map(|e| e.path()).collect();
            let mut files = Vec::new();
            for (name, version) in &pairs {
                let prefix = format!("{name}-{version}-");
                let file = cache
                    .iter()
                    .find(|p| p.file_name().and_then(|f| f.to_str()).is_some_and(|f| pacman_cache_match(f, &prefix)))
                    .ok_or_else(|| format!("{name} {version} isn't in pacman's package cache any more"))?;
                files.push(path_str(file)?.to_string());
            }
            let mut a = vec!["-U", "--noconfirm"];
            a.extend(files.iter().map(String::as_str));
            exec("pacman", &a, &[])
        }
    }
}

/// `name-version-` followed by exactly "release-arch.pkg.tar.*" (release and arch have no dashes).
fn pacman_cache_match(file: &str, prefix: &str) -> bool {
    file.strip_prefix(prefix).is_some_and(|rest| rest.matches('-').count() == 1 && rest.contains(".pkg.tar.") && !rest.ends_with(".sig"))
}

/// Copies a user's file into a fresh root-only directory and returns (directory, copy).
/// Refuses links and anything that isn't a plain file with one of `exts`.
fn stage(path: &str, exts: &[&str]) -> Result<(TempDir, PathBuf), String> {
    let p = Path::new(path);
    let name = p.file_name().and_then(|f| f.to_str()).ok_or("bad file name")?;
    if !p.is_absolute() || path.contains("/../") || path.contains("/./") || !exts.iter().any(|e| name.ends_with(e)) || !name.chars().all(|c| c.is_ascii_alphanumeric() || "+-._~".contains(c)) {
        return Err(format!("refused: {path}"));
    }
    let meta = fs::symlink_metadata(p).map_err(|e| format!("{path}: {e}"))?;
    if !meta.file_type().is_file() || meta.len() > 4 << 30 {
        return Err(format!("refused: {path} isn't a regular file"));
    }
    let dir = TempDir::new()?;
    let copy = dir.0.join(name);
    let mut src = fs::File::open(p).map_err(|e| format!("{path}: {e}"))?;
    // Check the file we opened is the one we inspected (same inode), then copy from that handle.
    let opened = src.metadata().map_err(|e| e.to_string())?;
    if opened.ino() != meta.ino() || opened.dev() != meta.dev() {
        return Err(format!("refused: {path} changed while it was being checked"));
    }
    let mut dst = fs::OpenOptions::new().write(true).create_new(true).open(&copy).map_err(|e| e.to_string())?;
    std::io::copy(&mut src, &mut dst).map_err(|e| format!("copying {path}: {e}"))?;
    fs::set_permissions(&copy, fs::Permissions::from_mode(0o644)).map_err(|e| e.to_string())?;
    Ok((dir, copy))
}

/// A root-owned directory under /run (or /tmp), removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<TempDir, String> {
        let base = if Path::new("/run").is_dir() { "/run" } else { "/tmp" };
        for i in 0..100u32 {
            let p = Path::new(base).join(format!("flash-helper-{}-{i}", std::process::id()));
            let mut b = fs::DirBuilder::new();
            std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
            if b.create(&p).is_ok() {
                return Ok(TempDir(p));
            }
        }
        Err("cannot create a private directory".into())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn file_sha256(p: &Path) -> Result<String, String> {
    let out = exec("sha256sum", &["--", path_str(p)?], &[])?;
    out.split_whitespace().next().map(str::to_ascii_lowercase).ok_or_else(|| "sha256sum gave no output".into())
}

/// Installs a local package file. With a digest it's Flash updating itself: the file must
/// match the digest and be the `flash` package.
fn pkg_install_file(path: &str, sha256: Option<&str>) -> Res {
    let pm = package_manager()?;
    let exts: &[&str] = match pm {
        Pm::Apt => &[".deb"],
        Pm::Dnf | Pm::Zypper => &[".rpm"],
        Pm::Pacman => &[".pkg.tar.zst", ".pkg.tar.xz"],
    };
    let (_dir, copy) = stage(path, exts)?;
    if let Some(want) = sha256 {
        if file_sha256(&copy)? != sha256_hex(want)? {
            return Err("The update package doesn't match its published SHA-256 digest.".into());
        }
        let name = match pm {
            Pm::Apt => exec("dpkg-deb", &["-f", path_str(&copy)?, "Package"], &[])?,
            Pm::Dnf | Pm::Zypper => exec("rpm", &["-qp", "--qf", "%{NAME}", path_str(&copy)?], &[])?,
            Pm::Pacman => return Err("Flash isn't packaged for pacman; update the AppImage or your AUR package instead.".into()),
        };
        if name.trim() != "flash" {
            return Err("The update package isn't Flash.".into());
        }
    }
    let file = path_str(&copy)?;
    match pm {
        Pm::Apt => exec("apt-get", &["install", "-y", "-q", "-o", "Dpkg::Options::=--force-confold", file], &APT_ENV),
        Pm::Dnf => exec("dnf", &["install", "-y", file], &[]),
        Pm::Zypper => exec("zypper", &["--non-interactive", "install", "--allow-unsigned-rpm", file], &[]),
        Pm::Pacman => exec("pacman", &["-U", "--noconfirm", file], &[]),
    }
}

// ---------------- snapshots ----------------

fn snapshot() -> Res {
    if tool("snapper").is_some() && exec("snapper", &["-c", "root", "list-configs"], &[]).is_ok_and(|o| o.lines().any(|l| l.split('|').next().is_some_and(|c| c.trim() == "root"))) {
        return exec("snapper", &["-c", "root", "create", "--description", SNAPSHOT_NOTE, "--cleanup-algorithm", "number", "--print-number"], &[]).map(|n| format!("Snapper snapshot {} created.\n", n.trim()));
    }
    if tool("timeshift").is_some() {
        return exec("timeshift", &["--create", "--comments", SNAPSHOT_NOTE, "--scripted"], &[]);
    }
    Err("No snapshot tool is set up: install Snapper (with a root config) or Timeshift.".into())
}

// ---------------- kernel modules ----------------

fn modprobe_conf(new: &str) -> PathBuf {
    PathBuf::from(format!("/etc/modprobe.d/flash-prefer-{new}.conf"))
}

fn regenerate_initramfs() -> Res {
    if tool("update-initramfs").is_some() {
        exec("update-initramfs", &["-u"], &[])
    } else if tool("dracut").is_some() {
        exec("dracut", &["--force", "--regenerate-all"], &[])
    } else if tool("mkinitcpio").is_some() {
        exec("mkinitcpio", &["-P"], &[])
    } else {
        Ok("No initramfs tool found; nothing to regenerate.\n".into())
    }
}

/// Makes `new` the driver for a device by blocking `old`, the module that took it.
fn module_prefer(new: &str, old: &str) -> Res {
    if Path::new("/etc/NIXOS").exists() {
        return Err(format!("On NixOS, add \"{old}\" to boot.blacklistedKernelModules and \"{new}\" to boot.kernelModules in configuration.nix, then run nixos-rebuild."));
    }
    if new == old {
        return Err("the two modules are the same".into());
    }
    // Both must be real modules for this kernel.
    exec("modinfo", &["-n", new], &[])?;
    exec("modinfo", &["-n", old], &[])?;
    let conf = modprobe_conf(new);
    let text = format!("# Written by Flash: use {new} instead of {old}. Delete this file (or use Undo in Flash) to switch back.\nblacklist {old}\n");
    fs::write(&conf, text).map_err(|e| format!("{}: {e}", conf.display()))?;
    let mut log = format!("Wrote {}.\n", conf.display());
    log += &regenerate_initramfs()?;
    log += "Restart the PC to switch drivers.\n";
    Ok(log)
}

fn module_unprefer(new: &str) -> Res {
    let conf = modprobe_conf(new);
    match fs::symlink_metadata(&conf) {
        Ok(m) if m.file_type().is_file() => fs::remove_file(&conf).map_err(|e| e.to_string())?,
        Ok(_) => return Err(format!("refused: {} isn't a regular file", conf.display())),
        Err(_) => return Ok(format!("{} was already removed.\n", conf.display())),
    }
    let mut log = format!("Removed {}.\n", conf.display());
    log += &regenerate_initramfs()?;
    log += "Restart the PC to switch back.\n";
    Ok(log)
}

/// Loads a module now and at every boot (for a device that has no driver).
fn module_load(name: &str) -> Res {
    if Path::new("/etc/NIXOS").exists() {
        return Err(format!("On NixOS, add \"{name}\" to boot.kernelModules in configuration.nix, then run nixos-rebuild."));
    }
    exec("modinfo", &["-n", name], &[])?;
    let conf = PathBuf::from(format!("/etc/modules-load.d/flash-{name}.conf"));
    let mut log = exec("modprobe", &[name], &[])?;
    fs::write(&conf, format!("# Written by Flash. Delete this file (or use Undo in Flash) to stop loading it at boot.\n{name}\n")).map_err(|e| format!("{}: {e}", conf.display()))?;
    log += &format!("Loaded {name} and added {}.\n", conf.display());
    Ok(log)
}

fn module_unload(name: &str) -> Res {
    let conf = PathBuf::from(format!("/etc/modules-load.d/flash-{name}.conf"));
    if fs::symlink_metadata(&conf).is_ok_and(|m| m.file_type().is_file()) {
        fs::remove_file(&conf).map_err(|e| e.to_string())?;
    }
    // Unloading fails while the device is busy; it's gone after the next restart either way.
    let log = exec("modprobe", &["-r", name], &[]).unwrap_or_else(|e| format!("{e}Restart the PC to finish.\n"));
    Ok(format!("Removed {}.\n{log}", conf.display()))
}

// ---------------- memory test ----------------

/// Boots into memtest86+ once, at the next restart.
fn memtest_next_boot() -> Res {
    // systemd-boot
    if tool("bootctl").is_some() {
        if let Ok(list) = exec("bootctl", &["list", "--no-pager"], &[]) {
            let id = list.lines().filter_map(|l| l.trim().strip_prefix("id: ")).map(str::trim).find(|id| id.to_lowercase().contains("memtest"));
            if let Some(id) = id.filter(|id| id.chars().all(|c| c.is_ascii_alphanumeric() || "._-+@".contains(c))) {
                exec("bootctl", &["set-oneshot", id], &[])?;
                return Ok(format!("The PC will start memtest86+ ({id}) at the next restart.\n"));
            }
        }
    }
    // GRUB: the entry title comes from the root-owned grub.cfg.
    for (cfg, reboot) in [("/boot/grub/grub.cfg", "grub-reboot"), ("/boot/grub2/grub.cfg", "grub2-reboot")] {
        let Ok(text) = fs::read_to_string(cfg) else { continue };
        if let Some(title) = grub_memtest_entry(&text) {
            if tool(reboot).is_some() {
                exec(reboot, &[&title], &[])?;
                return Ok(format!("The PC will start \"{title}\" at the next restart.\n"));
            }
        }
    }
    Err("memtest86+ isn't in the boot menu. Install the memtest86+ package (it adds itself to GRUB or systemd-boot), then try again.".into())
}

fn grub_memtest_entry(cfg: &str) -> Option<String> {
    cfg.lines().map(str::trim).filter_map(|l| l.strip_prefix("menuentry ")).find_map(|q| {
        let q = q.trim_start();
        let quote = q.chars().next().filter(|c| *c == '\'' || *c == '"')?;
        let title = q[1..].split(quote).next()?;
        (title.to_lowercase().contains("memtest") && !title.starts_with('-')).then(|| title.to_string())
    })
}

// ---------------- cleaning ----------------

fn clean(what: &str) -> Res {
    match what {
        "pkg-cache" => match package_manager()? {
            Pm::Apt => exec("apt-get", &["clean"], &APT_ENV),
            Pm::Dnf => exec("dnf", &["clean", "packages"], &[]),
            // Keep the newest cached copy of each package so a downgrade stays possible.
            Pm::Pacman if tool("paccache").is_some() => exec("paccache", &["-r", "-k", "1"], &[]),
            Pm::Pacman => exec("pacman", &["-Sc", "--noconfirm"], &[]),
            Pm::Zypper => exec("zypper", &["clean", "--all"], &[]),
        },
        "journal" => exec("journalctl", &["--vacuum-time=2weeks"], &[]),
        "coredumps" => {
            let mut freed = 0u64;
            for dir in ["/var/lib/systemd/coredump", "/var/crash"] {
                for e in fs::read_dir(dir).into_iter().flatten().flatten() {
                    // Plain files only, never links or directories.
                    if let Ok(m) = fs::symlink_metadata(e.path()) {
                        if m.file_type().is_file() && fs::remove_file(e.path()).is_ok() {
                            freed += m.len();
                        }
                    }
                }
            }
            Ok(format!("freed {freed}\n"))
        }
        "snap-disabled" => {
            let list = exec("snap", &["list", "--all"], &[])?;
            let mut log = String::new();
            for (name, rev) in disabled_snaps(&list) {
                log += &exec("snap", &["remove", name, &format!("--revision={rev}")], &[]).unwrap_or_else(|e| e);
            }
            Ok(log)
        }
        "flatpak-unused" => exec("flatpak", &["uninstall", "--system", "--unused", "-y", "--noninteractive"], &[]),
        _ => Err(format!("unknown clean target: {what}")),
    }
}

/// (name, revision) of every disabled snap in `snap list --all`.
fn disabled_snaps(list: &str) -> Vec<(&str, &str)> {
    list.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let ok = f.len() >= 6 && f.last().is_some_and(|n| n.split(',').any(|x| x == "disabled"));
            let valid = |s: &str| !s.is_empty() && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            (ok && valid(f[0]) && f[2].chars().all(|c| c.is_ascii_digit())).then(|| (f[0], f[2]))
        })
        .collect()
}

/// Removes one old kernel. Never the running kernel or the newest installed one.
fn remove_kernel(version: &str) -> Res {
    let version = pkg_version(version)?;
    let running = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    if running.trim() == version {
        return Err("refused: that's the kernel the PC is running".into());
    }
    let mut installed: Vec<String> = fs::read_dir("/lib/modules").into_iter().flatten().flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
    installed.sort_by(|a, b| cmp_versions(a, b));
    if installed.last().map(String::as_str) == Some(version) {
        return Err("refused: that's the newest installed kernel".into());
    }
    if !installed.iter().any(|k| k == version) {
        return Err(format!("kernel {version} isn't installed"));
    }
    match package_manager()? {
        Pm::Apt => {
            let candidates = ["linux-image-", "linux-image-unsigned-", "linux-modules-", "linux-modules-extra-", "linux-headers-"].map(|p| format!("{p}{version}"));
            let present: Vec<&str> = candidates.iter().map(String::as_str).filter(|p| exec("dpkg-query", &["-W", "-f=${Status}", p], &[]).is_ok_and(|s| s.contains("install ok installed"))).collect();
            if present.is_empty() {
                return Err(format!("no packages found for kernel {version}"));
            }
            let mut a = vec!["purge", "-y", "-q"];
            a.extend(present);
            exec("apt-get", &a, &APT_ENV)
        }
        Pm::Dnf => {
            let candidates = ["kernel-", "kernel-core-", "kernel-modules-", "kernel-modules-core-", "kernel-modules-extra-"].map(|p| format!("{p}{version}"));
            let present: Vec<&str> = candidates.iter().map(String::as_str).filter(|p| exec("rpm", &["-q", p], &[]).is_ok()).collect();
            if present.is_empty() {
                return Err(format!("no packages found for kernel {version}"));
            }
            let mut a = vec!["remove", "-y"];
            a.extend(present);
            exec("dnf", &a, &[])
        }
        _ => Err("Removing old kernels is only supported with apt and dnf here; use your package manager.".into()),
    }
}

/// Orders kernel versions like 6.8.0-45-generic numerically, part by part.
fn cmp_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |s: &str| s.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    parts(a).cmp(&parts(b))
}

// ---------------- read-only reports ----------------

fn smartctl(dev: &str) -> Res {
    let dev = block_device(dev)?;
    let path = tool("smartctl").ok_or("smartctl isn't installed (smartmontools)")?;
    // smartctl's exit code is a bit mask of drive conditions, so its JSON is the answer either way.
    let out = Command::new(path).args(["-j", "-a", dev]).env_clear().env("PATH", SAFE_PATH).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if text.trim_start().starts_with('{') { Ok(text) } else { Err(String::from_utf8_lossy(&out.stderr).into_owned()) }
}

/// Boot list plus the boots that ended with "Journal stopped" (a clean shutdown).
fn boots(days: u32) -> Res {
    let since = format!("-{days}d");
    let list = exec("journalctl", &["--list-boots", "-o", "json", "--no-pager", "-q"], &[])?;
    let stopped = exec("journalctl", &["--since", &since, "-o", "json", "--output-fields=_BOOT_ID", "--no-pager", "-q", "SYSLOG_IDENTIFIER=systemd-journald", "MESSAGE=Journal stopped"], &[]).unwrap_or_default();
    Ok(format!("{list}\n--STOPPED--\n{stopped}"))
}

/// Kernel crash logs saved by pstore: "unix-mtime<TAB>file name<TAB>1 if it's a panic" lines.
fn pstore() -> String {
    let mut out = String::new();
    for dir in ["/sys/fs/pstore", "/var/lib/systemd/pstore"] {
        let mut stack = vec![PathBuf::from(dir)];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).into_iter().flatten().flatten() {
                let Ok(m) = fs::symlink_metadata(e.path()) else { continue };
                if m.file_type().is_dir() && stack.len() < 64 {
                    stack.push(e.path());
                } else if m.file_type().is_file() {
                    let name = e.file_name().to_string_lossy().replace(['\t', '\n'], " ");
                    if name.starts_with("dmesg") {
                        let mut head = String::new();
                        fs::File::open(e.path()).and_then(|f| f.take(64 * 1024).read_to_string(&mut head)).ok();
                        let panic = head.contains("Kernel panic") || head.contains("Oops");
                        out += &format!("{}\t{}\t{}\n", m.mtime(), name, u8::from(panic));
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_arguments() {
        assert!(pkg_name("linux-firmware").is_ok());
        assert!(pkg_name("libgl1-mesa-dri").is_ok());
        assert!(pkg_name("nvidia-driver-550").is_ok());
        assert!(pkg_name("gcc-c++").is_ok());
        for bad in ["", "-o", "a;b", "a b", "../x", "x$(id)", ".hidden"] {
            assert!(pkg_name(bad).is_err(), "{bad}");
        }
        assert!(pkg_version("1:24.2.4-1ubuntu1").is_ok());
        assert!(pkg_version("24.2.4-1.fc41").is_ok());
        assert!(pkg_version("--allow").is_err() && pkg_version("1 2").is_err());
        assert!(module("nvidia_drm").is_ok() && module("r8125").is_ok() && module("x/y").is_err() && module("-r").is_err());
        assert!(number("90", 1, 366).is_ok() && number("0", 1, 366).is_err() && number("-1", 1, 366).is_err());
        assert!(sha256_hex(&"a".repeat(64)).is_ok() && sha256_hex("abc").is_err());
        assert!(block_device("/dev/../etc/passwd").is_err());
        assert!(block_device("/etc/passwd").is_err());
        assert!(pacman_cache_match("mesa-1:24.2.4-1-x86_64.pkg.tar.zst", "mesa-1:24.2.4-"));
        assert!(!pacman_cache_match("mesa-1:24.2.4-1-x86_64.pkg.tar.zst.sig", "mesa-1:24.2.4-"));
        assert!(!pacman_cache_match("mesa-utils-1:24.2.4-1-x86_64.pkg.tar.zst", "mesa-1:24.2.4-"));
    }

    #[test]
    fn refuses_unknown_requests() {
        assert!(run(&["manage", "rm", "-rf", "/"]).is_err());
        assert!(run(&["read", "smart"]).is_err());
        assert!(run(&["manage", "clean", "/etc"]).is_err());
        assert!(run(&["manage", "pkg-upgrade", "a;reboot"]).is_err());
        assert!(run(&["manage", "fw-update", "x"]).is_err()); // firmware goes through fwupd's own polkit rules
        assert!(run(&["manage", "pkg-downgrade", "mesa"]).is_err());
        assert!(run(&["manage", "pkg-downgrade", "mesa=--x"]).is_err());
        assert!(run(&["manage", "module-load", "../x"]).is_err());
        assert!(run(&[]).is_err());
    }

    #[test]
    fn stage_refuses_odd_paths() {
        assert!(stage("relative.deb", &[".deb"]).is_err());
        assert!(stage("/tmp/../etc/x.deb", &[".deb"]).is_err());
        assert!(stage("/etc/passwd", &[".deb"]).is_err());
        assert!(stage("/tmp/a b.deb", &[".deb"]).is_err());
        assert!(stage("/tmp/a;b.deb", &[".deb"]).is_err());
        // A Debian file name passes the name check (and then fails only because it doesn't exist).
        assert!(stage("/nonexistent/flash_1.2.0_amd64.deb", &[".deb"]).is_err_and(|e| e.contains("No such file")));
    }

    #[test]
    fn finds_memtest_and_disabled_snaps() {
        let grub = "menuentry 'Ubuntu' --class ubuntu {\n}\nmenuentry 'Memory test (memtest86+x64.efi)' {\n}\n";
        assert_eq!(grub_memtest_entry(grub).as_deref(), Some("Memory test (memtest86+x64.efi)"));
        assert_eq!(grub_memtest_entry("menuentry \"Fedora\" {}"), None);
        let snaps = "Name      Version   Rev    Tracking       Publisher   Notes\ncore22    20240111  1122   latest/stable  canonical✓  base,disabled\ncore22    20240408  1380   latest/stable  canonical✓  base\nfirefox   124.0     4090   latest/stable  mozilla✓    disabled\n";
        assert_eq!(disabled_snaps(snaps), vec![("core22", "1122"), ("firefox", "4090")]);
        assert_eq!(cmp_versions("6.8.0-45-generic", "6.8.0-100-generic"), std::cmp::Ordering::Less);
        assert_eq!(cmp_versions("6.11.4-301.fc41.x86_64", "6.11.10-300.fc41.x86_64"), std::cmp::Ordering::Less);
    }
}
