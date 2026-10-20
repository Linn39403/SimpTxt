# Workspace feature validation — 2026-10-20

This records the workspace updates on `feature/minimal-editor`, based on `5a3da9686bdd563064d7a17f3de66436f8fac8f7`. It is targeted prototype validation, not full Vim compatibility, v1 acceptance, or a cross-platform performance guarantee.

## Automated checks

The final Linux checks passed:

- `cargo fmt --all -- --check`.
- `cargo clippy --locked --all-targets -- -D warnings`.
- `cargo test --locked --all-targets`: **173 passed**, zero failures; three expensive opt-ins ignored. This includes 83 core unit tests, 85 desktop/UI tests, one randomized regression and four real-file integration tests.
- `cargo test --locked --no-default-features --all-targets`: **88 passed**, zero failures, the same three opt-ins ignored.
- `cargo test --locked --doc`: command passed; no doctests are present.
- `cargo build --locked`.

An independent test pass added **42 workspace regression tests** and reran the original 172-test suite. The portability correction adds one Linux regression, bringing the final local total to 173; these are not repeated counts of distinct tests. Source hashes and the implementation tree are recorded in [provenance](validation/workspace-source-provenance.json); the [sanitized command log](validation/workspace-final-checks.log) records the final checks.

### Feature and safety coverage

- Tabs: independent documents, cursor/selection, scrolling, undo, dirty revisions and registers; duplicate paths/hard links; tab limits; late load/save/search/replace/highlight completion isolation; closed-tab cancellation; Save As ownership and destination collisions.
- Closing and quitting: dirty confirmation, cancellation retaining every tab, revision-specific discard approval, inactive saves, save failures, reload during quit, operation/admission limits and modal sidebar selections. Reload refusal preserves the original dirty bytes and undo history.
- Vim: parser counts/operators, Normal/Insert/Visual input, paired key/text events executing once, IME ownership, readonly/modal exclusion, focus/tab-switch cancellation, UTF-8/BOM/CRLF-safe edits, inclusive Visual copy, per-tab registers, save/quit flow and bounded scans.
- Project sidebar: lazy nonrecursive listing, entry/cache/expansion limits, cancellation, stale root/request results, permission and I/O errors, retry, raw non-UTF-8 path identity, disabled symbolic links/reparse traversal, and viewport-only row rendering.
- Zoom/themes: Ctrl/Command shortcut aliases, reset and repeated bounds, both wheel directions without document scrolling, no shortcut insertion or dirty/undo changes, tab consistency, whole-interface theme switching and palette contrast.
- Matching pairs: all three delimiter types, nesting/mismatches, UTF-8/BOM/CRLF offsets, strings/comments/raw strings, partial or stale lexical indexes, edit/tab cache isolation, scan budgets and nearby pairs on very long Unicode lines.
- Existing core, randomized edit/undo, byte-exact save, failure injection and file-pipeline tests remain in the suite.

## Actual rendering and 1 GB workspace probe

The [dark](images/simptxt.png) and [light](images/simptxt-light.png) screenshots were captured from the shared real `WorkspaceApp` with two tabs, a loaded lazy sidebar, Vim Normal status and matching-brace outlines. Both were visually inspected. They use egui/wgpu and the real font atlas, not a mock UI.

A separate run opened the existing exact **1,000,000,000-byte** fixture with a second `Cargo.toml` tab and the `src` sidebar. The fixture uses a `.txt` extension to measure plain-text readiness, not complete source highlighting. In-process edit paths inserted and undid text at the beginning, middle and end; each undo verified restoration of the saved revision. Vim was displayed, but the exercise temporarily used ordinary editor insertion paths; this is not a 1 GB Vim keystroke benchmark.

| Observation | Result |
|---|---:|
| First completed preview GPU frame | 176.228 ms |
| Fully editable document | 15.612 s |
| Six insert/undo frames, including GPU completion | 10.092–26.734 ms |
| Mean of those six frames | 17.081 ms |
| Peak child RSS | 1,077,284 KiB (1,052.04 MiB) |

This is **one development-profile trial**, with package opt-level 1, unoptimized dependencies and software Vulkan (`llvmpipe`, LLVM 19.1.7) at 1280×800 in the same Linux container described in [the baseline report](validation.md). Cache state and host load were uncontrolled. Timing starts before the open request, **after graphics initialization**, and is not total process launch time. These are not native keyboard, release GUI, p95, or cold-cache results. The preview is fast; the measured development build still takes about 16 seconds to become editable at 1 GB.

[Raw observation log](validation/workspace-1gb.log), [process measurements](validation/workspace-1gb-resources.json), and [fixture hashes](validation/fixture-hashes.json) are retained. Reproduce after generating the source fixture and renaming it to `.txt`:

```sh
cargo run --locked -- --headless-benchmark output.png source-1000000000.txt \
  --vim --folder src --tab Cargo.toml
```

This probe does not save the large fixture. The earlier three-family 1 GB byte-exact save/reopen results are preserved in [the baseline report](validation.md); the document and file-I/O implementation is unchanged in this feature. The final current suite reruns smaller real save/reopen and safety cases. Do not treat old 1 GB timings as newly measured results.

## Cross-platform and remaining qualification

The existing three-OS Actions workflow applies to this branch and includes formatting, strict linting, tests, build and an actual native-window smoke test. The first hosted run passed Linux and Windows, including native smoke, but found a macOS-only test-fixture assumption: APFS rejects invalid-UTF-8 filenames. The correction separates an in-memory Unix raw-path identity test from the Linux filesystem regression, retaining strict filesystem assertions without pretending that macOS accepts those filenames. The corrected source passed [original-branch CI](https://github.com/Linn39403/SimpTxt/actions/runs/39388504754), [PR CI](https://github.com/Linn39403/SimpTxt/actions/runs/39388544191), and [post-merge main CI](https://github.com/Linn39403/SimpTxt/actions/runs/39389072076) on all three platforms, including native-window smoke. It was merged in [PR #1](https://github.com/Linn39403/SimpTxt/pull/1) at `40cfa7d0334e0fbb332e76b63c4d6652af8bfda8`. Later startup/sidebar feedback is recorded [separately](ui-feedback-validation.md). The [earlier baseline run](https://github.com/Linn39403/SimpTxt/actions/runs/39381414868) passed on Linux, Windows and macOS; that is not evidence for this changed source.

The local executor cannot create display-server sockets, so its successful offscreen checks do not establish native-window or OS-input success. Native dialogs, platform IME, accessibility, grapheme/bidi navigation, sustained latency, Windows filesystem identity races and all unusual filesystems still need broader hands-on testing.

The README documents the precise Vim subset, lazy-listing caps, bracket scan limits and shared zoom/theme behavior. The aggregate 2 GiB admission budget counts current document bytes and pending file loads, not all retained undo storage or future edits. At most two active tab-owned file operations are admitted; cancelled detached workers may briefly remain alive until their next cancellation check. These bounds are not a global RAM guarantee. No installer, license, release, or merge is included.
