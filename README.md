# SimpTxt

A minimalist desktop text editor focused on opening files quickly, editing safely, and highlighting source code without IDE clutter.

**Status: early implementation, not a production-ready release.** Windows, macOS, and Linux are the targets. Responsive editing of single files up to 1 GB is a requirement being validated, not a universal performance guarantee. Keep backups when evaluating the app.

![SimpTxt editing Rust source](docs/images/simptxt.png)

*Actual SimpTxt application rendered headlessly using its shared editor UI and egui/wgpu renderer, opening [`examples/welcome.rs`](examples/welcome.rs). This is not a mockup. Offscreen rendering does not validate native window integration or OS input methods.*

## Focus

- Open, edit, Save / Save As, undo / redo, and literal find / replace.
- Source highlighting for Rust, C/C++, and JSON; other files remain plain text.
- A small interface with no AI, embedded terminal, plugin marketplace, or project indexing.
- Immediate bounded preview, followed by background UTF-8 validation and indexing. The preview is read-only until editing is ready.
- A chunked text core, bounded viewport rendering, and background search/highlighting/save work.

See [REQUIREMENTS.md](REQUIREMENTS.md) for the agreed scope, provisional performance targets, assumptions, and acceptance plan. See [validation results](docs/validation.md) for measured local results and remaining gaps.

Three real 1 GB fixture families (source, logs, and a giant single line) passed the local core open/edit/undo/save/reopen pipeline. These were single uncontrolled-cache trials, not GUI latency or cross-platform acceptance results.

## Build and run

Install Rust **1.95.0 or newer**, with the platform's normal Rust linker/build tools. The desktop renderer also needs a working Vulkan, Metal, or Direct3D 12 adapter; a suitable software adapter may be used in headless environments.

```sh
cargo run --locked --release -- path/to/file.rs
```

Omit the file argument to start with a new document. Linux also requires the native windowing libraries listed in [the CI workflow](.github/workflows/ci.yml).

Common shortcuts use Ctrl on Windows/Linux and Command on macOS:

- N: new file; O: open; S: save; Shift+S: save as.
- F: find/replace; G: go to line.
- Z: undo; Shift+Z: redo; A: select all.
- Standard copy, cut, paste, arrow, Home/End, and Page Up/Down controls.

## Test

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo test --locked --no-default-features --all-targets
cargo test --locked --doc
```

The suite covers the document core, randomized edits and undo/redo, UTF-8 boundaries, line endings, file I/O, failure-safe saves, search/highlighting, and in-process editor interactions.

A native-window startup smoke test opens the real event loop, exercises basic editing/undo/redo across rendered frames, and exits:

```sh
cargo run --locked -- --smoke-test
```

The [GitHub Actions workflow](.github/workflows/ci.yml) builds and tests on Linux, Windows, and macOS, including native-window smoke checks. Workflow configuration is not evidence of a passing run; consult the repository's [Actions results](https://github.com/Linn39403/SimpTxt/actions).

### Real 1 GB regression

Run fixture families separately to bound disk use. Each needs roughly twice the fixture size in free temporary storage during saving, plus build storage and sufficient RAM.

```sh
cargo test --locked --release --no-default-features --test large_file \
  one_gigabyte_open_edit_save -- --ignored --exact --nocapture
```

The default family is `source`. Set `SIMPTXT_FIXTURE_FAMILY=logs` or `SIMPTXT_FIXTURE_FAMILY=long` to exercise dense logs or an extremely long line. These are core file-operation tests, not end-to-end GUI latency or cold-cache p95 benchmarks. A manual CI input also enables the heavier file regressions when workflow dispatch is available.

### Reproduce the screenshot

```sh
cargo run --locked -- --headless-screenshot docs/images/simptxt.png examples/welcome.rs
```

This runs the actual editor, waits for the sample's loading/highlighting, and captures its GPU-rendered output without an OS window. It still requires a suitable graphics driver. `WGPU_BACKEND` can select `vulkan`, `metal`, or `dx12` where supported.

## Current boundaries

- UTF-8 is the editable encoding. Invalid UTF-8 or NUL-containing input is not silently converted into editable text. Named symbolic links are rejected rather than followed unexpectedly.
- No soft wrapping. Very long lines are displayed in bounded horizontal regions.
- Clipboard/individual insertion size is limited to 16 MiB; undo history is limited to 1,000 actions. Large-file support does not mean unlimited clipboard or undo memory.
- This prototype has a 2 GiB document/output safety ceiling; replace-all expansion is checked before allocation. This is a safety limit, not a claim that 2 GiB has been performance-qualified.
- Literal search only, with a 1 MiB search-pattern limit and a 1,000,000-match replace-all limit. Operations exceeding safety limits report an error instead of silently truncating content.
- Highlighting may temporarily fall back to plain text while lexical state is prepared. C/C++ backslash-newline/trigraph cases conservatively use plain text instead of claiming complete language support.
- Bundled font coverage is limited; some CJK, emoji, and other scripts may display missing-glyph boxes even when the underlying UTF-8 bytes are preserved.
- Unicode codepoint editing is implemented; full grapheme-cluster, bidirectional-text, IME, and accessibility behavior still needs broader qualification.
- Saving writes a separate temporary file and checks for external changes before replacement. Unix mode bits are preserved; Windows ACLs, alternate streams, all filesystem metadata, and race-free external-change detection are not guaranteed. Filesystem durability varies; retain backups.
- The project is not yet signed, notarized, or packaged as an installer. Cross-platform CI and local headless checks do not replace hands-on testing on each desktop OS.

## License

A project license has not been selected or added yet. Public visibility alone does not grant a general reuse license. Dependency licenses remain their respective authors' licenses.
