# Scope and requirements

This branch contains a separate Rust desktop plain-text editor prototype. Its visual direction is the compact Windows 11 Notepad layout, not exact visual or behavioral parity.

## Implemented scope

- Plain-text document tabs; compact File, Edit and View menus; status bar; dedicated settings page with a Back control.
- UTF-8 open/save/save-as, including BOM and newline preservation; 16 MiB admission limit.
- Dirty indicators and save/discard/cancel protection for changed tabs and application exit.
- Undo/redo, clipboard commands, select all, literal find/replace and replace all.
- Font selection from discovered TTF/OTF files, size, wrap, zoom, light/dark themes, and persisted appearance preferences without document contents.
- Optional display-only lexical coloring for known source extensions.
- Same-directory atomic replacement, best-effort external-change detection, bounded history, and failed-save retention.
- Default native OpenGL renderer and optional offscreen wgpu capture of production UI widgets.

“Implemented” describes code scope, not full end-to-end validation. See [verification](docs/TEST_REPORT.md) for test coverage and unverified interactions.

## Unsupported or deferred

- Large-file or 1 GB editing; virtualized layout and asynchronous file I/O.
- UTF-16 and legacy encodings; TTC font collections and canonical font-family metadata.
- Rich text, Markdown formatting tools, project navigation, terminals, AI features, language servers or semantic highlighting.
- Printing, autosave/session recovery, crash/power-loss durability guarantees, and mandatory locking against concurrent writers.
- Native-platform certification, installers, accessibility/IME validation and measured release RAM/startup budgets.

## Verification contract

CI is configured for Rust 1.95.0 on Ubuntu 24.04, Windows 2025 and macOS 15. It checks formatting, default/all-feature tests and strict Clippy, and builds the default executable. Linux additionally captures the UI with Mesa software rendering. A workflow definition is not evidence of a passing run. Native dialogs, clipboard and OS-window interaction still require manual checks on each target platform.
