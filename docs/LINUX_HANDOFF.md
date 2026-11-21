# Flash for Linux: agent handoff

**Goal:** Flash for Linux, 1:1 with Flash for Windows 1.1.0. The same pages, features, look,
knowledge bases and release flow, built on Linux mechanisms. A Windows user moving to Linux
should find every button where they expect it.

Read first: `AGENTS.md` (release and website rules), `README.md` (feature list), and the
running Windows app if you can. Owner: ljcatherine. License: GPL-3.0-or-later.

## Decisions already made

- **Same repo, shared code.** Build Linux from this repository, not a fork. Keep the egui UI,
  knowledge bases (`whea.rs`, `hwkb.rs`), benchmarks, catalog parsing, self-update and state
  shared. Move OS-specific code behind `src/platform/{windows,linux}.rs` with `#[cfg(target_os)]`,
  so parity is enforced by the compiler: a missing Linux function fails the build.
- **Unprivileged app, privileged actions.** Windows runs Flash elevated (UAC manifest). On Linux,
  run as the user and elevate each action with polkit (`pkexec` + a small, fixed helper with a
  polkit policy). Never run the GUI as root.
- **Same look.** Same palette, fonts, pages, sidebar order and copy as Windows. Only wording that
  names Windows-only things changes (for example "Device Manager" becomes the Linux tool).
- **x86_64 first**, aarch64 second.

## Feature map: Windows → Linux

| Area | Windows (today) | Linux (build this) |
|---|---|---|
| Device scan | WMI `Win32_PnPEntity`/`Win32_PnPSignedDriver` | `/sys/bus/{pci,usb}/devices`, `modalias`, bound driver from the `driver` symlink, version from `modinfo`/`/sys/module/*/version`; `udevadm info` or `pci.ids` for names |
| Problems page | Device Manager problem codes | Devices with no bound driver, missing firmware (`journalctl -k` "firmware: failed to load"), modules that failed to load, devices fwupd flags |
| Purpose groups | PNP class → purpose | PCI class code / USB class → the same 13 purposes in `PURPOSES` |
| Update source 1 | Windows Update | Distro packages via PackageKit (`pkcon get-updates`), filtered to drivers: kernel, `linux-firmware`, `mesa`, NVIDIA packages, DKMS modules |
| Update source 2 | Microsoft Update Catalog | LVFS via fwupd (`fwupdmgr get-updates --json`): device firmware, UEFI/BIOS capsules |
| Update source 3 | Driver Store | Installed but unused kernel modules/DKMS builds that match a device's modalias better than the bound driver |
| Update source 4 | Local folder of `.inf` | Local folder of fwupd `.cab` files and distro packages (`.deb`/`.rpm`) |
| Install | WU Agent / `pnputil` | `pkcon update <pkgs>` / `fwupdmgr install` through the polkit helper |
| Restore point | `Checkpoint-Computer` | Snapper or Timeshift snapshot if present (Btrfs); otherwise warn clearly and continue only on confirm |
| Driver backup + undo | `pnputil /export-driver`, delete new package | Record the previous package version; undo with the package manager's downgrade (`dnf downgrade`, `apt install pkg=ver`, `pacman -U` from cache) or `fwupdmgr downgrade`; keep the previous kernel |
| Hide update | state.json key | Same, keyed by device + version |
| Hardware errors (WHEA) | WHEA-Logger events | `rasdaemon` (`ras-mc-ctl --errors`), kernel MCE lines in `journalctl -k`, EDAC counters `/sys/devices/system/edac/mc/*/[cu]e_count`, PCIe AER counters `/sys/bus/pci/devices/*/aer_dev_*`. Feed them into the **same** `whea.rs` knowledge base: MCA error types are architectural, identical on both OSes |
| Crashes | BugCheck 1001, Kernel-Power 41 | Kernel panics from `/sys/fs/pstore`, unclean shutdowns from `journalctl --list-boots` + the last boot's tail |
| Display driver resets | Display event 4101 | NVIDIA Xid (`NVRM: Xid`), `amdgpu: GPU reset`, `i915`/`xe` "GPU HANG" in the kernel log |
| Drive health | `Get-StorageReliabilityCounter` | `smartctl -j -a` (smartmontools) through the helper; NVMe temperature also via hwmon |
| Battery | WMI battery classes | `/sys/class/power_supply/BAT*/energy_full{,_design}` |
| RAM speed check | Win32_PhysicalMemory part number | `dmidecode -t memory` (helper) for part number and configured speed; reuse `health::rated_speed` |
| BIOS check | Win32_BIOS | `/sys/class/dmi/id/{bios_version,bios_date,board_vendor,board_name}`; reuse `extras::bios_page`; also offer fwupd if LVFS has the capsule |
| GPU driver check | NVIDIA lookup API | NVIDIA: compare the `nvidia-smi` driver with the distro's packaged branch; AMD/Intel: Mesa and kernel versions against the distro's latest |
| Benchmarks | Rust + OpenGL | Same code. Disk tests: `O_DIRECT` instead of `FILE_FLAG_NO_BUFFERING`; GPU tests already use OpenGL via eframe and work on X11/Wayland |
| Stress test | Same | Same; watch rasdaemon/MCE/EDAC instead of WHEA |
| Storage breakdown | Windows folders | `/`, `/home`, `/usr`, `/var`, `~/.cache`, Flatpak/Snap data |
| Cleaner | Temp, caches, WU cache, old drivers, AppX | `~/.cache` (per app), `/tmp`, `journalctl --vacuum-time`, package caches (`apt clean`, `dnf clean packages`, `paccache -r`), `flatpak uninstall --unused`, Trash, old kernels (never the running or newest one). Same rules: measure first, confirm, never follow symlinks |
| System monitor | sysinfo + GPU Engine counters | sysinfo (works); per-process GPU from DRM fdinfo (`/proc/*/fdinfo`, `drm-engine-*`) for amdgpu/i915/xe, NVML or `nvidia-smi pmon` for NVIDIA |
| Temperatures | ACPI + nvidia-smi | hwmon (`/sys/class/hwmon`: k10temp, coretemp, amdgpu, nvme), plus NVML. Linux exposes more sensors than Windows: show them all |
| Startup apps | StartupApproved registry | `~/.config/autostart/*.desktop` (`Hidden=true` to disable) and `systemctl --user` enabled units; boot impact from `systemd-analyze blame` |
| Close to tray + notification buttons | tray-icon + WinRT toasts | `tray-icon` (libayatana-appindicator) + `notify-rust` (D-Bus notifications with actions: Stop scan, Quit Flash) |
| Weekly scan | Task Scheduler `--scheduled` | `systemd --user` timer running `flash --scheduled` |
| Health report | HTML | Same code |
| Self-update | GitHub release, MSI or exe swap | GitHub release, same digest checks; AppImage replaces itself; `.deb`/`.rpm` go through the helper; distro, Flatpak and Nix installs say "update with your package manager" instead |

## Knowledge bases on Linux

`hwkb.rs` and `whea.rs` are shared. Add a Linux tip list per profile alongside the Windows one:

- **AMD CPUs:** `amd_pstate` mode; AM4 idle freezes fixed by BIOS "Power Supply Idle Control →
  Typical Current Idle"; `k10temp` for temperatures.
- **Intel CPUs:** `intel_pstate`; 13th/14th gen microcode 0x12B arrives via BIOS or the
  `intel-microcode` package; check the `microcode` field in `/proc/cpuinfo`.
- **NVIDIA:** proprietary vs open kernel modules (open recommended from Turing on), Wayland needs
  `nvidia-drm.modeset=1`, DKMS rebuilds after kernel updates, Secure Boot needs MOK-signed modules.
- **AMD Radeon:** amdgpu + Mesa + `linux-firmware` versions matter; GPU resets in the kernel log.
- **Intel Arc:** kernel 6.2+ (`i915`) or the `xe` driver, recent Mesa; Resizable BAR still required.

## Packaging and release

Match Windows' three forms:

| Windows | Linux |
|---|---|
| `Flash-X-x64.msi` (installer) | `flash_X_amd64.deb` and `flash-X.x86_64.rpm` (desktop entry, icon, polkit policy, helper) |
| `Flash-X-x64-portable.zip` | `Flash-X-x86_64.AppImage` |
| `Flash-X-x64.exe` | `flash-X-x86_64.tar.gz` (binary + helper + README + LICENSE) |

Also add a Nix flake (the owner runs NixOS; follow the NexusNix repo's style). Write
`scripts/release.sh` mirroring `release.ps1`: test, build, package, SHA256SUMS with LF endings.
Upload Linux assets to the **same** GitHub release as Windows, with one version number for both.

When Linux ships, follow `AGENTS.md` for both websites, plus:
- flash.liamjc.dev: replace "Linux in the works" with Linux downloads.
- liamjc.dev: remove the "Linux version in the works" badge on the Flash card.

## Rules carried over

- Validate anything that reaches a shell, a privileged helper or a URL. Never interpolate device
  names, package names or paths into shell text (see `extras::set_startup` and
  `app_update::apply` for the pattern).
- The cleaner and installer refuse shallow paths, never follow symlinks, and confirm first.
- Downloads come only from the vendor (GitHub, LVFS, distro mirrors, nvidia.com) and are checked
  against published digests or signatures.
- No secrets in the repo. Strict CSP on the websites. Commits end with the owner's attribution lines.

## Done means

- Every page and button in the Windows app exists on Linux and does the Linux equivalent.
- `cargo test` passes on both targets, with tests for each Linux parser using real captured output.
- Tested on Ubuntu 24.04, Fedora 41, Arch and NixOS, with at least one NVIDIA, one AMD and one
  Intel GPU, on both X11 and Wayland.
- Released, with both websites updated as `AGENTS.md` says.
