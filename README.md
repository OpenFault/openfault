<p align="center">
  <picture><source media="(prefers-color-scheme: dark)" srcset="static/mark-dark.svg"><img src="static/mark.svg" alt="OpenFault" width="56" height="56"></picture>
</p>

<h1 align="center">OpenFault</h1>

<p align="center">
  A self-hosted, Sentry-compatible error tracker. One container. Built for agents.
</p>

<p align="center">
  <a href="https://github.com/OpenFault/openfault/actions/workflows/ci.yml"><img src="https://github.com/OpenFault/openfault/actions/workflows/ci.yml/badge.svg" alt="Build status"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-black" alt="MIT license"></a>
  <a href="https://github.com/OpenFault/openfault/pkgs/container/openfault"><img src="https://img.shields.io/badge/ghcr.io-openfault%2Fopenfault-black" alt="Docker image"></a>
</p>

<p align="center">
  <a href="https://openfault.dev">Docs</a> ·
  <a href="https://openfault.featureos.app/roadmap">Roadmap</a> ·
  <a href="https://openfault.featureos.app">Feedback</a>
</p>

> Early development. Not ready for production yet.

## Why

- **Your errors, your server.** One container, SQLite inside, under 100 MB of RAM.
- **Drop-in for Sentry.** Keep your SDK. Change the DSN.
- **MCP-first.** Coding agents read and triage issues through the same handlers the UI uses.

## Quick start

```sh
docker run -d --name openfault -p 8080:8080 -v openfault-data:/data ghcr.io/openfault/openfault:latest
```

Or `docker compose up -d` with the [`docker-compose.yml`](docker-compose.yml) in this repo.

Open http://localhost:8080. A short wizard creates your organization, owner and first project, and hands you a DSN. Swap it into your Sentry SDK. Done.

In production, put it behind HTTPS and set `OPENFAULT_BASE_URL`. Upgrades are pull and restart.

## Features

- Sentry SDK ingest, live issues, releases and breadcrumbs.
- MCP server for Claude Code, Cursor and friends.
- Alerts by email, Slack, Discord, Teams and more.
- iOS push through a relay: hosted, or your own.
- Daily S3 or Cloudflare R2 backups.

Configuration, env vars and the full guide live at [openfault.dev](https://openfault.dev).

### iOS crash symbolication

Upload dSYMs and native `sentry-cocoa` crashes resolve to function, file and line. Add a Run Script build phase (Release), with a `write` API token from Settings:

```sh
cd "$DWARF_DSYM_FOLDER_PATH" && for d in *.dSYM; do
  zip -qr "/tmp/$d.zip" "$d"
  curl -fsS -H "Authorization: Bearer $OPENFAULT_TOKEN" -F "file=@/tmp/$d.zip" \
    "https://errors.example.com/api/0/projects/<org>/<project-slug>/files/dsyms/"
done
```

One dSYM per request keeps each upload under the 20 MiB request cap.

Already on Sentry's tooling? `sentry-cli` uploads chunked (no size cap per dSYM); Mach-O only, sources are not stored, so leave off `--include-sources`:

```sh
SENTRY_URL=https://errors.example.com SENTRY_ORG=<org> SENTRY_PROJECT=<project-slug> SENTRY_AUTH_TOKEN=$OPENFAULT_TOKEN sentry-cli debug-files upload "$DWARF_DSYM_FOLDER_PATH"
```

## iOS app

Coming soon to the App Store. Triage from your phone, with push alerts and Home Screen widgets. Pair it from Settings → Devices.

## Roadmap and feedback

See what's next on the [roadmap](https://openfault.featureos.app/roadmap). Ideas, bugs, rough edges: tell us on the [feedback board](https://openfault.featureos.app).

## Contributing

Pull requests are welcome. Small ones merge fast. Start with [CONTRIBUTING.md](CONTRIBUTING.md), and [DEVELOPMENT.md](DEVELOPMENT.md) gets you running locally.

## Sponsors

<p align="center">
  <a href="https://featureos.com">
    <picture><source media="(prefers-color-scheme: dark)" srcset="static/sponsors/featureos-dark.svg"><img src="static/sponsors/featureos.svg" alt="FeatureOS" height="20"></picture>
  </a>
  &nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;
  <a href="https://supportwire.ai">
    <picture><source media="(prefers-color-scheme: dark)" srcset="static/sponsors/supportwire-dark.svg"><img src="static/sponsors/supportwire.svg" alt="SupportWire" height="26"></picture>
  </a>
  &nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;
  <a href="https://www.skcript.com">
    <picture><source media="(prefers-color-scheme: dark)" srcset="static/sponsors/skcript-dark.svg"><img src="static/sponsors/skcript.svg" alt="Skcript" height="28"></picture>
  </a>
</p>

## License

MIT. See [LICENSE](LICENSE).
