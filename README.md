<p align="center">
  <img src="static/mark.svg#gh-light-mode-only" alt="OpenFault" width="56" height="56"><img src="static/mark-dark.svg#gh-dark-mode-only" alt="OpenFault" width="56" height="56">
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

## iOS app

Coming soon to the App Store. Triage from your phone, with push alerts and Home Screen widgets. Pair it from Settings → Devices.

## Roadmap and feedback

See what's next on the [roadmap](https://openfault.featureos.app/roadmap). Ideas, bugs, rough edges: tell us on the [feedback board](https://openfault.featureos.app).

## Contributing

Pull requests are welcome. Small ones merge fast. Start with [CONTRIBUTING.md](CONTRIBUTING.md), and [DEVELOPMENT.md](DEVELOPMENT.md) gets you running locally.

## Sponsors

<p align="center">
  <a href="https://featureos.com">
    <img src="static/sponsors/featureos.svg#gh-light-mode-only" alt="FeatureOS" height="20"><img src="static/sponsors/featureos-dark.svg#gh-dark-mode-only" alt="FeatureOS" height="20">
  </a>
  &nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;
  <a href="https://supportwire.ai">
    <img src="static/sponsors/supportwire.svg#gh-light-mode-only" alt="SupportWire" height="26"><img src="static/sponsors/supportwire-dark.svg#gh-dark-mode-only" alt="SupportWire" height="26">
  </a>
  &nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;
  <a href="https://www.skcript.com">
    <img src="static/sponsors/skcript.svg#gh-light-mode-only" alt="Skcript" height="28"><img src="static/sponsors/skcript-dark.svg#gh-dark-mode-only" alt="Skcript" height="28">
  </a>
</p>

## License

MIT. See [LICENSE](LICENSE).
