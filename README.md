<p align="center">
  <img src="assets/unclean-icon.png" alt="Unclean icon" width="180">
</p>

# Unclean

Unclean is a Windows tool for reviewing and changing which Unreal Engine plugins start enabled.
Its desktop and console programs use the same Rust library. The public repository is
[HakanErunsal/Unclean](https://github.com/HakanErunsal/Unclean).

## Product boundary

Unclean:

- discover installed and source-built Unreal Engine directories;
- scan plugin descriptors without changing them;
- show declared and effective plugin state;
- load portable TOML presets;
- produce a reviewable plan before requesting administrator access;
- resolve a `.uproject` engine association and combine engine defaults with project overrides;
- apply presets or individual plugin overrides to one `.uproject`;
- list new-project templates and apply suppression to an explicit template selection;
- build and apply one preset as separate reviewed transactions across selected engines;
- back up, replace, verify, journal, and restore every changed descriptor;
- expose engine, project, and template operations through `unclean.exe` and
  `unclean-gui.exe`.

Unclean does not install plugins, download content, run builds, or contact a network service.
Engine mode may edit only `EnabledByDefault` in recognized plugin descriptors. Project mode may
edit only `DisableEnginePluginsByDefault` and explicit `Plugins` entries in the selected
`.uproject`. Template mode may edit only `DisableEnginePluginsByDefault` in selected
`.uproject` files below the chosen engine's `Templates` directory.

## Release status

Unclean has no published binary release. Build it from source. The repository includes
conservative [starter presets](presets/README.md) for Windows desktop work and neutral
review-first or project-first workflows.

## Console contract

The console program works from Command Prompt, PowerShell, scripts, and CI:

```text
unclean engines
unclean plugins --engine 5.8
unclean presets
unclean preset show my-template
unclean preset validate .\team-preset.toml
unclean plan --engine 5.8 --preset my-template
unclean apply --engine 5.8 --preset my-template
unclean status --engine 5.8
unclean restore --engine 5.8 --snapshot 2026-07-14T09-12-00
unclean project plugins --project .\MyGame.uproject
unclean project plan --project .\MyGame.uproject --preset my-template
unclean project apply --project .\MyGame.uproject --disable InventedPlugin
unclean project history --project .\MyGame.uproject
unclean templates --engine 5.8
unclean template plan --engine 5.8 --template TP_Blank --suppression enabled
unclean template apply --engine 5.8 --template TP_Blank --suppression enabled
unclean template history --engine 5.8
unclean gui
```

An unavailable operation returns exit 78 with an actionable text or JSON error.
`plan` remains read-only. Write commands show the same plan used by the desktop interface.
Noninteractive writes require explicit confirmation through `--yes`.

## Build from source

Install the pinned Rust toolchain through rustup, then run:

```text
cargo build --workspace
cargo test --workspace
cargo run -p unclean-cli -- --help
node scripts/check-public-text.mjs
```

The workspace pins Rust 1.97.1 and records Rust 1.92 as its minimum supported version.

## Safety model

Changes under an engine installation may require administrator access. Unclean browses and plans
without elevation, then gives a narrow request to an elevated worker after confirmation. The
worker revalidates paths, source hashes, permitted fields, and planned output before it writes.
Project files use the current user's access. Engine plugin descriptors and engine templates use
the engine elevation boundary when their installation requires it.

Every write operation creates a full-content backup before the first target change. Each
replacement is atomic on supported Windows filesystems and verified from disk. A failed operation
stops, rolls back changed files where possible, and preserves recovery material.

## Contributing and security

Read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a change. Report suspected
vulnerabilities through the private process in [SECURITY.md](SECURITY.md). The project conduct
rules are in [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## Privacy

Unclean runs without accounts, telemetry, analytics, advertising, update checks, or other
network communication. [PRIVACY.md](PRIVACY.md) records its data behavior.

## License

Source is available under either the [Apache License 2.0](LICENSE-APACHE) or the
[MIT License](LICENSE-MIT), at your option. The repository accepts synthetic test fixtures only.

## Trademark notice

Unclean is an independent open-source project. Epic Games does not sponsor or endorse it.
Unreal and Unreal Engine are trademarks or registered trademarks of Epic Games, Inc. in the
United States of America and elsewhere. The project does not use Epic Games logos.
