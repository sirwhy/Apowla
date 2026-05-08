# rpow-miner

Multithreaded Rust miner for [rpow3.com](https://rpow3.com), the modern tribute
to Hal Finney's Reusable Proofs of Work.

It fetches challenges from `https://api.rpow3.com/challenge`, hashes
`SHA-256(nonce_prefix || nonce_le_8bytes)` across every CPU core, and submits
the first nonce whose digest has at least `difficulty_bits` trailing zero bits
back to `POST /mint`. Designed to run 24/7 — fully unattended with retries,
backoff, graceful shutdown and a tiny HTTP `/stats` endpoint.

The mining algorithm is byte-for-byte compatible with the official browser
miner shipped at `https://rpow3.com/assets/miner.worker-*.js` (SHA-256 with
the same little-endian 8-byte nonce encoding and the same trailing-zero-bit
counting convention). The server side is the open-source
[`frkrueger/rpow`](https://github.com/frkrueger/rpow) project.

> Heads up: the project was originally hosted at `rpow2.com`. It has since
> rebranded to `rpow3.com`. This miner targets `rpow3.com` by default but
> still understands the legacy `RPOW2_*` environment variable names.

## How it works

```
client                                              server (api.rpow3.com)
------                                              ----------------------
                                  POST /challenge
                  ───────────────────────────────────────►
                                  { challenge_id, nonce_prefix (hex), difficulty_bits }
                  ◄───────────────────────────────────────

  for nonce in 0..u64::MAX (split across N threads):
      digest = sha256(prefix_bytes || nonce.to_le_bytes())
      if trailing_zero_bits(digest) >= difficulty_bits: break

                              POST /mint
                              { challenge_id, solution_nonce }
                  ───────────────────────────────────────►
                                  { token: { id } }
                  ◄───────────────────────────────────────
```

`trailing_zero_bits` walks the digest from `digest[31]` to `digest[0]`, adding
8 for each fully-zero byte and `byte.trailing_zeros()` for the first non-zero
byte (LSB-first within the byte). This matches the official worker exactly.

The server (see [`apps/server/src/pow.ts`](https://github.com/frkrueger/rpow/blob/main/apps/server/src/pow.ts))
verifies the same way with `Buffer.alloc(8)` LE encoding.

## Requirements

- An email address. **You do not need to register at rpow3.com first** — the
  server auto-creates an account on the first magic-link verification, so the
  built-in `login` subcommand below is also the registration flow.
- Either:
  - Docker (recommended for VPS / Railway), or
  - Rust 1.85+ if you want to build natively.

## Quick start (local)

```bash
# 1) Build
cargo build --release

# 2) Register / log in (sends magic link, exchanges it for a session cookie)
./target/release/rpow-miner login --email you@example.com
# Open the link in your inbox, copy its URL, paste it back at the prompt.
# The command prints something like:
#   RPOW_COOKIE='rpow_session=eyJhbGciOi...'

# 3) Mine
export RPOW_COOKIE='rpow_session=eyJhbGciOi...'   # paste from step 2
./target/release/rpow-miner
```

You should see logs like:

```
INFO starting rpow-miner api_base=https://api.rpow3.com threads=8 status_port=8080
INFO authenticated to rpow3.com email=you@... balance=12 minted=12
INFO received challenge challenge_id=... difficulty_bits=28
INFO FOUND solution; submitting to /mint nonce=4218945 trailing_bits=29 elapsed_ms=312
INFO minted token token_id=...
INFO [stats] uptime=30s hashes=520400000 hashrate=17.34MH/s minted=4 ...
```

## Register / log in (`login` subcommand)

```
rpow-miner login [--email YOU@example.com]
```

Walks you through the rpow3 magic-link flow end-to-end:

1. Prompts for your email (or use `--email` / `RPOW_LOGIN_EMAIL`).
2. Calls `POST /auth/request` so the server sends you a magic link.
3. You open the email and paste the verification URL back at the prompt
   (right-click the link in your email → "Copy link address" works fine).
4. The CLI does a single `GET` of that URL with redirects disabled, captures
   the `rpow_session` cookie from the `Set-Cookie` header, and prints the
   exact value you should set as `RPOW_COOKIE` (in your shell or in the
   Railway service variables).

**No prior account is required.** The first successful verification creates
your user record on the server. The session cookie is valid for ~30 days; if
it ever expires, just re-run `rpow-miner login`.

If your environment doesn't have a TTY (e.g. you're running this inside a
non-interactive shell), set `RPOW_LOGIN_EMAIL` and the CLI will skip the
email prompt; the verify-URL prompt still requires stdin. For Railway, do the
login locally on your laptop and only set `RPOW_COOKIE` in the Railway
variables — Railway's runtime is non-interactive.

## Self-test (no auth needed)

To verify your build is hashing correctly and benchmark the hashrate without
contacting the server:

```bash
RPOW_SELFTEST=1 RPOW_SELFTEST_BITS=24 ./target/release/rpow-miner
```

This solves a synthetic 24-bit-difficulty challenge with the exact same hash
function the server uses, then prints the hashrate and exits.

## Configuration

All configuration is via environment variables. Each `RPOW_*` name also
accepts the legacy `RPOW2_*` form (for users who already configured the
previous miner version that targeted `rpow2.com`).

| Variable | Required | Default | Description |
|---|---|---|---|
| `RPOW_COOKIE` | yes (mining mode) | – | Cookie header value, e.g. `rpow_session=eyJ...`. Obtain via `rpow-miner login`. Multiple cookies semicolon-separated also work. |
| `RPOW_API_BASE` | no | `https://api.rpow3.com` | API base URL. |
| `RPOW_ORIGIN` | no | `https://rpow3.com` | Origin/Referer header. |
| `RPOW_USER_AGENT` | no | `rpow-miner/0.1 ...` | User-Agent header. |
| `RPOW_THREADS` | no | all logical CPUs | Number of mining worker threads. |
| `RPOW_LOG` | no | `info` | `tracing` filter, e.g. `debug`, `rpow_miner=debug`. |
| `PORT` / `RPOW_STATUS_PORT` | no | `8080` | HTTP status server port. Railway injects `PORT` automatically. |
| `RPOW_STATUS_DISABLED` | no | unset | Set to `1` to disable the HTTP status server. |

## Status HTTP endpoints

The miner exposes a tiny HTTP server (so platforms like Railway can health-check
it) on `PORT` (or `8080`):

- `GET /` — text status (`rpow-miner OK`)
- `GET /health` — returns `ok`
- `GET /stats` — JSON snapshot:

```json
{
  "uptime_secs": 1234.5,
  "total_hashes": 21000000000,
  "hashrate_per_sec": 17050000.0,
  "challenges_fetched": 4,
  "tokens_minted": 4,
  "mint_failures": 0,
  "current_difficulty": 28,
  "last_solution_ms": 1820,
  "last_token_unix_ts": 1762593900
}
```

## Deploy to Railway

This repo is Railway-ready. Two-minute path:

1. **Get your cookie** locally (only takes one minute, no account needed):
   ```bash
   cargo run --release -- login --email you@example.com
   # …click the link in your email, paste it back, copy the printed cookie.
   # First-time? This automatically creates your rpow3 account.
   ```
   You can do this on any machine — even a laptop. The cookie is portable.
2. **Push** the code to a GitHub repo of yours.
3. In [Railway](https://railway.app/), click **New Project → Deploy from
   GitHub repo** and pick the repo. Railway detects the `Dockerfile` and
   `railway.toml` automatically.
4. Open the new service → **Variables** → add:
   - `RPOW_COOKIE` = the value printed by `rpow-miner login`
     (looks like `rpow_session=eyJ...`).
   - *(Optional)* `RPOW_THREADS` if you want to cap cores.
5. **Deploy**. Watch the deploy logs — you should see
   `authenticated to rpow3.com email=you@...` within a few seconds, then
   mining logs.

The service exposes `/health` and `/stats` on the Railway-injected `$PORT`, so
you'll see a healthy green dot in Railway's dashboard once it boots.

### Cookie expiry

The rpow3 session cookie is valid for ~30 days (no refresh flow). If the
miner ever sees `401 Unauthorized` from `/mint` or `/challenge`, it logs an
error and sleeps 60 seconds between retries — it does **not** crash-loop the
container. To recover, run `rpow-miner login` again, copy the new cookie, and
update the `RPOW_COOKIE` Railway variable. The service will auto-restart and
resume mining.

## Run with Docker (any VPS)

```bash
docker build -t rpow-miner .

# Optional: log in inside the image (interactive, one-shot).
# This prints the cookie you need for the long-running container below.
docker run --rm -it rpow-miner login --email you@example.com

# Long-running miner.
docker run -d --name rpow-miner --restart=unless-stopped \
  -e RPOW_COOKIE='rpow_session=PASTE...' \
  -p 8080:8080 \
  rpow-miner
docker logs -f rpow-miner
curl localhost:8080/stats
```

## Run as a systemd service (bare metal)

```ini
# /etc/systemd/system/rpow-miner.service
[Unit]
Description=rpow miner
After=network-online.target
Wants=network-online.target

[Service]
Environment=RPOW_COOKIE=rpow_session=PASTE...
Environment=RPOW_LOG=info
ExecStart=/usr/local/bin/rpow-miner
Restart=always
RestartSec=5
User=miner
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now rpow-miner
journalctl -u rpow-miner -f
```

## Architecture

- `src/main.rs` — entrypoint: arg parsing (`login` / `--help` / `--version`),
  env config, auth probe, periodic heartbeat, signal handling
  (SIGINT/SIGTERM), mining supervisor.
- `src/api.rs` — typed `reqwest`-based client for the rpow API
  (`/me`, `/challenge`, `/mint`, `/auth/request`, `/auth/verify`). Cookie-based
  auth using the server's `rpow_session` cookie.
- `src/miner.rs` — multithreaded SHA-256 brute-forcer. Each worker `i` of `N`
  searches `nonce ∈ {i, i+N, i+2N, ...}` to avoid collisions and contention.
  Workers report progress via an atomic counter and stop via an
  `AtomicBool` + `oneshot` channel as soon as anyone finds a valid nonce.
- `src/server.rs` — minimal `axum` HTTP server for `/health` + `/stats`.
- `src/stats.rs` — atomic counters shared between miner workers and the HTTP
  server.
- `src/config.rs` — env-var parsing (with `RPOW2_*` legacy-name fallback).

The mining hot loop hashes 4096 nonces between `stop`-flag checks; on a 2-vCPU
machine the self-test reports ~26 MH/s aggregate (~13 MH/s/core). Real-world
performance scales linearly with cores.

## Caveats

- The `RPOW_COOKIE` is a **bearer credential**. Anyone with that cookie can
  spend tokens from your account. Treat it like a password — never commit it,
  and prefer Railway/your secrets manager over plain env files.
- The miner is single-process. Run multiple containers on multiple machines if
  you want more aggregate hashrate.
- Server-side, supply is capped at 21,000,000 tokens and difficulty grows by
  one bit per 1,000,000 minted (see
  [`apps/server/src/schedule.ts`](https://github.com/frkrueger/rpow/blob/main/apps/server/src/schedule.ts)).
  At the current default of 28 bits a modern x86 core finds a valid nonce in
  about half a minute on average.
