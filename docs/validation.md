# Prototype validation — 2026-10-20

These measurements describe the original `feature/minimal-editor` implementation published at `5a3da9686bdd563064d7a17f3de66436f8fac8f7`. See [workspace validation](workspace-validation.md) for the later tabs/Vim/sidebar/theme changes. The original source hashes and logs below are retained as historical evidence, not rerun results for the new branch.

This is evidence for the first implementation, not v1 acceptance or a performance guarantee. The complete acceptance matrix in [REQUIREMENTS.md](../REQUIREMENTS.md), including 20-trial warm/cold p95 measurements and hands-on validation on each supported OS, is still outstanding.

## Environment and method

- Linux x86-64 cloud container, kernel 6.18.55; AMD EPYC 9V74; 9 exposed logical processors; about 9.7 GiB RAM, no swap.
- Overlay filesystem. The backing storage model, host contention, and hardware cache state are not exposed.
- Rust 1.95.0. File-pipeline tests used the optimized `release` profile. The offscreen UI uses the real application and egui-wgpu renderer, not a mockup.
- Each file-pipeline timing below is **one trial**. Fixture generation populated caches; no cold-cache reset was performed. Throughput numbers must not be interpreted as physical-disk performance.
- Peak RSS is the OS-reported maximum resident set for the test child. Private/committed memory and file-backed mappings were not measured separately. The source run's process summary also includes Cargo compilation time; its operation timings do not.

## Real-file checks

The source, log, and single-line fixture families each passed complete pipelines at **10,000,000**, **100,000,000**, and **1,000,000,000 bytes**. The source fixture has a UTF-8 BOM; fixtures include mixed line endings and multibyte text. Source/log fixtures contain many lines; the long-line fixture has no newline.

Each pipeline verifies:

- A bounded read-only preview, separately from complete editable loading.
- Byte-exact no-op saving, including BOM, line endings, and final-newline presence.
- Insertions/replacements at the beginning, middle, and end; undo and redo; literal search for inserted text.
- Cancellation after real save progress, with the original unchanged and temporary output removed.
- Complete edited saving, streaming byte-for-byte comparison with an independently constructed expected output, and reopening.

### Observed 1 GB core timings

| Fixture | Preview read | Editable load | Small edits, observed range | Edited save | Peak RSS |
|---|---:|---:|---:|---:|---:|
| Source | 0.049 ms | 3.277 s | 0.324–6.981 ms | 1.097 s | 958.75 MiB |
| Logs | 0.035 ms | 2.687 s | 0.138–44.008 ms | 0.475 s | 957.72 MiB |
| Single line | 0.027 ms | 1.278 s | 0.105–0.451 ms | 0.265 s | 956.70 MiB |

Preview-read time is **not first-paint time**. Editable load is measured separately and is not instantaneous. The three observed save-cancellation acknowledgments were 0.082–0.754 ms. These sparse observations do not establish p95 input latency, cancellation guarantees, or a memory budget for long editing sessions.

Machine-readable observations: [1 GB JSON](validation/1gb-core-results.json). Sanitized raw operation logs: [source](validation/1gb-source.log), [logs](validation/1gb-logs.log), [single line](validation/1gb-long.log), [10 MB families](validation/10mb.log), [100 MB families](validation/100mb.log). [Exact fixture hashes](validation/fixture-hashes.json) are reproducible from the bounded-memory generator.

```sh
cargo test --locked --release --no-default-features --test large_file \
  one_gigabyte_open_edit_save -- --ignored --exact --nocapture
SIMPTXT_FIXTURE_FAMILY=logs cargo test --locked --release --no-default-features \
  --test large_file one_gigabyte_open_edit_save -- --ignored --exact --nocapture
SIMPTXT_FIXTURE_FAMILY=long cargo test --locked --release --no-default-features \
  --test large_file one_gigabyte_open_edit_save -- --ignored --exact --nocapture
```

Run these separately: an atomic save needs the original and a complete temporary replacement on disk at once. Do not generate all default fixtures when available storage is limited.

## Actual 1 GB offscreen UI probe

The real `EditorApp` and GPU renderer also opened the exact 1,000,000,000-byte source fixture, temporarily named `.txt` to test plain-text readiness rather than a full-file syntax scan. The shared input paths inserted and undid text at the beginning, middle, and end, checking that each insertion changed the document and each undo restored its saved revision. Each input measurement ends after GPU completion, not just command submission.

These are **single dev-profile runs**, with unoptimized dependencies and software Vulkan (`llvmpipe`, LLVM 19.1.7), at 1280 × 800. They are not release-build, native keyboard, or p95 measurements. The timer starts before the editor's open request but after graphics-device initialization; it does **not** measure complete process launch.

| Run | First completed preview frame | Fully editable | Six insert/undo frames | Mean | Peak RSS |
|---|---:|---:|---:|---:|---:|
| Initial unoptimized prototype | 198.912 ms | 11.682 s | 8.811–27.293 ms | 16.380 ms | 1,059.47 MiB |
| Package opt-level 1, first repeat | 195.746 ms | 13.265 s | 8.449–29.694 ms | 16.901 ms | 1,058.79 MiB |
| Final source, package opt-level 1 | 194.214 ms | 13.213 s | 8.096–23.882 ms | 14.279 ms | 1,057.47 MiB |

The final run verifies the corrected eight-digit line-number gutter and clean status after undo. These observations do **not** show that the package-only optimization made loading faster; dependencies remain unoptimized and cache/host conditions are uncontrolled. The read-only preview is quick, but the measured development build takes about 13 seconds to become editable at 1 GB. Full-release GUI and whole-file syntax readiness remain unqualified.

Raw observations: [initial](validation/1gb-ui-unoptimized.log), [first repeat](validation/1gb-ui-opt1-repeat.log), [final](validation/1gb-ui-final.log). Process measurements: [initial](validation/1gb-ui-unoptimized-resources.json), [first repeat](validation/1gb-ui-opt1-repeat-resources.json), [final](validation/1gb-ui-final-resources.json).

To reproduce, generate only the needed source fixture with `scripts/generate_fixtures.py`, rename that fixture to a `.txt` suffix, and run:

```sh
cargo run --locked -- --headless-benchmark output.png source-1000000000.txt
```

The fixture SHA-256 is `0980cd658beeb8619bd6e8cb4932c6915680c1ae821356f552643e962576d034`. Driver selection is machine-specific; the invocation requires a working Vulkan, Metal, or Direct3D 12 adapter. It writes the PNG, not the input file.

## Final automated checks

Formatting, strict Clippy (`-D warnings`), and the desktop build passed. The default-feature suite passed **86 tests**: 60 core, 21 UI/view, one randomized regression, and four file-pipeline tests. The no-desktop suite passed **65 tests**. Three heavier tests are opt-in and were run separately as reported above. The documentation-test command passed with no doctests present.

[Sanitized final command log](validation/final-checks.log) and [source hashes/implementation commit](validation/source-provenance.json) record the checked source. The independent stress run below is additional coverage, not 150,000 separate unit tests.

## Correctness and failure coverage

Core tests cover immutable snapshot sharing, UTF-8 chunk seams, sparse newline checkpoints, local typing coalescing, million-match replacement compaction, undo history, projected-size overflow, and the 2 GiB safety ceiling. UI-state tests cover readonly input, Unicode cursor boundaries, CRLF deletion, clipboard operations, dirty-close choices, modal input exclusion, stale worker completions, and saved-snapshot revision tracking.

The independent randomized regression passed **25,000 arbitrary byte/chunk validation cases and 125,000 edit operations**, including bulk changes and full undo/redo. Reproduce the larger run with:

```sh
SIMPTXT_STRESS=1 cargo test --locked --no-default-features \
  --test document_randomized -- --nocapture
```

Save tests cover cancellation, short writes, external changes before/during saving, create-new races, symlinks, Unix permissions, and an interrupted child process. Disk exhaustion is **fault-injected**, not tested by filling the host disk. Process interruption does not establish survival of every power-loss/filesystem failure. Metadata conflict checks have a documented check-to-rename race; directory sync and filesystem durability vary.

## Desktop and rendering qualification

The checked-in screenshot is produced by the real shared application UI, font atlas, syntax highlighter, and GPU renderer in offscreen mode. Native-window smoke is a separate check in the three-OS GitHub Actions workflow. This local executor cannot create the display-server sockets needed for a native X window, so local offscreen success must not be reported as native-window success.

Full grapheme/visual-column navigation, bidirectional text, accessibility, platform IME behavior, all native dialogs, and sustained interactive latency remain subject to broader desktop qualification. Syntax tests establish supported lexical behavior and safe fallback; these file-pipeline timings do not measure whole-file syntax readiness.
