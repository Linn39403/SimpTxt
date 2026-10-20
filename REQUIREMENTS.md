# SimpTxt Requirements

Status: initial v1 requirements. License, implementation language, and UI framework are undecided.

## 1. Goal and scope

Build a useful, open-source desktop text editor inspired by Sublime Text's focus and responsiveness. Open a file, read it with syntax highlighting, edit it, and save it safely. A single file up to **1 GB** (provisional benchmark interpretation: 1,000,000,000 bytes) must remain practical to use, not merely possible to load.

Requirement origins:
- **User:** explicitly requested.
- **Derived:** proposed essentials needed to make the requested editor usable.
- **Assumption:** an initial boundary to validate, not an explicit user decision.

### In scope

- **S-01 [User]:** Minimal interface, fast file opening, source-code syntax highlighting, and files up to 1 GB.
- **S-02 [User]:** Full text editing and safe saving in v1, including large files. A large-file viewer alone does not satisfy v1.
- **S-03 [Assumption]:** Desktop Windows, macOS, and Linux. Supported OS versions and minimum hardware remain TBD.
- **S-04 [Assumption]:** Local, regular text files. Network drives, remote files, and special filesystem objects are outside the initial performance benchmark scope.

### Non-goals for v1

No IDE features, AI, plugin system, embedded terminal, project-wide indexing, language servers, debugging, or build integration. Do not add feature breadth at the expense of the core file-editing workflow. Tabs, sessions, themes, and other convenience features are not release requirements.

## 2. Core behavior

- **F-01 [User]: Open fast.** Show a useful, interactive first viewport without waiting for a complete file read, line count, syntax scan, or other whole-file processing. Display actual content, not just an empty editor or loading screen.
- **F-02 [User]: Edit large files.** Insert, delete, select, copy, cut, and paste at the beginning, middle, and end of files up to 1 GB. Scrolling, horizontal navigation, and small edits must not require repeated whole-file copies or scans.
- **F-03 [Derived]: Basic editing controls.** Provide keyboard navigation, selection, standard platform shortcuts, undo/redo, and an obvious modified-file indicator. Group undo into understandable user actions. Document any history limit.
- **F-04 [Derived]: Find and replace.** Provide literal text find, next/previous match, replace current, and replace all. Whole-file work must be cancellable. Replace all must be undoable as one action; cancellation must leave either the original document or a clearly reported, undoable result. Regex and project-wide search are out of scope.
- **F-05 [User]: Save and Save As.** Save all edits, including those near the end of a 1 GB file. Keep the editor responsive and report completion or failure accurately.

## 3. Syntax highlighting

- **H-01 [User]:** Automatically highlight supported source-code files, with plain-text mode available. Select and document a small initial language set before implementation; do not imply universal support.
- **H-02 [Derived]:** Prioritize visible content and a small nearby region. Bound individual highlighting tasks and cancel or supersede stale work after edits or navigation.
- **H-03 [Derived]:** Respect multiline lexical state, including comments and strings starting outside the viewport. Do not guess state from an isolated visible fragment and present it as correct.
- **H-04 [Derived]:** If correct state is unavailable or a line exceeds the safe work budget, show readable plain text for the affected region until correct highlighting is ready. The fallback must not hide content or disable editing.
- **H-05 [Derived]:** Editing an early multiline delimiter must invalidate and refresh affected later highlighting without blocking interaction.

## 4. Text and file safety

- **T-01 [Assumption]:** UTF-8 is the initial editable encoding, with or without a BOM. Preserve the BOM. Identify unsupported encodings and invalid UTF-8; open read-only or offer explicit conversion rather than silently replacing bytes.
- **T-02 [Derived]:** Preserve LF, CRLF, mixed line endings, and final-newline presence unless deliberately changed. Do not normalize unrelated content on save. New files default to UTF-8 without BOM and the platform's normal line ending.
- **T-03 [Derived]:** Extremely long lines must remain navigable and editable. Default to no soft wrapping; render the visible horizontal region without laying out an entire enormous line. Handle multibyte text correctly at chunk and viewport boundaries.
- **T-04 [Derived]:** Never silently truncate files or overwrite undecodable/binary content. A no-op open/save of a supported file must preserve its bytes exactly.
- **SAFE-01 [Derived]:** Do not truncate the original before a complete replacement is ready. Use failure-safe saving appropriate to each supported filesystem. Test cancellation, insufficient disk space, permission failure, and process interruption.
- **SAFE-02 [Derived]:** On save failure, retain unsaved edits and a usable original or recovery copy. Never report success after a partial write. Document filesystem durability limitations.
- **SAFE-03 [Derived]:** Detect external file changes and warn before overwriting them. Offer reload, save elsewhere, or explicit overwrite; never silently discard either version.
- **SAFE-04 [Derived]:** Warn before closing or replacing an unsaved document. Preserve existing permissions where supported. Handle symbolic links explicitly rather than silently replacing the link or saving to an unexpected target.

## 5. Responsiveness and resources

- **P-01 [User]:** Opening, scrolling, typing, selection, search, highlighting, and saving must remain usable at the 1 GB boundary.
- **P-02 [Derived]:** Perform expensive work incrementally or in the background. Long operations need honest progress/activity feedback and cancellation where safe.
- **P-03 [Derived]:** Bound memory and queued work. Opening or making a small edit must not require multiple complete in-memory copies. Resource limits must produce recoverable errors, never silent data loss.
- **P-04 [Derived]:** Cancellation must stop unnecessary work promptly. Clearly indicate any short final save-commit step that cannot safely be cancelled.

### Provisional performance targets

These are **proposals to validate, not measured results or guarantees**. Confirm or revise them after the first large-file prototype on a documented reference machine.

- First useful viewport (actual file content): p95 **at most 250 ms warm-cache** and **1 second cold-cache**, measured from an open request in an already-running app.
- Visible response to typing/navigation: p95 **at most 50 ms**, including during background work.
- Cancellation acknowledgment: **at most 100 ms**, with work stopping at the next safe bounded step.
- Memory: establish a numerical budget from the prototype before v1 sign-off. Report peak private/committed memory, resident memory, and file-backed mappings separately.

Measure time until editing is available separately; a temporary read-only loading preview must be clearly identified and must not be represented as full editor readiness. Its acceptable delay remains to be established by the prototype.

Measure app launch, full-file indexing, complete syntax readiness, search completion, and save completion separately. First-viewport timing does not mean these operations have completed. Search/save throughput targets remain TBD until the storage baseline is measured.

## 6. Benchmark and acceptance plan

**Reference machine: TBD.** Record CPU, RAM, storage model/type, OS/build, display settings, application build/configuration, and cache-reset method. Validate each supported OS. No hardware specification or benchmark result is assumed here.

Create reproducible fixtures at **10 MB, 100 MB, and 1 GB**, with exact byte counts and hashes:

1. Source code with typical lines, multiline comments/strings, and edits changing lexical state.
2. Logs with many short lines and mixed ASCII/multibyte UTF-8.
3. Pathological long lines, including a single line spanning essentially the entire file.

Keep cold- and warm-cache trials separate. Use at least 20 recorded trials per timing case when reporting p95. Publish fixture generation, commands, raw results, and exclusions.

At each size and for each fixture family, verify:
- First useful viewport and first editable viewport timings, separately from full processing.
- Scroll/jump to beginning, middle, and end; selection across boundaries; insert/delete/paste; undo/redo; and find/replace, including during background work.
- Input latency, peak memory, background completion times, cancellation, and save throughput.
- Correct highlighting after distant navigation and multiline-state edits, or a clearly indicated plain-text fallback.
- Byte-exact no-op saving; expected output after edits at multiple offsets; preserved BOM/line endings; successful reopen; and no truncation.

Also test external modification, save failure, disk exhaustion, interrupted saving, invalid UTF-8, and unsaved-close warnings. A responsive viewer that fails editing or saving does not pass v1.

## 7. Delivery order and open decisions

1. **Large-file proof first:** demonstrate first-viewport loading, bounded rendering, real editing, undo, and failure-safe saving at 1 GB. Measure before making performance claims.
2. **Core editor:** finish the minimal file workflow, search/replace, supported text behavior, and visible-first correct highlighting.
3. **Application polish:** keyboard behavior, simple UI, packaging, cross-platform validation, and contributor documentation.

Open decisions: open-source license; implementation language, including Rust versus C++; UI framework; initial syntax languages; supported OS versions; reference hardware; and measured performance/memory budgets. No license or framework is selected by this document.
