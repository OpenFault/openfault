# OpenFault

OpenFault is a self-hosted, Sentry-compatible error tracker. It ships as one container backed by SQLite, idles under 100 MB of RAM, and is MCP-first, so coding agents can read and triage issues through the same handlers the UI uses. An iOS app is planned for on-call triage.

> **Status: early development, not yet usable.** Nothing here is ready to run in production.

## Install

```sh
docker run -d --name openfault -p 8080:8080 -v openfault-data:/data ghcr.io/openfault/openfault:latest
```

Or with Compose:

```yaml
services:
  openfault:
    image: ghcr.io/openfault/openfault:latest
    ports:
      - "8080:8080"
    volumes:
      - openfault-data:/data
    restart: unless-stopped

volumes:
  openfault-data:
```

Open http://localhost:8080. Nothing needs configuring first. The first visit runs a setup wizard: step 1 creates the organization, the owner and the first project; step 2 (optional, skippable) sets the base URL, SMTP (with an Amazon SES preset and a "Send test email" button) and the push relay; step 3 (optional) sets up S3 backups. All of it stays editable under Settings → Organization.

Prefer an agent or a script? Until the owner exists, the server logs a one-time **setup token** (`ofs_…`, also shown on `/setup`). Use it as `Authorization: Bearer <setup token>` either for `POST /api/v1/setup` (`{"org": {"name"}, "owner": {"email", "name", "password"}, "project": {"name", "platform"}, "smtp"?, "push_relay"?, "base_url"?}` → `{api_token, dsn, project, login_url}`) or for the MCP endpoint, where the only tool is `setup_instance`. Set `OPENFAULT_SETUP_TOKEN` to choose the token yourself. It stops working once the first user exists.

The container's `HEALTHCHECK` runs `openfault healthcheck` (GET `/healthz` on `127.0.0.1:$OPENFAULT_PORT`, exit 0 on 200, else 1), since the distroless image has no shell or curl.

## Point your SDK at it

Only the DSN changes.

- Rails: `config.dsn = "http://<key>@your-host/1"` in the Sentry initializer.
- Next.js: set `dsn: "http://<key>@your-host/1"` in `sentry.client.config.ts`, `sentry.server.config.ts` and `sentry.edge.config.ts`.

## Development

```sh
~/.cargo/bin/cargo run
```

Tests: `cargo test` (includes a SIGKILL crash-safety test and property tests). The 10k-event load check is ignored by default: `cargo test --release -- --ignored load`.

Soak (ignored, ~13 min): `cargo test --release --test soak -- --ignored --nocapture` runs the release binary through 72 simulated hours (200 events/hour, retention 1 day, two SSE clients, resolves and mutes) and checks retention, purges, mute expiry, WAL size, DB size, RSS < 100 MB and a clean log. It fast-forwards the clock with `OPENFAULT_TIME_OFFSET_FILE` (a file holding seconds added to the server's `now()`), which is a test-only hook: never set it in production.

| Variable | Default | Meaning |
|---|---|---|
| `OPENFAULT_DATA` | `./data` | Directory for the SQLite database and generated secret |
| `OPENFAULT_PORT` | `8080` | HTTP listen port |
| `OPENFAULT_BIND` | `::` | Listen address. The default is dual-stack (IPv4 and IPv6); falls back to `0.0.0.0` if IPv6 is unavailable |

## First-boot wizard

See Install: wizard (three steps), `POST /api/v1/setup` or MCP `setup_instance` with the setup token.

## Inviting teammates

Coming in M2.

## MCP setup (Claude Code, Cursor)

Settings → MCP shows the `claude mcp add` command and `mcp.json` snippets for `/mcp` with an API token. Issue tools: `list_projects`, `list_new_issues`, `search_issues`, `get_issue`, `get_event`, `list_releases`, `get_stats`, and with a write token `resolve_issue`, `mute_issue`, `link_fix`.

Onboarding tools: on a fresh server, connect with the setup token and call `setup_instance` (one-time; it returns an owner API token to reconnect with). Owner/admin write tokens also get `create_project` (→ DSN), `invite_member` (→ invite link), `list_members`, `configure_email` + `send_test_email`, and `configure_push_relay`.

## Source maps (`sentry-cli`)

Coming in M5.

## Email (SMTP)

Set SMTP in the wizard or Settings → Organization (host, port, username, password, from, STARTTLS/TLS/none; Amazon SES preset per region), or via MCP `configure_email`. "Send test email" reports the SMTP error inline. Every send is logged (recipient domain only) and the last 24h show under Settings → Organization. `OPENFAULT_SMTP_URL` (`smtp://user:pass@host:587?from=OpenFault <noreply@x>`, `smtps://` for implicit TLS) overrides the stored settings.

## Push notifications

OpenFault notifies on four triggers: new issue, regression, unmute (mute expired or volume threshold hit) and spike (an issue's hour is ≥ 20 events and over 10× its 24h hourly median, at most once per 6h). Each user picks email and push per trigger in Settings → Notifications (or `PUT /api/v1/me/notifications`). Every user gets at most one email/push per issue per trigger per hour. Webhooks (Slack, Discord, Mattermost, Teams, Telegram, generic JSON) are set per project. Failed deliveries are retried 3 times and kept for 24h in the project's delivery log.

Env vars are optional overrides of Settings → Organization; when set, the form shows "set by environment" and locks the field.

| Env | Meaning |
|---|---|
| `OPENFAULT_SETUP_TOKEN` | Pre-set the one-time setup token (automation). |
| `OPENFAULT_SMTP_URL` | SMTP URL, overrides the stored SMTP settings. |
| `OPENFAULT_BASE_URL` | Public URL used for links in emails, pushes and webhooks. |
| `OPENFAULT_PUSH_RELAY_URL` | Push relay to send iOS pushes through, e.g. `https://push.openfault.dev`. Push is off unless a relay URL and token are set (here or in Settings → Organization). |
| `OPENFAULT_PUSH_RELAY_TOKEN` | Bearer token for that relay. |
| `OPENFAULT_ALLOW_PRIVATE_WEBHOOKS` | `1` allows webhook URLs on localhost, private or link-local IPs (and plain `http://` to loopback). Off by default. |

- `mattermost`: a Mattermost incoming webhook URL; posts `text` plus one attachment (title link, level/events/users/release fields).
- `teams`: a Teams Workflows (Power Automate) "post to channel when a webhook request is received" URL; posts an Adaptive Card 1.4 with an "Open issue" button.
- `telegram`: `https://api.telegram.org/bot<token>/sendMessage?chat_id=<chat id>`; the chat id is stored separately and the message is sent as HTML with link previews off.

Generic webhooks receive `{"kind", "project", "issue": {"id", "title", "level", "url", "events", "users", "first_seen", "last_seen", "release"}}`. If you set a secret, each request carries `X-OpenFault-Timestamp: <unix seconds>` and `X-OpenFault-Signature: sha256=<hex HMAC-SHA256(secret, "{timestamp}.{raw body}")>`. Verify the signature over the timestamp header, a `.` and the raw body, and reject requests whose timestamp is more than ~5 minutes old (replay protection).

**Hosted relay (App Store app).** The APNs key belongs to whoever ships the iOS build, so your server never holds it. Set `OPENFAULT_PUSH_RELAY_URL=https://push.openfault.dev` and the `OPENFAULT_PUSH_RELAY_TOKEN` you were given. The relay only receives device tokens, the issue title, the project name and the deep link.

**Self-run relay.** The relay is the same image in a second mode:

```sh
docker run -d -p 8081:8080 \
  -e APNS_KEY_P8_FILE=/keys/AuthKey_ABC123.p8 -v $PWD/keys:/keys:ro \
  -e APNS_KEY_ID=ABC123 -e APNS_TEAM_ID=TEAM123456 -e APNS_BUNDLE_ID=com.example.openfault \
  -e RELAY_TOKEN=$(openssl rand -hex 32) \
  ghcr.io/openfault/openfault:latest relay
```

| Env | Meaning |
|---|---|
| `APNS_KEY_P8` / `APNS_KEY_P8_FILE` | The `.p8` key as PEM text (literal `\n` allowed) or a path to it. |
| `APNS_KEY_ID`, `APNS_TEAM_ID` | From your Apple developer account. |
| `APNS_BUNDLE_ID` | The iOS app's bundle id (APNs topic). |
| `RELAY_TOKEN` | Servers must send `Authorization: Bearer <RELAY_TOKEN>`. |
| `APNS_SANDBOX` | `1` sends to the APNs development host (debug builds from Xcode). |
| `PORT` | Listen port, default 8080. |

Endpoints: `POST /v1/push` (`{tokens, title, body, url, collapse_id}` → per-token `{token, status, reason}`; tokens APNs reports as gone are deleted on your server) and `GET /healthz`. The provider JWT is cached and refreshed every 50 minutes.

**Self-built iOS app.** Pushes only reach a build signed with the same team and bundle id as the relay's key. To stay fully self-sovereign, build the open-source `openfault-ios` app with your own Apple account and bundle id, run the relay with your own key, and point your server at it.

## iOS app pairing

In the web UI, open **Settings → Devices → Pair a phone** and scan the QR code with the OpenFault iOS app (or type the 8-character code and host). The phone gets its own write token; remove it from the same page to revoke that token. Codes expire after 10 minutes and work once.

## Deploy

Run it behind a reverse proxy with HTTPS (and so HTTP/2). Every open tab holds one live-update (SSE) connection, and browsers on HTTP/1.1 allow only 6 per host, so a seventh tab stalls. Caddy does it in three lines:

```
errors.example.com {
  reverse_proxy localhost:8080
}
```

Set `OPENFAULT_BASE_URL=https://errors.example.com` so DSNs and links use the public address.

## Upgrade and backup

Everything lives in the `/data` volume (`openfault.db`). Upgrades: pull the new image and restart; the schema migrates on boot.

### Backups (S3-compatible)

Settings → Backups (admins) or wizard step 3: endpoint, region, bucket, prefix, access key, secret key (never shown again), hour (UTC), keep (default 14) and path-style URLs. Presets: Amazon S3 per region, and **Cloudflare R2** (endpoint `https://<account>.r2.cloudflarestorage.com`, region `auto`, path-style on). "Test connection" writes and deletes a marker object; "Back up now" runs one backup (waits up to 30s, then keeps running in the background).

Once a day at `hour_utc:00` the server takes an online SQLite snapshot (the writer is never blocked), compresses it with zstd and uploads `<prefix>/openfault-<YYYYMMDDTHHMMSSZ>.db.zst` with a single SigV4-signed PUT (up to 5 GB), then deletes all but the newest `keep`. Every run is logged; the last 20 and any failures show on the Backups page.

Env vars override the stored settings field by field (setting the bucket also turns backups on and makes the form read-only): `OPENFAULT_BACKUP_S3_ENDPOINT`, `_REGION`, `_BUCKET`, `_PREFIX`, `_ACCESS_KEY`, `_SECRET_KEY`, `_HOUR`, `_KEEP`.

CLI (same `OPENFAULT_DATA`):

- `openfault backup now` runs one backup with the database settings (or env) and prints the key.
- `openfault backup list` lists backups (key, bytes).
- `openfault restore <key|s3://bucket/key>` downloads, decompresses and integrity-checks a backup, then writes it to `<data>/openfault.db`. It reads credentials **from env only** (`OPENFAULT_BACKUP_S3_*`), since the database being replaced may be gone. The previous database is kept as `openfault.db.pre-restore-<time>`.

Restore steps: stop the server (`restore` refuses while `/healthz` answers), run `restore` with the env vars set (the Backups page shows the exact command for the latest backup), start the server. With Docker: `docker compose stop openfault && docker compose run --rm -e OPENFAULT_BACKUP_S3_ENDPOINT=… -e OPENFAULT_BACKUP_S3_REGION=… -e OPENFAULT_BACKUP_S3_BUCKET=… -e OPENFAULT_BACKUP_S3_ACCESS_KEY=… -e OPENFAULT_BACKUP_S3_SECRET_KEY=… openfault restore <key> && docker compose start openfault`.

## Memory and sizing

Coming in M8.

## Design

The web UI follows Karthik's shadcn preset b1VlJj2Q (style Luma, neutral grayscale): Inter for all text, Phosphor icons, and the preset's `:root`/`.dark` tokens, reimplemented in plain CSS (`static/app.css`, no Tailwind or JS framework).

## AI-assisted development

OpenFault is built by Skcript with heavy use of AI coding agents (Claude, via Claude Code). A person sets the direction, reviews the plan, and verifies each milestone; agents write most of the code, tests and docs. Every change still goes through the test suite, lint, and a human reviewing the result before it reaches `main`. If you find something that looks wrong, open an issue. We would rather know than guess.

## License

MIT. See [LICENSE](LICENSE).
