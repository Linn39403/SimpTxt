# SimpTxt

A minimalist desktop text editor focused on opening files quickly, editing safely, and highlighting source code without IDE clutter.

**Status: early implementation, not a production-ready release.** Windows, macOS, and Linux are the targets. Responsive editing of single files up to 1 GB is a requirement being validated, not a universal performance guarantee. Keep backups when evaluating the app.

### Dark

![SimpTxt dark theme with tabs, project files, Vim mode and matching brackets](docs/images/simptxt.png)

### Light

![SimpTxt light theme with the same file and workspace](docs/images/simptxt-light.png)

*Actual SimpTxt application rendered headlessly using its shared editor UI and egui/wgpu renderer, opening [`examples/project/src/main.cpp`](examples/project/src/main.cpp). This is not a mockup. Offscreen rendering does not validate native window integration or OS input methods.*

## Focus

- Independent file tabs with open, edit, Save / Save As, undo / redo, and literal find / replace.
- A lazy, read-only project-folder sidebar. No project-wide scan or indexing.
- Optional Vim-style editing, editor text zoom, and dark/light themes.
- Bounded, syntax-aware matching-pair highlighting for `()`, `[]`, and `{}`.
- Source highlighting for Rust, C/C++, and JSON; other files remain plain text.
- A small interface with no AI, embedded terminal, plugin marketplace, or project indexing.
- Immediate bounded preview, followed by background UTF-8 validation and indexing. The preview is read-only until editing is ready.
- A chunked text core, bounded viewport rendering, and background search/highlighting/save work.

See [REQUIREMENTS.md](REQUIREMENTS.md) for the agreed scope, provisional performance targets, assumptions, and acceptance plan. See [input, memory and performance follow-up validation](docs/review-fixes-validation.md), [startup/sidebar feedback validation](docs/ui-feedback-validation.md), [workspace validation](docs/workspace-validation.md) for the earlier feature set and [baseline large-file results](docs/validation.md) for earlier measurements and remaining gaps.

Three real 1 GB fixture families (source, logs, and a giant single line) passed the local core open/edit/undo/save/reopen pipeline. These were single uncontrolled-cache trials, not GUI latency or cross-platform acceptance results.

## Build and run

Install Rust **1.95.0 or newer**, with the platform's normal Rust linker/build tools. The desktop renderer also needs a working Vulkan, Metal, or Direct3D 12 adapter; a suitable software adapter may be used in headless environments.

For an existing clone, update the original feature branch before building:

```sh
git fetch origin
git switch feature/minimal-editor
git pull --ff-only
cargo run --locked --release
```

This command opens a new untitled document. To open the checked-in sample instead, use `cargo run --locked --release -- examples/welcome.rs`. If the startup path is missing, SimpTxt opens a clean Untitled document without creating the missing file. Linux also requires the native windowing libraries listed in [the CI workflow](.github/workflows/ci.yml).

Common shortcuts use Ctrl on Windows/Linux and Command on macOS:

- N: new file; O: open; S: save; Shift+S: save as.
- F: find/replace; G: go to line. In Find, Enter searches forward, Shift+Enter backward, and Escape returns to editing. F3 / Shift+F3 repeat the last search in either direction, wrapping once.
- Z: undo; Shift+Z: redo; A: select all.
- W: close the current tab; Tab / Shift+Tab: next / previous tab.
- `+` (or `=`), `−`, and mouse wheel: editor text zoom while Ctrl is held; Ctrl 0 resets. Command shortcuts are also accepted on macOS.
- Standard copy, cut, paste, arrow, Home/End, and Page Up/Down controls. Ctrl+Left/Right moves by word; Ctrl+Backspace/Delete deletes a word (Option is also accepted on macOS).
- Plain Tab indents selected lines; Shift+Tab removes one leading tab or up to four spaces. Enter carries forward the current line's indentation. Adjacent ordinary typing is grouped for undo until a pause or another action. Undo/redo restores the caret and selection.

Use **Light theme** / **Dark theme** in the top toolbar to switch the entire interface without changing the documents. Text zoom is shared across tabs and bounded to 9–36 px. Native launches remember theme, zoom and Vim mode in a small user-local preferences file. Documents, file paths and workspace sessions are not stored. Malformed or unsupported preference files are left unchanged, with a nonfatal notice.

Use **Open folder…** for the sidebar and **Close Folder** beside it to close the project without closing tabs or discarding unsaved edits. Folder arrows and open/closed folder icons are painted shapes, not icon-font glyphs. Source-file badges distinguish C/C++, headers, Rust, Python, JSON, TOML and CMake; unknown types use a generic document icon. Hover the project name to see its full path. Reopening a file focuses its existing tab, preserving unsaved edits; Save As refuses a destination already owned by another tab. Closing the window walks all unsaved tabs. Cancelling quit retains every tab, including tabs whose discard decision had already been made for that quit attempt.

Drop files onto the window to open or focus their tabs. **Reload** explicitly reads the current file from disk and requires confirmation before discarding unsaved edits. Background metadata checks show a notice when a file changes or disappears; they never automatically replace either a clean or dirty buffer. Checks are not a content hash or filesystem lock.

Preferences are saved atomically as `SimpTxt/preferences.conf` under `%APPDATA%` on Windows, `~/Library/Application Support` on macOS, or `$XDG_CONFIG_HOME` (falling back to `~/.config`) on Linux. Headless captures and smoke tests use deterministic defaults and do not read or write these settings.

### Optional Vim-style mode

Enable **Vim mode** in the toolbar. It starts in Normal mode; the footer shows Normal, Insert or Visual and any `:` command. Mode/pending commands reset when switching tabs or leaving editor focus. A fresh preferences file defaults to dark theme and ordinary editing; later native launches restore the saved settings.

Implemented subset:

- Motions: `h j k l`, arrow keys, `0 ^ $`, `w b e`, `gg`, `G`, with bounded numeric counts.
- Insert: `i I a A o O`, then Escape to Normal. Ordinary text/IME input is used in Insert mode.
- Characterwise Visual: `v`, motions, then `d`, `c`, `y` or `x`; Escape cancels.
- Operators: `d`, `c`, `y` with supported motions, including `dd`, `cc`, `yy`; `x` deletes characters without crossing the line ending.
- `p` / `P` use the tab-local unnamed register, `u` undoes, Ctrl+R redoes.
- `:w`, `:q`, `:wq`, `:q!`. Quit commands close the current tab. `:q` prompts for unsaved edits; `:wq` closes only after a successful current-revision save; explicit `:q!` discards that tab's unsaved edits, but refuses during an active file operation.

This is **not full Vim compatibility**. There are no mappings, macros, marks, text objects, named registers, dot-repeat, Vim search, visual-line/block mode, counted insert commands, or Visual replacement paste. Word motions use Unicode alphanumeric/underscore versus punctuation classes, not configurable Vim options. Motion scans are bounded to 64 KiB, counts to 1,000, and edit/register operations to 16 MiB; exceeding a safety limit reports an error without a partial operation. Standard clipboard paste is available in Insert mode; Normal-mode `p` is not the system clipboard. Undo groups adjacent ordinary typing, not a whole Vim Insert session. Vertical movement retains a preferred Unicode scalar column across short lines, rather than a grapheme/display column.

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
cargo run --locked -- --headless-screenshot docs/images/simptxt.png examples/project/src/main.cpp \
  --vim --folder examples/project --tab examples/welcome.rs --cursor-byte 168
cargo run --locked -- --headless-screenshot docs/images/simptxt-light.png examples/project/src/main.cpp \
  --light --vim --folder examples/project --tab examples/welcome.rs --cursor-byte 168
```

A missing startup path opens a clean, usable Untitled editor without a banner or dialog and without creating a file. See the [clean startup capture](docs/ui-feedback-validation.md#startup-recovery). Other opening errors and real save failures remain visible.

This runs the actual editor, waits for the sample's loading/highlighting and lazy folder listings, and captures its GPU-rendered output without an OS window. It still requires a suitable graphics driver. `WGPU_BACKEND` can select `vulkan`, `metal`, or `dx12` where supported.

## Current boundaries

- UTF-8 is the editable encoding. Invalid UTF-8 or NUL-containing input is not silently converted into editable text. Named symbolic links are rejected rather than followed unexpectedly.
- No soft wrapping. Very long lines are displayed in bounded horizontal regions.
- At most 128 tabs are open at once. Loads and edits reserve against a 2 GiB tracked document-memory budget, including shared backing bytes, retained undo and document index/metadata storage. Shared snapshots are counted once, including background jobs after a tab closes. At most two file operations run concurrently. This is not a process RSS or OS out-of-memory guarantee: graphics, syntax/search buffers and allocator overhead are outside this accounting.
- Folder listing is lazy and bounded: 256-entry increments, at most 2,048 entries per directory, 8,192 cached entries and 128 expanded directories. Partial listings are visibly labeled; there is no promise of unlimited project browsing.
- Matching-pair searches examine at most 24 KiB in one direction, with total lexical replay/scan under 64 KiB. Missing lexical state or a distant match yields no guessed highlight; a status message distinguishes an unknown result.
- Clipboard/individual insertion size is limited to 16 MiB. Undo/redo is limited to 1,000 actions and 128 MiB of unique history-only storage, including metadata; shared live bytes are not charged again. Older history is evicted first. An edit whose own undo record exceeds the budget is rejected before mutation with an actionable error; the existing document and history remain intact. Large-file support does not mean unlimited clipboard or undo memory.
- This prototype has a 2 GiB document/output safety ceiling; replace-all expansion is checked before allocation. This is a safety limit, not a claim that 2 GiB has been performance-qualified.
- Literal search only, with a 1 MiB search-pattern limit and a 1,000,000-match replace-all limit. Operations exceeding safety limits report an error instead of silently truncating content.
- Block indentation is limited to 16,384 lines per action, copied autoindent to 4,096 bytes, IME surrounding deletion to 4,096 Unicode scalars per side, and word scans to 64 KiB. Exceeding a bound leaves the document unchanged.
- Highlighting reuses verified unchanged checkpoints after local edits and batches visible rows. First loads, expired change history or genuinely changed lexical state may still require a cancellable background suffix/full scan. Highlighting may temporarily fall back to plain text while lexical state is prepared. C/C++ backslash-newline/trigraph cases conservatively use plain text instead of claiming complete language support.
- Bundled font coverage is limited; some CJK, emoji, and other scripts may display missing-glyph boxes even when the underlying UTF-8 bytes are preserved.
- Unicode codepoint editing is implemented; full grapheme-cluster, bidirectional-text, IME, and accessibility behavior still needs broader qualification.
- Saving writes a separate temporary file and checks for external changes before replacement. Unix mode bits are preserved; Windows ACLs, alternate streams, all filesystem metadata, and race-free external-change detection are not guaranteed. Filesystem durability varies; retain backups.
- The project is not yet signed, notarized, or packaged as an installer. Cross-platform CI and local headless checks do not replace hands-on testing on each desktop OS.

## License

A project license has not been selected or added yet. Public visibility alone does not grant a general reuse license. Dependency licenses remain their respective authors' licenses.
