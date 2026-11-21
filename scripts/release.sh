#!/usr/bin/env bash
# Builds Flash for Linux and packages a release into dist-linux/ (the Linux counterpart of
# release.ps1, which builds the Windows assets on Windows):
#   flash_<version>_amd64.deb          Debian / Ubuntu package
#   flash-<version>.x86_64.rpm         Fedora / openSUSE package
#   Flash-<version>-x86_64.AppImage    portable
#   flash-<version>-x86_64.tar.gz      binary, helper, README and LICENSE
#   SHA256SUMS-linux.txt               checksums of the Linux assets only (LF endings)
#
# Usage: scripts/release.sh [--arch x86_64|aarch64] [--upload]
#
# The build runs in a Debian 12 container (Docker or Podman), so the binaries run on every
# supported distribution whatever this machine runs. aarch64 builds use emulation
# (qemu-user binfmt) when this machine is x86_64.
#
# --upload publishes to the GitHub release for tag v<version> on ljcatherine/Flash. HEAD must
# be that tag. The release is created if it doesn't exist yet (whichever platform's script
# runs first creates it); otherwise only the Linux assets are added or replaced. The Windows
# assets and their SHA256SUMS.txt are never touched.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
arch=$(uname -m)
upload=0
while [ $# -gt 0 ]; do
  case "$1" in
    --arch) arch=$2; shift 2 ;;
    --upload) upload=1; shift ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
case "$arch" in
  x86_64) platform=linux/amd64 ;;
  aarch64) platform=linux/arm64 ;;
  *) echo "unsupported architecture: $arch" >&2; exit 2 ;;
esac

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
helper_version=$(sed -n 's/^version = "\(.*\)"$/\1/p' helper/Cargo.toml | head -1)
tag="v$version"
echo "Flash $version for Linux ($arch)"
if [ "$helper_version" != "$version" ]; then
  echo "helper/Cargo.toml is $helper_version but Cargo.toml is $version: keep them the same." >&2
  exit 1
fi
if [ "$upload" = 1 ]; then
  head_tags=$(git tag --points-at HEAD)
  if ! grep -qx "$tag" <<<"$head_tags"; then
    echo "Refusing to upload: HEAD isn't tagged $tag (tags here: ${head_tags:-none})." >&2
    exit 1
  fi
  command -v gh >/dev/null || { echo "gh (GitHub CLI) is needed to upload." >&2; exit 1; }
fi

engine=$(command -v docker || command -v podman || true)
[ -n "$engine" ] || { echo "Docker or Podman is needed: the build runs in a Debian 12 container." >&2; exit 1; }

image="flash-linux-build:$arch"
"$engine" build --platform "$platform" -t "$image" -f scripts/linux/Dockerfile scripts/linux
cache="${XDG_CACHE_HOME:-$HOME/.cache}/flash-release"
mkdir -p "$cache/registry" "$cache/target-$arch"
"$engine" run --rm --platform "$platform" \
  -v "$root:/src" -w /src \
  -v "$cache/registry:/usr/local/cargo/registry" \
  -v "$cache/target-$arch:/build/target" \
  -e CARGO_TARGET_DIR=/build/target \
  -e OWNER="$(id -u):$(id -g)" \
  "$image" scripts/linux/package.sh

# Checksums last, over the final files. Linux assets only, so neither platform's script can
# break the other's checksum file.
dist=dist-linux
(cd "$dist" && sha256sum -- *.deb *.rpm *.AppImage *.tar.gz > SHA256SUMS-linux.txt)
cat "$dist/SHA256SUMS-linux.txt"

if [ "$upload" = 1 ]; then
  repo=ljcatherine/Flash
  assets=("$dist"/*.deb "$dist"/*.rpm "$dist"/*.AppImage "$dist"/*.tar.gz "$dist/SHA256SUMS-linux.txt")
  if gh release view "$tag" -R "$repo" >/dev/null 2>&1; then
    gh release upload "$tag" -R "$repo" --clobber "${assets[@]}"
  else
    gh release create "$tag" -R "$repo" --title "Flash $version" --notes "Flash $version" --verify-tag "${assets[@]}"
  fi
  echo "Uploaded the Linux assets to $tag."
fi
