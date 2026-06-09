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

## Automated renewal (preferred)

GitHub Actions runs `.github/workflows/cert-renew.yml` every 60 days.
The workflow:

1. Installs certbot + `python3-certbot-dns-cloudflare` on the runner.
2. Issues a fresh cert via DNS-01 using the Cloudflare token secret.
3. Overwrites the pem files in `src-tauri/certs/`.
4. Bumps the patch version in `tauri.conf.json` + `Cargo.toml`.
5. Opens a PR against `main`.

After merging the PR, tag `v<new-version>` and push it to trigger
`release.yml`, which builds the new MSI, signs it for the auto-updater,
and publishes it to a GitHub Release (the dashboard + updater use the
permanent `/releases/latest/download/` URL).

### Required repo secrets

| Secret                     | Purpose                         | Scope                                                                                                                                   |
| -------------------------- | ------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| `CLOUDFLARE_DNS_API_TOKEN` | DNS-01 challenge                | `Zone:Read` + `DNS:Edit` limited to the `busticco.com` zone                                                                             |
| `CERT_RENEWAL_PR_TOKEN`    | Open the PR from the new branch | PAT or app token with `repo` scope — needed because `GITHUB_TOKEN` will not trigger downstream workflow runs from a PR opened by itself |

Set these under **Settings → Secrets and variables → Actions**.

### Manual dry-run

Trigger the workflow with `dry_run = "true"` from the Actions UI to
exercise the DNS challenge without consuming a real Let's Encrypt
issuance. Useful for verifying Cloudflare token scope or DNS
propagation after changes.

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
