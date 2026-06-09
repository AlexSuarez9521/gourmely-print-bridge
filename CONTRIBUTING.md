# Contributing

Thanks for your interest in GourmelyPrint Bridge. This is the open
source local print service for the GourmelyHub POS, but it's a normal
Tauri 2 + Rust app and you don't need any GourmelyHub access to build,
run, or improve it.

## Prerequisites

- Rust 1.96+ (stable)
- Node 24+
- Windows: Visual Studio Build Tools 2022 + WebView2
- macOS/Linux: the usual Tauri prerequisites (see
  https://tauri.app/start/prerequisites/)

## Local development

```bash
npm install
npm run tauri dev
```

The bundled TLS cert lives in `src-tauri/certs/` and is embedded at
compile time via `include_bytes!()`. For local hacking you can point
the bridge at any cert you like; see `src-tauri/src/server.rs`.

## Tests

```bash
cd src-tauri
cargo test                                              # router + handler tests (no TLS)
cargo test --test tls_smoke -- --ignored --nocapture    # full TLS + reqwest smoke
```

## Pull requests

- Branch from `main`, open the PR against `main`.
- Keep commits focused; conventional-commit style (`feat:`, `fix:`,
  `ci:`, `docs:`) is appreciated but not enforced.
- Code, comments, and commit messages in English.
- Don't bump the version in your PR — releases are cut by maintainers
  via tags (`v*`), which the release workflow turns into an MSI.

## Releasing (maintainers only)

```bash
# Bump version in src-tauri/tauri.conf.json AND src-tauri/Cargo.toml
git tag v0.1.1
git push origin v0.1.1
```

The `Release` workflow builds the MSI, signs it for the auto-updater,
and publishes a GitHub Release with a stable-named installer asset
(`GourmelyPrint-Bridge-setup.msi`). The GourmelyHub dashboard download
button + the updater point at the permanent `/releases/latest/download/`
URL. See `.github/workflows/release.yml`.

## License

By contributing you agree your contributions are licensed under the
[MIT License](LICENSE).
