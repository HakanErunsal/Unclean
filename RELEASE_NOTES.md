# Unclean 0.1.0-alpha.2

This first public alpha supports Windows 11 x86-64. The Windows executables are unsigned.
Verify the published SHA-256 checksum and GitHub build provenance before running them.

## Included

- Review and change Unreal Engine plugin defaults from the desktop application or console.
- Discover launcher installations and source-built engines.
- Load, save, validate, and apply TOML presets.
- Preview each engine, project, or template change before writing.
- Read project plugin state and resolve the engine associated with a `.uproject` file.
- Apply project-specific plugin overrides without changing engine defaults.
- Back up changed files, record operation history, and restore reviewed snapshots.
- Start with the bundled review-first, project-first, or Windows desktop presets.

## Release limits

- Write features are alpha quality. Close Unreal Editor and related tools before applying a plan.
- Engine discovery and plugin loading may take time on installations with hundreds of descriptors.
- Maintainer acceptance covers Unreal Engine 5.5 through 5.8. Versions 5.3 and 5.4 may appear as partial installations.
- The application has no automatic updater and makes no network requests.

## Download contents

The Windows archive contains `unclean-gui.exe`, `unclean.exe`, starter presets, licenses,
third-party notices, privacy and security policies, release notes, and a source revision
manifest. The release also provides a checksum file and CycloneDX SBOMs for both executables.
