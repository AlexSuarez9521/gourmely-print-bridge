# TLS cert ops

This directory holds local-only state for managing the Let's Encrypt
cert that GourmelyPrint Bridge uses on every customer's machine:

```
ops/
├── credentials/      # Cloudflare API token (.ini) — gitignored
├── letsencrypt/      # certbot config/work/logs root — gitignored
└── logs/             # certbot log output — gitignored
```

All three subfolders are listed in `.gitignore` and **must never be
committed**.

## Cert details

- Domain: `localhost.gourmelyhub.busticco.com`
- Resolves to: `127.0.0.1` (Cloudflare A record, DNS-only, no proxy)
- Challenge: DNS-01 via `certbot-dns-cloudflare`
- Lifetime: 90 days (Let's Encrypt standard)

The issued `fullchain.pem` + `privkey.pem` are copied into
`src-tauri/certs/` and embedded into the binary via `include_bytes!()`
at compile time. Same cert ships to every customer because the domain
only resolves to their loopback.

## Renewing without a rebuild (v0.1.3+)

Since v0.1.3 the bridge reads a certificate from disk when one is there, and
only falls back to the pair compiled into the binary. An owner-approved disk
replacement can rescue an **existing** installation without a new release.
A fresh installation still needs a released binary with a valid embedded pair.
On an authorized till:

```powershell
# As administrator, on the machine running the bridge
$dir = "$env:PROGRAMDATA\GourmelyPrint\certs"
New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item fullchain.pem, privkey.pem $dir
# Restart the bridge from the tray (Salir, then reopen) so TLS picks it up
```

Confirm which one is live — the bridge logs `TLS material resolved` with
`source=disk` or `source=embedded` at startup, and `GET /health` still answers
over the renewed cert.

A bad file can't take printing offline: the PEM is parsed before it is trusted,
and anything wrong (truncated, cert and key swapped, half a pair) logs and keeps
the embedded certificate. See `src-tauri/src/tls_material.rs`.

> **A disk replacement is not the normal publication path.** The embedded
> pair is what a fresh install uses. Merge the renewal PR and publish a new
> bridge version before the embedded pair reaches the 30-day CI runway gate.
> Never assume an existing till's disk PEM makes a new installer safe.

## Automated renewal (preferred)

GitHub Actions checks `.github/workflows/cert-renew.yml` every Monday at
03:00 UTC and issues only when the embedded certificate has at most 45 days
left. The workflow creates a PR; it does **not** publish a new bridge. For the
certificate expiring 2026-12-30 08:41 UTC, the first scheduled check inside
that window is 2026-11-16. The 30-day CI gate requires a replacement before
approximately 2026-11-30; allow time for review and the owner's release.
The workflow:

1. Installs certbot + `python3-certbot-dns-cloudflare` on the runner.
2. Issues a fresh cert via DNS-01 using the Cloudflare token secret.
3. Overwrites the pem files in `src-tauri/certs/`.
4. Bumps the patch version in `tauri.conf.json` + `Cargo.toml`.
5. Opens a PR against `main`.

After review, obtain explicit owner approval before merging the PR and creating
the `v<new-version>` tag. The tag triggers `release.yml`, which publishes the
release automatically; there is no separate promotion gate. The owner verifies
the published signed MSI and updater artifacts afterward. This procedure only
documents those operations; work on bridge v0.1.7 does not authorize them.

### Required repo secrets

| Secret                     | Purpose                         | Scope                                                                                                                                   |
| -------------------------- | ------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| `CLOUDFLARE_DNS_API_TOKEN` | DNS-01 challenge                | `Zone:Read` + `DNS:Edit` limited to the `busticco.com` zone                                                                             |
| ~~`CERT_RENEWAL_PR_TOKEN`~~ | **No longer used** — removed 2026-07-30 | It was a PAT, it expired silently, and because the secret still existed the `\|\|` fallback never fired: checkout failed with `could not read Username` and the renewal never ran. The workflow now uses `GITHUB_TOKEN`, which cannot expire. The cost is that the renewal PR does not trigger `ci.yml`; dispatch it by hand on that PR if you want the check, and `release.yml` compiles everything from the tag anyway |

Set these under **Settings → Secrets and variables → Actions**.

### Manual dry-run

Trigger the workflow with `dry_run = "true"` from the Actions UI to
exercise the DNS challenge without consuming a real Let's Encrypt
issuance. Useful for verifying Cloudflare token scope or DNS
propagation after changes.

### Review and publication checklist

1. Before requesting issuance, record the embedded certificate's SAN and
   expiry: `openssl x509 -in src-tauri/certs/fullchain.pem -noout -enddate -ext subjectAltName`.
   Run `npm run test:certificate`; it checks the hostname, matching private
   key, and at least 30 days of remaining validity without printing key bytes.
2. Only with the owner's approval, dispatch the renewal workflow or use the
   manual fallback. A dry run exercises DNS but does not prove the real
   certificate-copy and PR path.
3. Review the new PEM pair, patch version in all manifests/lockfiles, and
   `cargo test --locked` / `cargo clippy --locked --all-targets -- -D warnings`
   / `cargo fmt --all -- --check`. Because a `GITHUB_TOKEN`-authored PR does not
   automatically trigger CI, dispatch `ci.yml` on the PR branch and require
   its Windows TLS and release-metadata checks to pass.
4. The owner decides whether and when to merge, tag, publish, and install.
   Record the published version and date; do not count certificate issuance
   or PR creation as completed protection for a fresh installation.
5. If the renewal path fails, escalate before the 30-day gate. Disk PEM
   replacement is an owner-approved fallback for existing installations only;
   it does not repair an expired embedded certificate in a fresh installer.

## Manual renewal (fallback)

If the workflow is unavailable, renew locally on a Linux host
(certbot's Windows story is poor):

```bash
# One-time setup
sudo apt-get install -y certbot python3-certbot-dns-cloudflare
mkdir -p ops/credentials
chmod 700 ops/credentials
cat > ops/credentials/cloudflare.ini <<EOF
dns_cloudflare_api_token = <your-token>
EOF
chmod 600 ops/credentials/cloudflare.ini

# Issue / renew
sudo certbot certonly \
  --non-interactive \
  --agree-tos \
  --email alexsuarez9521@gmail.com \
  --dns-cloudflare \
  --dns-cloudflare-credentials ops/credentials/cloudflare.ini \
  --dns-cloudflare-propagation-seconds 30 \
  --config-dir ops/letsencrypt \
  --work-dir ops/letsencrypt/work \
  --logs-dir ops/logs \
  -d localhost.gourmelyhub.busticco.com

# Copy into the bridge + bump version + commit
LIVE=ops/letsencrypt/live/localhost.gourmelyhub.busticco.com
sudo cp "$LIVE/fullchain.pem" src-tauri/certs/fullchain.pem
sudo cp "$LIVE/privkey.pem"   src-tauri/certs/privkey.pem
sudo chown "$(id -u):$(id -g)" src-tauri/certs/*.pem
```

Bump `version` in both `src-tauri/tauri.conf.json` and `Cargo.toml`,
commit on a feature branch, open a PR to `main`, then tag the merged
commit to trigger the release pipeline.

## Security notes

- The Cloudflare token must be scoped to `busticco.com` only. Do not
  use a global token.
- `privkey.pem` is intentionally embedded in the binary — see
  [SECURITY.md](../SECURITY.md) for why this is safe.
- Never paste the token into a chat, a commit, or a comment. Set it
  once in repo Secrets and forget it.
