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

## Local dev

Prereqs: Rust 1.96+ · Node 24+ · Visual Studio Build Tools 2022 · WebView2.

```bash
# Install deps
npm install
cd src-tauri

# Get a Let's Encrypt cert (one-time; see ops/print-bridge-cert/)
# Then copy fullchain.pem + privkey.pem into ./certs/

# Build the Rust binary
cargo build

# Run the bridge + frontend together
cd ..
npm run tauri dev
```

## Tests

```bash
cd src-tauri
cargo test                                              # router + handler tests (no TLS)
cargo test --test tls_smoke -- --ignored --nocapture    # full TLS + reqwest smoke
```

## Distribution

V1: `npm run tauri build` produces a `.msi` installer in
`src-tauri/target/release/bundle/msi/`. Distribute via GitHub Releases.

V2 will add code signing (Sectigo cert) to remove the Windows
SmartScreen prompt on first install.

## License

MIT — see [LICENSE](./LICENSE).
