# Agent notes for Flash

Flash is a driver updater and PC health tool for Windows and Linux in Rust (egui). The app lives
in `src/`, with everything OS-specific in `src/platform/` (`windows.rs`, `linux/`); both export
the same functions, so a feature missing on one platform is a build error. The Linux polkit helper
is the `helper/` crate. Packaging: `scripts/release.ps1` and `packaging/flash.wxs` (Windows),
`scripts/release.sh`, `scripts/linux/` and `packaging/linux/` (Linux), `flake.nix` and `nix/` (Nix).
The project website lives in a local `site/` folder that isn't committed (`/site` is in
`.gitignore`): edit and deploy it from there.

## Every public release: update the version on the websites

Whenever a new version is published on GitHub Releases, the current release version must also
be updated on both websites, in the same piece of work, before calling the release done.

1. **flash.liamjc.dev** (`site/public/index.html`, local and untracked)
   - Hero pill: `Version X.Y.Z · ...`
   - Download section eyebrow: `Download · version X.Y.Z`
   - The three download links (`/releases/download/vX.Y.Z/Flash-X.Y.Z-x64.msi`, `-portable.zip`, `.exe`),
     their file names and sizes, and the `SHA256SUMS.txt` link
   - The Linux downloads (`flash_X.Y.Z_amd64.deb`, `flash-X.Y.Z.x86_64.rpm`, `Flash-X.Y.Z-x86_64.AppImage`,
     `flash-X.Y.Z-x86_64.tar.gz`), their sizes, and the `SHA256SUMS-linux.txt` link
   - Deploy: `cd site && npx wrangler deploy`
2. **liamjc.dev** (repo `ljcatherine/liamjc.dev`, local clone `../liamjc.dev`)
   - The version badge on the Flash card in `public/index.html`
   - Commit, push, then deploy: `npx wrangler deploy` from that repo
3. **This repo**: the version in `Cargo.toml` and `helper/Cargo.toml` (they must match), the release
   badge and footer in `README.md`.

## One release, two platforms

One tag `vX.Y.Z`, one GitHub release on `ljcatherine/Flash`, with each platform's assets built by
its own script: `scripts/release.ps1` on Windows, `scripts/release.sh --upload` on Linux (it builds
in a Debian 12 container so the binaries run on every distribution, refuses to upload unless HEAD is
tagged `v<version>`, creates the release if it doesn't exist and otherwise uploads only its own
assets with `--clobber`). Each platform has its own checksum file (`SHA256SUMS.txt`,
`SHA256SUMS-linux.txt`), and self-update reads GitHub's per-asset digests, so neither script can
break the other's assets. Neither script cross-compiles the other platform.

Check both live sites afterwards (`curl` the pages and look for the new version) before reporting the release as finished.


- On Windows the app embeds a `requireAdministrator` manifest, so `cargo test` covers the library only (the bin has `test = false`).
- On Linux the GUI runs as the user and refuses to run as root. Privileged actions go through
  `pkexec flash-helper <read|logs|manage> <verb> ...` (`packaging/linux/dev.liamjc.flash.policy.in`):
  add new privileged work as a validated helper verb, never as a shell command. Run
  `cargo test --workspace` (the helper has its own tests); Linux parser tests use real captured
  output in `tests/fixtures/linux/` (scrub serial numbers and MAC addresses before committing).
  `cargo test --lib live_linux -- --ignored --nocapture` runs the read-only backend against this PC.
- Wording that names OS-only things uses `os!("Windows text", "Linux text")`; everything else is shared.
- Check the Windows target still compiles from Linux:
  `docker run --rm -v $PWD:/src -w /src rust:1-bookworm sh -c 'apt-get update -qq && apt-get install -y -qq gcc-mingw-w64-x86-64 && rustup target add x86_64-pc-windows-gnu && cargo check --target x86_64-pc-windows-gnu --all-targets -p flash'`
- `scripts/release.ps1` builds the exe, portable zip and MSI into `dist/` and signs them when
  `FLASH_SIGN_*` environment variables are set; `-RequireSigning` refuses unsigned builds.
- Both websites serve a strict Content-Security-Policy (`_headers`): no inline styles or scripts.
- License: GPL-3.0-or-later.
