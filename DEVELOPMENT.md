# Development

OpenFault is one Rust binary with SQLite inside. No database server, no Node, no build step for the UI.

## Prerequisites

- Rust stable with `clippy` and `rustfmt` (edition 2024; CI uses the latest stable).
- That's it. SQLite is bundled through `rusqlite`.
- Optional: Docker, to build the image.

## Run it

```sh
cargo run
```

It listens on http://localhost:8080 and keeps data in `./data`. On first boot, with no users yet, the log prints a one-time setup token (`ofs_…`). Then either:

- open http://localhost:8080/setup and walk the wizard, or
- send the token as `Authorization: Bearer <token>` to `POST /api/v1/setup`, or to the MCP endpoint, where the only tool is `setup_instance`.

Want a project ready without the wizard? Debug builds seed a `dev` project with a fixed DSN key:

```sh
OPENFAULT_DEV_DSN_KEY=devkey cargo run
```

## Environment

All optional. Settings in the UI cover most of these; env wins when set.

| Variable | What it does |
|---|---|
| `OPENFAULT_DATA` | Data directory. Default `./data`. |
| `OPENFAULT_PORT` | Listen port. Default `8080`. |
| `OPENFAULT_BIND` | Listen address. Default `::` (dual-stack). |
| `OPENFAULT_BASE_URL` | Public URL used in links, DSNs, emails and pushes. |
| `OPENFAULT_SETUP_TOKEN` | Choose the one-time setup token yourself. |
| `OPENFAULT_SMTP_URL` | `smtp://user:pass@host:587?from=...` (`smtps://` for TLS). |
| `OPENFAULT_PUSH_RELAY_URL`, `OPENFAULT_PUSH_RELAY_TOKEN` | Push relay for iOS notifications. |
| `OPENFAULT_BACKUP_S3_*` | S3/R2 backup settings (`_ENDPOINT`, `_REGION`, `_BUCKET`, `_PREFIX`, `_ACCESS_KEY`, `_SECRET_KEY`, `_HOUR`, `_KEEP`). |
| `OPENFAULT_TRUST_PROXY` | `1` trusts `X-Forwarded-For` behind a reverse proxy. |
| `OPENFAULT_ALLOW_PRIVATE_WEBHOOKS` | `1` allows webhooks to localhost and private IPs. |
| `OPENFAULT_DEV_DSN_KEY` | Debug builds only: seed the `dev` project. |
| `OPENFAULT_TIME_OFFSET_FILE` | Tests only: shifts the server clock. Never set it in production. |

## Test and lint

Same as CI:

```sh
cargo test --locked
cargo clippy -- -D warnings
cargo fmt --check
```

Slower checks, ignored by default:

```sh
cargo test --release -- --ignored load                     # 10k-event load check
cargo test --release --test soak -- --ignored --nocapture  # ~13 min, 72 simulated hours
```

## CLI

The same binary has a few subcommands:

```sh
openfault healthcheck           # GET /healthz, exit 0 on 200
openfault backup now            # run one backup
openfault backup list           # list backups
openfault restore <key>         # restore a backup (server must be stopped)
openfault relay                 # run the APNs push relay
```

With cargo: `cargo run -- backup list`.

## Docker

```sh
docker build -t openfault .
docker run -p 8080:8080 -v openfault-data:/data openfault
```

The Dockerfile cross-compiles static musl binaries for amd64 and arm64. CI publishes `ghcr.io/openfault/openfault` on every push to `main`.

## Push relay

`src/relay.rs` is a tiny APNs relay that ships in the same image. Run it with your own Apple key:

```sh
APNS_KEY_P8_FILE=./AuthKey_ABC123.p8 APNS_KEY_ID=ABC123 APNS_TEAM_ID=TEAM123456 \
APNS_BUNDLE_ID=com.example.openfault RELAY_TOKEN=secret PORT=8081 \
cargo run -- relay
```

`APNS_SANDBOX=1` targets the development APNs host. Point your server at it with `OPENFAULT_PUSH_RELAY_URL` and `OPENFAULT_PUSH_RELAY_TOKEN`. Full guide at [openfault.dev](https://openfault.dev).

## Other repos

- [openfault-ios](https://github.com/OpenFault/openfault-ios): the iOS app and widgets. Generate the project with `xcodegen generate`, then build the `OpenFault` scheme in Xcode.
- [openfault.dev](https://github.com/OpenFault/openfault.dev): the docs site (Astro). `bun install && bun run dev`.

## Where things live

- `src/ingest`: Sentry envelope and store endpoints.
- `src/api`, `src/mcp`: REST API and MCP server, sharing the same handlers.
- `src/ui`, `templates/`, `static/`: server-rendered UI (htmx, plain CSS).
- `src/notify`: email, push and webhooks.
- `src/schema.sql`: the database schema.
