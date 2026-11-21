#!/usr/bin/env bash
# Runs inside the build container (see scripts/release.sh): tests, builds and packages
# Flash for Linux into dist-linux/: .deb, .rpm, AppImage and a tarball.
set -euo pipefail
cd "$(dirname "$0")/../.."
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
arch=$(uname -m)                                   # x86_64 / aarch64
deb_arch=$([ "$arch" = aarch64 ] && echo arm64 || echo amd64)
out=dist-linux
helper_path=/usr/libexec/flash/flash-helper

cargo test --workspace --locked
cargo build --release --locked -p flash -p flash-helper
target=${CARGO_TARGET_DIR:-target}
bin=$target/release/flash
helper=$target/release/flash-helper

rm -rf "$out"
mkdir -p "$out"
stage=$(mktemp -d)
# Hand the packages back to whoever ran release.sh (the container runs as root), even on failure.
trap '[ -n "${OWNER:-}" ] && chown -R "$OWNER" "$out"; rm -r -f "$stage"' EXIT
policy() { sed "s|@HELPER@|$1|g" packaging/linux/dev.liamjc.flash.policy.in; }

# A system install: /usr with the helper in libexec and the polkit policy.
install_tree() {
  local root=$1
  install -Dm755 "$bin" "$root/usr/bin/flash"
  install -Dm755 "$helper" "$root$helper_path"
  policy "$helper_path" | install -Dm644 /dev/stdin "$root/usr/share/polkit-1/actions/dev.liamjc.flash.policy"
  install -Dm644 packaging/linux/flash.desktop "$root/usr/share/applications/flash.desktop"
  install -Dm644 assets/flash.png "$root/usr/share/icons/hicolor/256x256/apps/flash.png"
  install -Dm644 assets/flash.svg "$root/usr/share/icons/hicolor/scalable/apps/flash.svg"
  install -Dm644 README.md "$root/usr/share/doc/flash/README.md"
  install -Dm644 LICENSE "$root/usr/share/doc/flash/LICENSE"
}
desktop-file-validate packaging/linux/flash.desktop

# 1. Debian / Ubuntu
deb="$stage/deb"
install_tree "$deb"
mkdir -p "$deb/DEBIAN"
cat > "$deb/DEBIAN/control" <<CONTROL
Package: flash
Version: $version
Architecture: $deb_arch
Maintainer: ljcatherine <noreply@github.com>
Section: admin
Priority: optional
Homepage: https://flash.liamjc.dev
Depends: libc6 (>= 2.36), pkexec | policykit-1, udev, kmod
Recommends: fwupd, udisks2, xdg-utils, zenity | kdialog
Suggests: smartmontools, dmidecode, memtest86+, snapper | timeshift
Description: Driver updater and PC health tool
 Driver and firmware updates (distribution packages, LVFS), hardware health with a
 hardware error knowledge base, benchmarks, storage cleanup and a system monitor.
CONTROL
dpkg-deb --build --root-owner-group -Zxz "$deb" "$out/flash_${version}_${deb_arch}.deb"

# 2. Fedora / openSUSE
rpmtop="$stage/rpm"
mkdir -p "$rpmtop"/{BUILD,RPMS,SPECS,BUILDROOT}
install_tree "$stage/rpmroot"
cat > "$rpmtop/SPECS/flash.spec" <<SPEC
Name: flash
Version: $version
Release: 1
Summary: Driver updater and PC health tool
License: GPL-3.0-or-later
URL: https://flash.liamjc.dev
Requires: polkit, kmod
Recommends: fwupd, udisks2, xdg-utils
%global __os_install_post %{nil}
%global debug_package %{nil}
%description
Driver and firmware updates (distribution packages, LVFS), hardware health with a
hardware error knowledge base, benchmarks, storage cleanup and a system monitor.
%install
cp -a $stage/rpmroot/. %{buildroot}/
%files
/usr/bin/flash
$helper_path
/usr/share/polkit-1/actions/dev.liamjc.flash.policy
/usr/share/applications/flash.desktop
/usr/share/icons/hicolor/256x256/apps/flash.png
/usr/share/icons/hicolor/scalable/apps/flash.svg
%doc /usr/share/doc/flash/README.md
%license /usr/share/doc/flash/LICENSE
SPEC
rpmbuild --quiet --define "_topdir $rpmtop" --target "$arch" -bb "$rpmtop/SPECS/flash.spec"
cp "$rpmtop/RPMS/$arch/flash-$version-1.$arch.rpm" "$out/flash-$version.$arch.rpm"

# 3. AppImage. Flash copies the helper out at run time (root can't read the AppImage mount);
#    without the system policy, pkexec asks for a password for each action.
app="$stage/Flash.AppDir"
install -Dm755 "$bin" "$app/usr/bin/flash"
install -Dm755 "$helper" "$app/usr/bin/flash-helper"
install -Dm644 packaging/linux/flash.desktop "$app/flash.desktop"
install -Dm644 assets/flash.png "$app/flash.png"
install -Dm644 assets/flash.png "$app/usr/share/icons/hicolor/256x256/apps/flash.png"
cat > "$app/AppRun" <<'APPRUN'
#!/bin/sh
here=$(dirname "$(readlink -f "$0")")
export APPDIR="${APPDIR:-$here}"
exec "$here/usr/bin/flash" "$@"
APPRUN
chmod 755 "$app/AppRun"
ARCH="$arch" appimagetool --no-appstream --runtime-file /usr/local/share/appimage-runtime "$app" "$out/Flash-$version-$arch.AppImage" >/dev/null

# 4. Tarball: binary, helper, policy and desktop entry to install by hand.
tdir="$stage/flash-$version-$arch"
mkdir -p "$tdir"
install -m755 "$bin" "$helper" "$tdir/"
install -m644 README.md LICENSE packaging/linux/flash.desktop assets/flash.png "$tdir/"
policy "$helper_path" > "$tdir/dev.liamjc.flash.policy"
cat > "$tdir/INSTALL.txt" <<TXT
Flash $version for Linux ($arch)

Run ./flash from this folder. For password-free hardware reads and remembered
authorization, install the helper and its polkit policy once (as root):

  install -Dm755 flash-helper $helper_path
  install -Dm644 dev.liamjc.flash.policy /usr/share/polkit-1/actions/dev.liamjc.flash.policy

Without them Flash still works and asks for an administrator password for each change.
TXT
tar -C "$stage" --owner=0 --group=0 -czf "$out/flash-$version-$arch.tar.gz" "flash-$version-$arch"

ls -l "$out"
