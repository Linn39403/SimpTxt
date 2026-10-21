# Verification report

Validated on Linux with Rust 1.95.0 and egui/eframe 0.36.2. This is not Windows certification.

- 37 automated tests passed in both default-native and all-feature configurations: 16 document/preferences tests, 15 application/font/highlighting tests, 6 independent filesystem safety tests.
- Real egui editor input and undo tested in-process, not just the document model.
- Dirty tab close, explicit cancellation state, per-dirty-tab exit prompting, failed-save retention, invalid-open retention, Unicode search selection tested.
- UTF-8 rejection, BOM and CRLF/mixed newline preservation, Unicode lowercase matching, bounded undo, nonrecursive replace-all, external-change detection, deleted-original protection, failed-save cleanup, read-only protection, symlink/permission preservation tested.
- Strict Clippy passes for both default and all-feature configurations. Formatting checks pass.
- Settings round-trip, malformed input fallback, isolated rendering, and preservation of unsaved text across the full-page settings screen are covered. Tests and captures never load or save real user settings.
- Light and dark screenshots rendered from the actual production App::render widgets through wgpu/llvmpipe at 1200×800. They are not mockups. A native OS window is not part of these captures.

## Remaining manual checks

Native file dialogs (including pressing Cancel and overwrite Yes/No), system clipboard cut/copy/paste, OS close events, IME, accessibility, high-DPI displays, and Windows runtime behavior have not been exercised end to end in this environment. The save dialog cancellation path returns before saving; the failed-save and pending-close states have automated coverage. Atomic replacement uses tempfile, but concurrent writers are detected best-effort, not locked; crash/power-loss durability is not guaranteed. Large-file responsiveness and memory benchmarks have not been measured. Source coloring is lexical, not a parser or language server.

## Reproducing verification

Generated evidence under artifacts/ is intentionally not committed. Run:

    cargo fmt --all -- --check
    cargo test --locked --all-targets
    cargo test --locked --all-targets --all-features
    cargo clippy --locked --all-targets -- -D warnings
    cargo clippy --locked --all-targets --all-features -- -D warnings
    bash tests/capture.sh

The capture helper uses the current Cargo toolchain and installed graphics drivers. On Linux, install Mesa Vulkan drivers and set VK_DRIVER_FILES to the installed lavapipe ICD to force software rendering; CI demonstrates discovery of that path. Captures produce artifacts/light.png, artifacts/dark.png and artifacts/settings.png. A working Vulkan adapter is required; these are not native window tests.

The historical local results below are not a claim that the new hosted CI workflow has passed. Rebuilt executable sizes and timings can differ by toolchain, platform, driver and build configuration.

## Build size and bounded capture measurements

The configured native Glow release profile (thin LTO, one codegen unit, stripped) compiled successfully. Its Linux executable is 9,753,808 bytes (9.30 MiB). This excludes system libraries and graphics drivers and is not an installed-package size. Native startup latency and native memory use were not measured; no Windows build or runtime verification was performed.

The separate all-feature debug capture executable is 71,499,384 bytes (68.19 MiB). The final six-frame real-widget capture plus PNG encoding completed in 0.477 seconds with process peak RSS of 598,908 KiB (584.9 MiB), using software llvmpipe rendering. An earlier run varied substantially, so this single observation must not be used as a stable memory benchmark. These numbers include software graphics initialization and capture allocations; they are **not** native startup or steady-state editor memory figures, nor evidence of Glow memory savings. They do not establish a lightweight performance claim.

Reproduce the bounded capture with tests/measure_capture.py after building with --features headless-capture; raw result is artifacts/capture-measurement.json. Simulated frame spacing is 100 ms solely to settle UI fade animations; it does not add wall-clock sleeps. The Linux-only tests/build_release_guard.py helper builds the unchanged release profile and stops if disk headroom falls below 250 MiB.
