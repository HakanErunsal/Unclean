# Starter presets

The archive includes an opt-in Windows desktop preset plus two neutral presets.

- `review-first.toml` builds a no-change engine plan. Add exact names only after reviewing the
  selected engine and dependency results.
- `project-first.toml` pairs with project suppression. Set suppression to `enabled`, then add
  explicit project overrides for plugins the project needs.
- `windows-desktop-lean.toml` disables non-Windows platform and XR plugins plus niche source
  editors. Use it for Windows-only desktop work. It keeps Visual Studio support. Rider and Visual
  Studio Code remain available.

Unreal Engine 5.5 through 5.8 installations supplied the reviewed plugin names. Exact names keep
version differences visible as unmatched rules. Review the plan before applying it. Packaged
builds list these presets in the desktop selector and resolve their file names in the console.

Pass an explicit path in the console:

```text
unclean plan --engine 5.8 --preset .\presets\review-first.toml
unclean plan --engine 5.8 --preset .\presets\windows-desktop-lean.toml
unclean project plan --project .\MyGame.uproject --preset .\presets\project-first.toml --suppression enabled
```

An extracted release also accepts the bundled name:

```text
unclean plan --engine 5.8 --preset windows-desktop-lean
```

The desktop **Open** action accepts the same files.
