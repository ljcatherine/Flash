//! The distribution's package manager: which driver packages have updates, what's
//! installed, and how versions compare. Listing runs as the user; installing goes through
//! the helper.
use super::{cmd, cmd_any, has, is_nixos};
use std::cmp::Ordering;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pm {
    Apt,
    Dnf,
    Pacman,
    Zypper,
}

/// The system's package manager, or why packages can't be updated from Flash.
pub(crate) fn detect() -> Result<Pm, &'static str> {
    if is_nixos() {
        return Err("Managed by NixOS: update with nixos-rebuild");
    }
    if Path::new("/run/ostree-booted").exists() {
        return Err("Image-based system: update with rpm-ostree or bootc");
    }
    let os = super::os_release();
    let ids = format!("{} {}", os.get("ID").map_or("", String::as_str), os.get("ID_LIKE").map_or("", String::as_str));
    let family = |f: &str| ids.split_whitespace().any(|i| i == f);
    let found = if (family("debian") || family("ubuntu")) && has("apt-get") {
        Some(Pm::Apt)
    } else if (family("fedora") || family("rhel") || family("centos")) && has("dnf") {
        Some(Pm::Dnf)
    } else if family("arch") && has("pacman") {
        Some(Pm::Pacman)
    } else if (family("suse") || family("opensuse")) && has("zypper") {
        Some(Pm::Zypper)
    } else {
        [("apt-get", Pm::Apt), ("dnf", Pm::Dnf), ("pacman", Pm::Pacman), ("zypper", Pm::Zypper)].into_iter().find(|(t, _)| has(t)).map(|(_, p)| p)
    };
    found.ok_or("No supported package manager found")
}

/// A package with a newer version available.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Upgrade {
    pub name: String,
    pub old: String,
    pub new: String,
}

/// `apt list --upgradable`: "name/suites version arch [upgradable from: old]".
pub(crate) fn parse_apt(text: &str) -> Vec<Upgrade> {
    text.lines()
        .filter_map(|l| {
            let (name, rest) = l.split_once('/')?;
            let mut f = rest.split_whitespace();
            let _suites = f.next()?;
            let new = f.next()?.to_string();
            let old = l.split_once("[upgradable from: ")?.1.trim_end_matches(']').trim().to_string();
            Some(Upgrade { name: name.to_string(), old, new })
        })
        .collect()
}

/// `dnf check-update` (dnf4 and dnf5): "name.arch version repo" after the metadata lines.
pub(crate) fn parse_dnf(text: &str) -> Vec<Upgrade> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() != 3 || l.starts_with(' ') || !f[1].chars().next().is_some_and(|c| c.is_ascii_digit()) {
                return None;
            }
            let (name, arch) = f[0].rsplit_once('.')?;
            (!name.is_empty() && !arch.is_empty() && !arch.contains('%')).then(|| Upgrade { name: name.to_string(), old: String::new(), new: f[1].to_string() })
        })
        .collect()
}

/// `pacman -Qu` and `checkupdates`: "name old -> new".
pub(crate) fn parse_pacman(text: &str) -> Vec<Upgrade> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() >= 4 && f[2] == "->").then(|| Upgrade { name: f[0].to_string(), old: f[1].to_string(), new: f[3].to_string() })
        })
        .collect()
}

/// `zypper --quiet list-updates`: "S | Repository | Name | Current Version | Available Version | Arch".
pub(crate) fn parse_zypper(text: &str) -> Vec<Upgrade> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('|').map(str::trim).collect();
            (f.len() >= 6 && f[0] == "v").then(|| Upgrade { name: f[2].to_string(), old: f[3].to_string(), new: f[4].to_string() })
        })
        .collect()
}

/// Every package with an update. PackageKit, when present, refreshes the package lists
/// first (it allows that without a password).
pub(crate) fn upgrades(pm: Pm) -> Result<Vec<Upgrade>, String> {
    if has("pkcon") {
        cmd_any("pkcon", &["refresh", "--noninteractive"]);
    }
    let mut list = match pm {
        Pm::Apt => parse_apt(&cmd("apt", &["list", "--upgradable"])?),
        Pm::Dnf => {
            let (out, code) = cmd_any("dnf", &["check-update", "-q"]).ok_or("couldn't run dnf")?;
            if code != 0 && code != 100 {
                return Err(format!("dnf check-update failed ({code})"));
            }
            parse_dnf(&out)
        }
        Pm::Pacman => {
            // checkupdates syncs a private copy of the databases; plain -Qu uses the last sync.
            let fresh = has("checkupdates").then(|| cmd_any("checkupdates", &[])).flatten().filter(|(_, c)| *c == 0 || *c == 2);
            let text = match fresh {
                Some((out, _)) => out,
                None => cmd_any("pacman", &["-Qu"]).map(|(o, _)| o).unwrap_or_default(),
            };
            parse_pacman(&text)
        }
        Pm::Zypper => parse_zypper(&cmd("zypper", &["--quiet", "--non-interactive", "list-updates"])?),
    };
    for u in list.iter_mut().filter(|u| u.old.is_empty()) {
        u.old = installed(pm, &u.name).unwrap_or_default();
    }
    Ok(list)
}

/// The installed version of a package, if it's installed.
pub(crate) fn installed(pm: Pm, name: &str) -> Option<String> {
    let out = match pm {
        Pm::Apt => cmd("dpkg-query", &["-W", "-f=${Status}\t${Version}", name]).ok().filter(|s| s.contains("install ok installed"))?.split('\t').nth(1)?.to_string(),
        Pm::Dnf | Pm::Zypper => {
            let v = cmd("rpm", &["-q", "--qf", "%{EPOCH}:%{VERSION}-%{RELEASE}\n", name]).ok()?;
            let v = v.lines().last()?.trim().to_string();
            v.strip_prefix("(none):").map(String::from).unwrap_or(v)
        }
        Pm::Pacman => cmd("pacman", &["-Q", name]).ok()?.split_whitespace().nth(1)?.to_string(),
    };
    (!out.trim().is_empty()).then(|| out.trim().to_string())
}

/// The version of the first installed package among `names`.
pub(crate) fn installed_any(names: &[&str]) -> Option<String> {
    let pm = detect().ok()?;
    names.iter().find_map(|n| installed(pm, n))
}

/// Package names as the helper accepts them.
pub(crate) fn is_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && !s.starts_with(['-', '.']) && s.chars().all(|c| c.is_ascii_alphanumeric() || "+-._@".contains(c))
}

pub(crate) fn is_version(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_alphanumeric() || "+-._:~".contains(c))
}

/// What kind of driver a package carries, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Kind {
    Kernel,
    Firmware,
    Microcode,
    Nvidia,
    Mesa,
    Xorg,
    Dkms,
}

impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Kind::Kernel => "Linux kernel",
            Kind::Firmware => "Linux firmware",
            Kind::Microcode => "CPU microcode",
            Kind::Nvidia => "NVIDIA driver",
            Kind::Mesa => "Mesa graphics drivers",
            Kind::Xorg => "X.Org video drivers",
            Kind::Dkms => "Kernel module",
        }
    }
}

pub(crate) fn kind(name: &str) -> Option<Kind> {
    let n = name.to_ascii_lowercase();
    let k = if n.contains("nvidia") {
        Kind::Nvidia
    } else if n.ends_with("-dkms") || n.starts_with("akmod-") || n.starts_with("kmod-") {
        Kind::Dkms
    } else if n.contains("microcode") || n.contains("-ucode") || n.starts_with("ucode-") {
        Kind::Microcode
    } else if n.contains("firmware") && !n.contains("fwupd") {
        Kind::Firmware
    } else if matches!(n.as_str(), "linux" | "linux-lts" | "linux-zen" | "linux-hardened" | "linux-rt" | "kernel" | "kernel-core" | "kernel-modules" | "kernel-modules-core" | "kernel-modules-extra" | "kernel-default" | "linux-generic" | "linux-image-generic")
        || n.starts_with("linux-image-")
        || n.starts_with("linux-generic-hwe")
        || n.starts_with("linux-modules-")
        || n.starts_with("kernel-default-")
    {
        Kind::Kernel
    } else if n.starts_with("mesa") || n.contains("-mesa-") || n.starts_with("libgl1-mesa") || n.starts_with("libegl-mesa") || n.starts_with("libglx-mesa") || n.starts_with("vulkan-radeon") || n.starts_with("vulkan-intel") || n.starts_with("lib32-mesa") || n.starts_with("lib32-vulkan-") || n == "intel-media-driver" || n.starts_with("libdrm") {
        Kind::Mesa
    } else if n.starts_with("xserver-xorg-video-") || n.starts_with("xorg-x11-drv-") || n.starts_with("xf86-video-") {
        Kind::Xorg
    } else {
        return None;
    };
    Some(k)
}

/// The package whose version stands for a group (the driver itself, not a helper library).
pub(crate) fn primary<'a>(k: Kind, names: &[&'a str]) -> &'a str {
    let pick = |pats: &[&str]| names.iter().copied().find(|n| pats.iter().any(|p| n.starts_with(p) || n == p));
    let best = match k {
        Kind::Kernel => pick(&["linux-image-generic", "linux-generic", "kernel-core", "kernel", "linux", "kernel-default"]),
        Kind::Firmware => pick(&["linux-firmware"]),
        Kind::Nvidia => pick(&["nvidia-driver", "nvidia-open", "nvidia-dkms", "akmod-nvidia", "nvidia", "xorg-x11-drv-nvidia"]),
        Kind::Mesa => pick(&["mesa-dri-drivers", "libgl1-mesa-dri", "mesa", "Mesa-dri"]),
        _ => None,
    };
    best.or(names.first().copied()).unwrap_or("")
}

// ---------------- Version comparison (dpkg / rpm / pacman style) ----------------

fn split_epoch(v: &str) -> (u64, &str) {
    match v.split_once(':') {
        Some((e, rest)) if e.chars().all(|c| c.is_ascii_digit()) && !e.is_empty() => (e.parse().unwrap_or(0), rest),
        _ => (0, v),
    }
}

/// rpmvercmp-style segment compare: digit runs numerically, letter runs alphabetically,
/// `~` sorts before everything (pre-releases), other characters separate segments.
fn cmp_segments(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        // Tilde: a pre-release sorts lower.
        match (a.first() == Some(&b'~'), b.first() == Some(&b'~')) {
            (true, true) => {
                a = &a[1..];
                b = &b[1..];
                continue;
            }
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        let skip = |s: &[u8]| s.iter().position(|c| c.is_ascii_alphanumeric() || *c == b'~').unwrap_or(s.len());
        a = &a[skip(a)..];
        b = &b[skip(b)..];
        if a.first() == Some(&b'~') || b.first() == Some(&b'~') {
            continue;
        }
        match (a.is_empty(), b.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        let numeric = a[0].is_ascii_digit();
        let run = |s: &[u8]| s.iter().position(|c| if numeric { !c.is_ascii_digit() } else { !c.is_ascii_alphabetic() }).unwrap_or(s.len());
        let (ra, rb) = (run(a), run(b));
        if !numeric && rb == 0 || numeric && !b[0].is_ascii_digit() {
            // Numbers sort after letters.
            return if numeric { Ordering::Greater } else { Ordering::Less };
        }
        let (sa, sb) = (&a[..ra], &b[..rb]);
        let o = if numeric {
            let ta = std::str::from_utf8(sa).unwrap_or("").trim_start_matches('0');
            let tb = std::str::from_utf8(sb).unwrap_or("").trim_start_matches('0');
            ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb))
        } else {
            sa.cmp(sb)
        };
        if o != Ordering::Equal {
            return o;
        }
        a = &a[ra..];
        b = &b[rb..];
    }
}

/// Compares package versions ("1:24.2.4-1.fc41", "3.20251202.1ubuntu0.24.04.1").
pub(crate) fn cmp_versions(a: &str, b: &str) -> Ordering {
    let (ea, ra) = split_epoch(a);
    let (eb, rb) = split_epoch(b);
    ea.cmp(&eb).then_with(|| {
        let (va, rela) = ra.rsplit_once('-').unwrap_or((ra, ""));
        let (vb, relb) = rb.rsplit_once('-').unwrap_or((rb, ""));
        cmp_segments(va, vb).then_with(|| cmp_segments(rela, relb))
    })
}

/// Reads a local package file's name and version.
pub(crate) fn file_package(path: &Path) -> Option<(String, String)> {
    let name = path.file_name()?.to_str()?;
    let p = path.to_str()?;
    let (n, v) = if name.ends_with(".deb") {
        (cmd("dpkg-deb", &["-f", p, "Package"]).ok()?, cmd("dpkg-deb", &["-f", p, "Version"]).ok()?)
    } else if name.ends_with(".rpm") {
        let out = cmd("rpm", &["-qp", "--qf", "%{NAME}\t%{EPOCH}:%{VERSION}-%{RELEASE}", p]).ok()?;
        let (n, v) = out.split_once('\t')?;
        (n.to_string(), v.strip_prefix("(none):").unwrap_or(v).to_string())
    } else if let Some(stem) = name.strip_suffix(".pkg.tar.zst").or_else(|| name.strip_suffix(".pkg.tar.xz")) {
        // name-version-release-arch
        let mut parts: Vec<&str> = stem.rsplitn(4, '-').collect();
        parts.reverse();
        if parts.len() != 4 {
            return None;
        }
        (parts[0].to_string(), format!("{}-{}", parts[1], parts[2]))
    } else {
        return None;
    };
    let (n, v) = (n.trim().to_string(), v.trim().to_string());
    (is_name(&n) && is_version(&v)).then_some((n, v))
}

#[cfg(test)]
mod tests {
    use super::*;

    const APT: &str = include_str!("../../../tests/fixtures/linux/apt-list-upgradable.txt");
    const DNF5: &str = include_str!("../../../tests/fixtures/linux/dnf5-check-update.txt");
    const PACMAN: &str = include_str!("../../../tests/fixtures/linux/pacman-Qu.txt");

    #[test]
    fn parses_real_package_manager_output() {
        let apt = parse_apt(APT);
        assert_eq!(apt.len(), 3); // "Listing..." is skipped
        assert_eq!(apt[0], Upgrade { name: "amd64-microcode".into(), old: "3.20231019.1ubuntu2".into(), new: "3.20251202.1ubuntu0.24.04.1".into() });
        assert_eq!(kind(&apt[0].name), Some(Kind::Microcode));
        assert_eq!(kind("libc6"), None);
        let dnf = parse_dnf(DNF5);
        assert_eq!(dnf.iter().map(|u| u.name.as_str()).collect::<Vec<_>>(), ["amd-ucode-firmware", "linux-firmware-whence", "xz", "xz-libs"]);
        assert_eq!(dnf[2].new, "1:5.8.1-4.fc41");
        assert_eq!(kind("amd-ucode-firmware"), Some(Kind::Microcode));
        assert_eq!(kind("linux-firmware-whence"), Some(Kind::Firmware));
        let pac = parse_pacman(PACMAN);
        assert!(pac.len() >= 20);
        assert_eq!(pac[0], Upgrade { name: "ca-certificates-mozilla".into(), old: "3.133-1".into(), new: "3.134-1".into() });
        assert!(pac.iter().all(|u| cmp_versions(&u.new, &u.old) == Ordering::Greater), "{pac:?}");
        assert!(apt.iter().all(|u| cmp_versions(&u.new, &u.old) == Ordering::Greater));
        let zyp = parse_zypper("Loading repository data...\nS | Repository | Name           | Current Version | Available Version | Arch\n--+------------+----------------+-----------------+-------------------+-------\nv | repo-oss   | kernel-firmware-amdgpu | 20240909-1.1 | 20241017-1.1 | noarch\n");
        assert_eq!(zyp, vec![Upgrade { name: "kernel-firmware-amdgpu".into(), old: "20240909-1.1".into(), new: "20241017-1.1".into() }]);
    }

    #[test]
    fn classifies_driver_packages() {
        for (n, k) in [
            ("linux-image-generic", Kind::Kernel),
            ("linux-generic-hwe-24.04", Kind::Kernel),
            ("kernel-core", Kind::Kernel),
            ("linux-zen", Kind::Kernel),
            ("linux-firmware", Kind::Firmware),
            ("firmware-misc-nonfree", Kind::Firmware),
            ("amd-gpu-firmware", Kind::Firmware),
            ("intel-microcode", Kind::Microcode),
            ("amd-ucode", Kind::Microcode),
            ("nvidia-driver-550", Kind::Nvidia),
            ("libnvidia-gl-550", Kind::Nvidia),
            ("akmod-nvidia", Kind::Nvidia),
            ("mesa-dri-drivers", Kind::Mesa),
            ("libgl1-mesa-dri", Kind::Mesa),
            ("vulkan-radeon", Kind::Mesa),
            ("xserver-xorg-video-amdgpu", Kind::Xorg),
            ("r8125-dkms", Kind::Dkms),
        ] {
            assert_eq!(kind(n), Some(k), "{n}");
        }
        for n in ["fwupd", "linux-headers-generic", "libc6", "firefox", "linux-api-headers"] {
            assert_eq!(kind(n), None, "{n}");
        }
        assert_eq!(primary(Kind::Nvidia, &["libnvidia-gl-550", "nvidia-driver-550", "nvidia-utils-550"]), "nvidia-driver-550");
        assert_eq!(primary(Kind::Kernel, &["linux-headers-generic", "linux-image-generic", "linux-generic"]), "linux-image-generic");
    }

    #[test]
    fn compares_versions_like_package_managers() {
        use Ordering::*;
        for (a, b, o) in [
            ("1.0", "1.0", Equal),
            ("1.10", "1.9", Greater),
            ("24.2.4-1.fc41", "24.2.10-1.fc41", Less),
            ("1:5.8.1-4.fc41", "5.9-1.fc41", Greater), // epoch wins
            ("2.0~rc1", "2.0", Less),
            ("3.20251202.1ubuntu0.24.04.1", "3.20231019.1ubuntu2", Greater),
            ("6.11.4-301.fc41", "6.11.4-300.fc41", Greater),
            ("550.120-0ubuntu1", "550.120-0ubuntu0.24.04.1", Greater),
            ("1.0a", "1.0", Greater),
            ("1.0", "1.0.1", Less),
        ] {
            assert_eq!(cmp_versions(a, b), o, "{a} vs {b}");
            assert_eq!(cmp_versions(b, a), o.reverse(), "{b} vs {a}");
        }
        assert!(is_name("linux-firmware") && !is_name("-x") && is_version("1:2.3-4") && !is_version("--y"));
    }
}
