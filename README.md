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

Two ways in, one way out — everything ends at the same spooler call.

```
1. Same machine (the till's own browser)
Browser POS (HTTPS) → WSS over Let's Encrypt cert → Bridge (port 8181)
                                                       → Windows Print Spooler
                                                       → POS-58 / kitchen printer / etc.

2. Anywhere (a waiter on mobile data, the owner from home)
GourmelyHub API ── socket.io /print ──→ Bridge (outbound, always connected)
     durable queue + ack                     → same spooler call
```

- DNS: `localhost.gourmelyhub.busticco.com → 127.0.0.1` (Cloudflare A record).
- Cert: real Let's Encrypt cert, issued via DNS-01 challenge (no popups, no warnings in Chrome).
- Bridge: Rust + axum, binds `127.0.0.1:8181` only, and enforces who may print (below).
- Print: uses the OS print spooler in RAW mode so ESC/POS bytes hit the printer unchanged.

## Remote printing (the relay)

Path 1 only works while a browser is open **in the shop**. If nobody has the POS
open, the comanda never comes out and the API answers `{triggered:true}` — a
silent failure. And a waiter outside on 4G could not print at all: the API is on
a VPS, the printer is behind the shop's NAT.

The relay inverts the direction. The bridge dials *out* to the server and keeps
the socket open, so there is no port to forward and no static IP to buy.

- **Pairing.** The owner generates an 8-character code in GourmelyHub →
  Configuración → Impresión → Estaciones. Someone at the till types it into
  **Ajustes → Estación** (or the tray's *Vincular estación…*). The bridge
  redeems it once for a station token and keeps it in
  `%PROGRAMDATA%\GourmelyPrint\config.json`. No file editing, ever.
- **Transport.** Socket.IO v5 over Engine.IO v4, namespace `/print`, spoken by
  hand in `src/engineio.rs` — see the module header for why an off-the-shelf
  client did not survive contact with the real server. Remote API endpoints
  require HTTPS; HTTP is permitted only for explicit loopback development
  endpoints. Pairing never follows redirects. A station paired to an older
  remote HTTP URL stops before sending its token and shows a security warning
  in the settings, tray, and `/health`; re-pair it with the HTTPS URL.
- **Reconnection.** Infinite, exponential, capped at 60 s. A revoked token is
  told apart from a dead link: it backs off to 5 minutes and the UI says "vuelve
  a vincular" instead of "sin conexión".
- **No duplicate tickets.** The server re-delivers anything it did not get an
  ack for. The bridge writes the job id to disk **before** printing, so a crash
  mid-print turns a silent duplicate into a visible "no salió" — see
  `src/job_ledger.rs`.

## Who may print through the local socket

`localAuth` in `config.json`:

| value | meaning |
|---|---|
| `origin-or-secret` | **default.** The caller passes with an allowlisted browser `Origin` **or** with the local secret. A browser cannot forge `Origin`, so a random tab on the till is rejected; the POS keeps working untouched. |
| `secret` | The secret is mandatory for everyone. The end state, once the web sends it. |
| `off` | No check. Support escape hatch. |

The secret is generated on first run and shown under **Ajustes → Estación →
Token local**. It travels as `?token=`, `Authorization: Bearer …` or
`x-gourmelyprint-token`.

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

The relay has an end-to-end test against a **real** GourmelyHub API — pair,
connect, receive a job, print it, ack it. It needs a live server and a fresh
pairing code, so it is `#[ignore]`d; the header of
`src-tauri/tests/relay_e2e.rs` has the exact environment to set. No thermal
printer required: `GOURMELYPRINT_SINK_DIR` writes the ESC/POS bytes to a file,
which is also how you inspect what actually came out.

```bash
cargo test --test relay_e2e -- --ignored --nocapture
```

UI security tests run against Vite with mocked Tauri IPC and the configured
production CSP as a browser response header. They need no model key, backend,
or printer; they do not replace a packaged WebView/MSI smoke test.

```bash
npm run test:e2e
npm run test:csp
npm run test:version
```

## Releases (maintainers)

`.github/workflows/release.yml` builds and publishes the MSI on every
`v*` tag (or manually via workflow dispatch). It signs the MSI for the
Tauri auto-updater and publishes a GitHub Release with a stable-named
asset (`GourmelyPrint-Bridge-setup.msi`). The dashboard download button
and the updater both use the permanent `/releases/latest/download/` URL,
so customers always get the newest build from a single direct-download
link — they never browse this repo.

```bash
# 1. Bump Tauri/Cargo metadata with ops/bump-version.py --set X.Y.Z;
#    bump package.json with npm pkg set version=X.Y.Z and refresh package-lock.json.
#    Confirm all six version values with npm run test:version.
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

On Windows, the bridge protects `%PROGRAMDATA%\GourmelyPrint` and existing
config/print-ledger files for the active console or RDP user, SYSTEM, and
Administrators. An OWNER RIGHTS entry prevents a legacy owner from restoring
access by rewriting the ACL. An already protected directory or file cannot
be reassigned to another active Windows user. If ACL repair fails, readable
config remains available but settings writes stop; `/health.configuration_security`
and the status panel explain the repair.

For installation or repair, stop the bridge, install binaries as Administrator
**without launching the bridge**, then start it only from the cashier's
interactive session. Never launch it from an SSH or administrator session.
For a legacy ACL that still needs elevation, keep the cashier's session active,
run the bridge once elevated from that session, close the elevated instance,
and start it normally as the cashier. If the status says files belong to
another Windows user, do not reassign the ACL manually; close other sessions
and use the correct cashier session. Confirm `/health.configuration_security`
is `null` before pairing or changing settings; do not edit tokens by hand.

If the status instead reports damaged `config.json`, elevation alone cannot
repair invalid JSON. Stop the bridge and make a restricted-access backup of
`%PROGRAMDATA%\GourmelyPrint\config.json`; restore a known-good copy or
rename the damaged file, then restart and re-pair the station. The bridge
does not overwrite the damaged file while reporting this error.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT — see [LICENSE](./LICENSE).
