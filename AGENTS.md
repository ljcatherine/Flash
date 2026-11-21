# Agent notes for Flash

Flash is a Windows driver updater and PC health tool in Rust (egui). The app lives in `src/`,
packaging in `scripts/release.ps1` and `packaging/flash.wxs`, and the project website in `site/`.

## Every public release: update the version on the websites

Whenever a new version is published on GitHub Releases, the current release version must also
be updated on both websites, in the same piece of work, before calling the release done.

1. **flash.liamjc.dev** (`site/public/index.html` in this repo)
   - Hero pill: `Version X.Y.Z for Windows ...`
   - Download section eyebrow: `Download · version X.Y.Z`
   - The three download links (`/releases/download/vX.Y.Z/Flash-X.Y.Z-x64.msi`, `-portable.zip`, `.exe`),
     their file names and sizes, and the `SHA256SUMS.txt` link
   - Deploy: `cd site && npx wrangler deploy`
2. **liamjc.dev** (repo `ljcatherine/liamjc.dev`, local clone `../liamjc.dev`)
   - The version badge on the Flash card in `public/index.html`
   - Commit, push, then deploy: `npx wrangler deploy` from that repo
3. **This repo**: the version in `Cargo.toml`, the release badge and footer in `README.md`.

Check both live sites afterwards (`curl` the pages and look for the new version) before reporting the release as finished.

## Linux version

The Linux port is planned in `docs/LINUX_HANDOFF.md`: same repo, shared code, 1:1 with Windows.

## Other things to know

- The app embeds a `requireAdministrator` manifest, so `cargo test` covers the library only (the bin has `test = false`).
- `scripts/release.ps1` builds the exe, portable zip and MSI into `dist/` and signs them when
  `FLASH_SIGN_*` environment variables are set; `-RequireSigning` refuses unsigned builds.
- Both websites serve a strict Content-Security-Policy (`_headers`): no inline styles or scripts.
- License: GPL-3.0-or-later.
