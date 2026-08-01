# Contributing

Unclean accepts focused changes that preserve its narrow write authority and shared CLI and GUI
behavior.

## Before editing

1. Read [README.md](README.md) and [SECURITY.md](SECURITY.md).
2. Open an issue before changing product scope, supported platforms, writable fields, elevation
   behavior, compatibility, or recovery semantics.

## Change rules

- Keep core behavior in the shared library. Frontends may format results but must not own
  discovery, planning, writing, or restore rules.
- Treat every path, preset, descriptor, saved plan, and elevated request as untrusted input.
- Keep read-only operations unelevated.
- Revalidate paths, source hashes, writable fields, and intended state inside the elevated
  worker.
- Preserve descriptor bytes outside the requested field edit.
- Add fault tests for every new filesystem boundary.
- Use synthetic fixtures. Do not commit files copied from Unreal Engine installations,
  marketplace plugins, customer projects, or private repositories.
- Keep dependencies narrow. Explain each new dependency's source, maintenance, license, and
  security record in the pull request.
- Do not add telemetry, analytics, update checks, advertising, or other network communication
  without an approved design and a privacy policy update.

## Public text

Comments, API documentation, tooltips, UI copy, CLI help, errors, warnings, logs, documentation,
release notes, changelogs, and commit text must use direct, behavior-focused language.

Keep code comments and tooltips on one physical source line. Start errors with the failure, then
state the recovery action. Describe current behavior and keep implementation history out of
comments.

## Tests and review

Each pull request must pass formatting, Clippy with warnings denied, tests, dependency checks,
the repository text check, and required builds.

Run the current local gate with:

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
node scripts/check-foundation.mjs
node scripts/check-public-text.mjs
```

Changes to descriptor editing, backup, replacement, rollback, restore, or elevation need tests
for both success and injected failure. A successful happy-path test does not cover these areas.

Release packaging requires the pinned license tool:

```text
cargo install --locked cargo-about --version 0.9.1 --features cli
```

## Pull requests

Keep each pull request centered on one coherent change. Include:

- the user-visible result;
- the safety properties affected;
- the commands used to verify the change;
- remaining risks or untested conditions;
- sample output or screenshots when interface behavior changes.

Do not include engine source excerpts, private paths, access tokens, descriptor contents from an
installed engine, or customer data.

## Contribution license

Unless stated otherwise, any contribution intentionally submitted for inclusion in Unclean is
licensed under either Apache-2.0 or MIT, at the recipient's option.
