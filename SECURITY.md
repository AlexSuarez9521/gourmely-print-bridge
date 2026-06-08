# Security

## Reporting a vulnerability

Email **contacto@busticco.com** with details. Please do not open a
public issue for security-sensitive reports. We aim to respond within
72 hours.

## Why the TLS private key is committed to this public repo

`src-tauri/certs/privkey.pem` is the private half of a real Let's
Encrypt certificate for `localhost.gourmelyhub.busticco.com`. It is
**intentionally** versioned and embedded into the compiled binary.

This is safe, and here is the precise reasoning:

1. **The domain only resolves to `127.0.0.1`.** The DNS A record for
   `localhost.gourmelyhub.busticco.com` points at loopback. There is no
   public server anywhere that this certificate authenticates.

2. **The bridge binds to `127.0.0.1:8181` only.** It never listens on a
   routable interface. The only client that can ever complete the TLS
   handshake is software running on the same machine (the GourmelyHub
   POS open in the user's browser).

3. **There is no man-in-the-middle position to exploit.** A stolen
   private key lets an attacker impersonate a server — but the only
   "server" here is the user's own loopback. To abuse the key an
   attacker would already need code execution on the victim's machine,
   at which point the key is the least of anyone's problems.

4. **The certificate exists purely to silence the browser.** Chrome and
   Firefox refuse `wss://` connections to a self-signed or
   plain-`localhost` origin without warnings/popups. A publicly-trusted
   cert on a loopback-only domain is the standard trick (QZ Tray uses
   `localhost.qz.io` the same way) to get a clean, popup-free secure
   WebSocket to a local service.

The certificate is rotated every 60 days by
`.github/workflows/cert-renew.yml`. See [ops/README.md](ops/README.md).

## What is NOT in this repo

- No customer data, no POS database, no API secrets.
- No Cloudflare token, no GitHub PAT — those live only in repo Actions
  secrets and are never printed in logs.
- No Tauri updater **private** signing key — only the public half is in
  `tauri.conf.json`. The private key is held offline and injected into
  CI via the `TAURI_SIGNING_PRIVATE_KEY` secret at release time.

## Telemetry

The bridge itself sends no telemetry. The GourmelyHub POS web app may
report the bridge's version and printer count (read from the bridge's
local `/health` endpoint) to the GourmelyHub backend, so operators can
see which branches are running which version. No print job contents,
printer names, IPs, or MAC addresses are ever transmitted.
