# Contributing to OpenFault

Thanks for being here. OpenFault gets better one pull request at a time, and yours counts. Open a PR. Small ones merge fast.

## Ideas, bugs and questions

- Ideas and feature requests: [openfault.featureos.app](https://openfault.featureos.app). Vote on what you want next.
- Bugs: [GitHub issues](https://github.com/OpenFault/openfault/issues). Steps to reproduce beat paragraphs.
- What's planned: [openfault.featureos.app/roadmap](https://openfault.featureos.app/roadmap).

Not sure an idea fits? Open an issue first and ask. A two-line question saves a two-day PR.

## Your first PR

1. Fork, then set up locally with [DEVELOPMENT.md](DEVELOPMENT.md).
2. Make one change. One fix or one feature per PR.
3. Add a test that fails without your change.
4. Run what CI runs:

   ```sh
   cargo test --locked
   cargo clippy -- -D warnings
   cargo fmt --check
   ```

5. Open the PR against `main`. Say what changed and why, in a few lines.

## What gets merged

- One change, with tests, and CI green.
- Plain code over clever code. No new dependency for something a few lines can do.
- UI changes stay black and white, use the tokens in `static/app.css`, and work in light and dark. Screenshots help.
- Docs updated when behavior changes.

## Commit messages

Short, lowercase area, colon, what changed:

```
ingest: precise item-length errors
live: tick on project/release/member changes
api: device sessions see full issue details without fix approval
```

## License

By contributing you agree your work is released under the [MIT license](LICENSE).

Contribute away.
