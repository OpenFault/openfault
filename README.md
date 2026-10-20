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

Coming in M6.

## iOS app pairing

Coming in M7.

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

## AI-assisted development

OpenFault is built by Skcript with heavy use of AI coding agents (Claude, via Claude Code). A person sets the direction, reviews the plan, and verifies each milestone; agents write most of the code, tests and docs. Every change still goes through the test suite, lint, and a human reviewing the result before it reaches `main`. If you find something that looks wrong, open an issue. We would rather know than guess.

## License

MIT. See [LICENSE](LICENSE).
