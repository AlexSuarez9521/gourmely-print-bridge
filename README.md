# GourmelyPrint Bridge

> Local print bridge for the GourmelyHub POS — open source replacement for QZ Tray.

A tiny desktop service (Tauri 2 + Rust) that listens on
`wss://localhost.gourmelyhub.busticco.com:8181` and forwards
ESC/POS byte streams from the GourmelyHub web POS to USB thermal
printers attached to the cashier's PC.

## Why this exists

The browser's same-origin / Mixed Content / Private Network Access
policies prevent an HTTPS website from talking directly to USB devices
on the cashier's machine. Most POS vendors solve this with a paid
bridge (QZ Tray costs ~$300/year per domain). This is the free,
self-hosted equivalent — bundled with the same DNS+TLS trick they
use, but on our own domain.

## How it works

```
Browser POS (HTTPS) → WSS over Let's Encrypt cert → Bridge (port 8181)
                                                       → Windows Print Spooler
                                                       → POS-58 / kitchen printer / etc.
```

- DNS: `localhost.gourmelyhub.busticco.com → 127.0.0.1` (Cloudflare A record).
- Cert: real Let's Encrypt cert, issued via DNS-01 challenge (no popups, no warnings in Chrome).
- Bridge: Rust + axum, binds `127.0.0.1:8181` only, enforces an Origin allowlist.
- Print: uses the OS print spooler in RAW mode so ESC/POS bytes hit the printer unchanged.

## Wire protocol (WebSocket)

Connect: `wss://localhost.gourmelyhub.busticco.com:8181/print`

Messages (client → bridge), one JSON object per text frame:

```json
{ "op": "list",  "id": "uuid" }
{ "op": "print", "id": "uuid", "printer": "POS-58", "data": "<base64-escpos>" }
{ "op": "test",  "id": "uuid", "printer": "POS-58" }
```

Responses (bridge → client):

```json
{ "id": "uuid", "ok": true, "printers": ["POS-58", "..."] }
{ "id": "uuid", "ok": true, "jobId": 0 }
{ "id": "uuid", "ok": false, "error": "human-readable message" }
```

Convenience HTTP routes (same TLS server, no auth needed):

- `GET /health` — JSON with version, uptime, printer count
- `GET /printers` — JSON with the printer list

## For restaurant operators

You don't download from this repo. Open the GourmelyHub dashboard →
**Configuración → Impresión → Instalar GourmelyPrint Bridge**, click
the download button, run the installer, and follow the on-screen guide.
The bridge auto-updates itself afterwards. This repo is for developers.

## Local dev

Prereqs: Rust 1.96+ · Node 24+ · Visual Studio Build Tools 2022 · WebView2.

```bash
# Install deps
npm install

# The Let's Encrypt cert ships in src-tauri/certs/ (see SECURITY.md for
# why it's committed). Nothing to set up for a normal build.

# Run the bridge + frontend together
npm run tauri dev
```

## Tests

```bash
cd src-tauri
cargo test                                              # router + handler tests (no TLS)
cargo test --test tls_smoke -- --ignored --nocapture    # full TLS + reqwest smoke
```

## Releases (maintainers)

`.github/workflows/release.yml` builds and publishes the MSI on every
`v*` tag (or manually via workflow dispatch). It also signs the MSI for
the Tauri auto-updater and mirrors the artifacts to the Cloudflare R2
download bucket the dashboard points at.

```bash
# 1. Bump version in src-tauri/tauri.conf.json AND src-tauri/Cargo.toml
# 2. Commit + merge to main
# 3. Tag + push:
git tag v0.1.1
git push origin v0.1.1
# 4. Watch Actions → "Release"
```

The Let's Encrypt cert is rotated automatically every 60 days by
`.github/workflows/cert-renew.yml` — see [ops/README.md](ops/README.md).

Code signing (to remove the Windows SmartScreen prompt) activates
automatically once the `WINDOWS_CODE_SIGN_PFX_BASE64` secret is set.

## Security

The committed TLS private key is safe by design — see
[SECURITY.md](SECURITY.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT — see [LICENSE](./LICENSE).
