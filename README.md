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

Open http://localhost:8080. The first visit runs a setup wizard that creates the admin and the first project.

## Point your SDK at it

Only the DSN changes.

- Rails: `config.dsn = "http://<key>@your-host/1"` in the Sentry initializer.
- Next.js: set `dsn: "http://<key>@your-host/1"` in `sentry.client.config.ts`, `sentry.server.config.ts` and `sentry.edge.config.ts`.

## Development

```sh
~/.cargo/bin/cargo run
```

Tests: `cargo test` (includes a SIGKILL crash-safety test and property tests). The 10k-event load check is ignored by default: `cargo test --release -- --ignored load`.

| Variable | Default | Meaning |
|---|---|---|
| `OPENFAULT_DATA` | `./data` | Directory for the SQLite database and generated secret |
| `OPENFAULT_PORT` | `8080` | HTTP listen port |

## First-boot wizard

Coming in M0.

## Inviting teammates

Coming in M2.

## MCP setup (Claude Code, Cursor)

Coming in M4.

## Source maps (`sentry-cli`)

Coming in M5.

## Email (SMTP)

Coming in M2.

## Push notifications

OpenFault notifies on four triggers: new issue, regression, unmute (mute expired or volume threshold hit) and spike (an issue's hour is ≥ 20 events and over 10× its 24h hourly median, at most once per 6h). Each user picks email and push per trigger in Settings → Notifications (or `PUT /api/v1/me/notifications`). Every user gets at most one email/push per issue per trigger per hour. Webhooks (Slack, Discord, generic JSON) are set per project. Failed deliveries are retried 3 times and kept for 24h in the project's delivery log.

| Env | Meaning |
|---|---|
| `OPENFAULT_BASE_URL` | Public URL used for links in emails, pushes and webhooks. |
| `OPENFAULT_PUSH_RELAY_URL` | Push relay to send iOS pushes through, e.g. `https://push.openfault.dev`. Push is off unless this and the token are set. |
| `OPENFAULT_PUSH_RELAY_TOKEN` | Bearer token for that relay. |
| `OPENFAULT_ALLOW_PRIVATE_WEBHOOKS` | `1` allows webhook URLs on localhost, private or link-local IPs (and plain `http://` to loopback). Off by default. |

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

Coming in M8. Everything lives in the `/data` volume.

## Memory and sizing

Coming in M8.

## Design

The web UI follows Karthik's shadcn preset b1VlJj2Q (style Luma, neutral grayscale): Inter for all text, Phosphor icons, and the preset's `:root`/`.dark` tokens, reimplemented in plain CSS (`static/app.css`, no Tailwind or JS framework).

## AI-assisted development

OpenFault is built by Skcript with heavy use of AI coding agents (Claude, via Claude Code). A person sets the direction, reviews the plan, and verifies each milestone; agents write most of the code, tests and docs. Every change still goes through the test suite, lint, and a human reviewing the result before it reaches `main`. If you find something that looks wrong, open an issue. We would rather know than guess.

## License

MIT. See [LICENSE](LICENSE).
