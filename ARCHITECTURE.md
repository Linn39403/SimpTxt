# Rust Notepad prototype architecture

This is a separate editor prototype on the `feature/notepad-lite` branch of the SimpTxt repository. It does not embed the existing SimpTxt implementation. The branch preserves the prototype as a distinct application; its working title is not a finalized product name.

## Scope and stack

Rust 2024 with eframe/egui 0.36.2 supplies a cross-platform native desktop window and immediate-mode controls; rfd 0.17.2 supplies platform file dialogs. The design is inspired by Windows 11 Notepad, not a native WinUI reproduction. There is no embedded browser, rich-text document model, language server, project tree, terminal, or IDE feature set.

The compact slice comprises document tabs, File/Edit/View menus, a settings gear, a plain-text editor, status information, theme/font controls, open/save/save-as, undo/redo, literal find/replace, unsaved-change confirmation, and optional basic source coloring. Implementation and validation reports, rather than this design, determine which features actually work.

## Boundaries

- Document state owns each tab's text, path, saved baseline, and history. Disk operations must not clear dirty state until successful. Failed or canceled save must not close a dirty document.
- The application coordinates menu commands, active tab, settings, and close/save/discard/cancel flows.
- The editor renders editable plain text and reports cursor/selection state. Coloring changes layout only, never document contents.
- `fonts.rs` discovers readable TTF/OTF files from conventional OS font locations and installs a chosen font in the `editor` family with fallback fonts.
- `highlight.rs` produces an egui layout job for a deliberately limited lexical approximation and exposes an extension allow-list. This is display-only, not parsing or semantic analysis.

## Honest limits

The text buffer is a whole UTF-8 String, not a paged or virtualized text engine. Full-buffer layout, searching, history and saved-baseline copies can all consume time and memory proportional to document size. This prototype makes **no 1 GB performance claim** and is not suitable for unbounded files. A production editor needs explicit size admission limits, asynchronous I/O, bounded history, incremental layout and measurement on actual target hardware.

System-font discovery is best effort. File stems are display labels, not a full font-family/style database. Font collections, variable-font controls, platform font registration APIs, and complete script shaping are outside this slice. Bundled egui fallbacks remain necessary. Theme tracking and window chrome may differ by platform.

A Linux build or headless screenshot cannot establish Windows or macOS correctness. Native dialogs, shortcuts, close handling, font rendering, accessibility, IME input, and packaging require platform-specific validation. Atomic replacement, external modification conflicts, encoding detection, newline fidelity, crash recovery, and symlink behavior must be verified separately before relying on the prototype for important files.

## Validation approach

Keep model tests independent of the UI where possible. Exercise dirty-state transitions, undo/redo, Unicode search/replace, save failures and cancellation. Check layout output reproduces source text exactly. Build the desktop binary and use headless smoke screenshots only as Linux UI evidence. Do not conflate successful compilation with end-to-end usability.

## Renderer feature split

The default `native-glow` feature uses eframe's OpenGL renderer. `headless-capture` separately enables wgpu and the screenshot helpers (`image` and `pollster`). With both features enabled, native startup explicitly selects Glow; capture uses its independent offscreen wgpu path. A build with neither renderer feature is unsupported. To use only wgpu, use `--no-default-features --features headless-capture`.

An offline Linux normal-dependency graph comparison of equivalent manifests found 135 unique nodes for Glow and 161 for wgpu (including the temporary audit package in each). The wgpu-only set had 31 nodes, including wgpu-core, wgpu-hal, naga and ash; the Glow-only set had five nodes, including glutin and egui_glow. These are dependency-graph counts, **not** release-size or runtime-memory measurements. The stack still includes shared text/font shaping, windowing, clipboard and file-dialog dependencies.

The earlier debug screenshot measurement includes a software LLVM graphics driver, offscreen rendering and PNG encoding; it is not native startup or steady-state editor memory. Switching renderer does not guarantee a particular memory reduction. OpenGL may itself use a software driver on this machine. Driver compatibility, native initialization, visual parity, and actual release footprint must be measured separately. The optional capture path may differ from the default native renderer.

Source of feature semantics: locally cached eframe 0.36.2 Cargo.toml and src/epi.rs (Renderer default picks wgpu when both renderers are enabled).

## Settings design reference

Microsoft's original Windows 11 Notepad redesign describes a dedicated settings page containing theme and font controls, with rounded corners and Mica styling elsewhere in the application: https://blogs.windows.com/windows-insider/2021/12/07/redesigned-notepad-for-windows-11-begins-rolling-out-to-windows-insiders/ . This historical primary source supports a full-page settings direction, not a claim of pixel-accurate parity with current Notepad releases. A Back control, Settings title and grouped appearance/font cards are this prototype's design choices. Modern Notepad has added features outside this intentionally plain-text scope.
