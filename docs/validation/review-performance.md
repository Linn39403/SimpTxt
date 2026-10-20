# Review-fix performance validation

These are release-mode core measurements, not promises about every machine or end-to-end GPU frame time. No timing threshold is used as a test gate.

## Reproduce the maintained probe

Run from the repository root:

```sh
cargo run --release --no-default-features --example performance_probe -- 100000000 > docs/validation/review-performance-100mb.json
cargo run --release --no-default-features --example performance_probe -- 1000000000 > docs/validation/review-performance-1gb.json
cargo run --release --no-default-features --example performance_probe -- --history > docs/validation/review-memory-after.json
```

Add `--offline` before `--` if all dependencies are cached. Run each command in a separate process, serially and without another build or benchmark competing for resources. The history measurement must not follow the large-file workload in the same process: allocator reuse would hide resident-memory growth.

The deterministic source fixture repeats `fn main() { let value = 42; } // needle\n` and is constructed in shared 256 KiB chunks. File sizes are decimal 100 MB and 1 GB. The history workload uses binary 8 MiB payloads and 40 actual alternating replacements, verifying that each changes the document.

## What the probe measures

- Forward/reverse literal Find near the end of the file, with actual inspected-byte counts.
- Full lexical indexing, then 100 middle-of-file one-byte insertion/deletion refreshes. Incremental mean, p95, maximum, and maximum scanned bytes are reported separately.
- Forty visible row windows: separate per-row lexer calls, batched calls, and 10,000 warm-cache repaints. These timings exclude text shaping, paint, and GPU submission.
- One hundred insertion/undo pairs, including document bookkeeping.
- In a separate process, retained undo storage and shared tracked document storage, Linux RSS/high-water marks when available, and the number of undo operations still retained after 40 whole-document replacements.

The baseline is commit `19afe1ca1ff5fd550db1fb98ac530d065998182b`; source hashes and measured 100 MB values are in `review-performance-before.json`. Its single-contiguous-viewport timing was a diagnostic alternative, not the old editor's per-row rendering path. The earlier mixed-process memory sample is deliberately excluded because allocator reuse made it unsuitable evidence.

## Recorded results

The final release measurements and hashes are retained in [100 MB results](review-performance-100mb.json), [1 GB results](review-performance-1gb.json), and [source/result provenance](review-performance-provenance.json).

| Core workload | Before | After |
| --- | ---: | ---: |
| 100 MB reverse Find near EOF | 149.264 ms; 100,000,000 bytes inspected | 0.000330 ms; 7 bytes inspected |
| 100 MB syntax refresh after a local edit | Full rebuild: 1,882.734 ms | Incremental mean 0.290 ms; p95 0.501 ms; maximum 0.868 ms |
| 100 MB 40-row lexical work | Per-row mean 0.691 ms | Batched mean 0.036 ms; warm cached mean 0.000047 ms |
| 100 MB insert + undo pair | Mean 0.030 ms | Mean 0.240 ms |
| Fresh-process history after 40 × 8 MiB replacements | RSS 353,816 KiB; 40 undo actions | RSS 157,452 KiB; 15 undo actions |

The additional insertion/undo cost is a measured tradeoff from storage accounting and atomic undo-budget admission, not a speedup. Sub-microsecond single-call search readings are noisy; inspected bytes provide the algorithmic evidence.

On the actual **1,000,000,000-byte** fixture, full indexing took 21,061 ms. The 100 local incremental refreshes averaged **2.128 ms**, with p95 **5.866 ms**, maximum **23.828 ms**, and at most **4,097 bytes lexed** per refresh. Initial indexing is not claimed to be constant time. This measures the core worker operation, not keystroke-to-screen latency.

The final 100 MB full-build measurement was 2,076 ms, compared with the baseline 1,883 ms. The improvement comes from avoiding full rebuilds after ordinary local edits, not a claim that the lexer itself became faster. The current-tree per-row diagnostic was 0.742 ms; the batch/cache measurements above use the same run.

[Before memory results](review-memory-before.json) and [after memory results](review-memory-after.json) come from separate fresh processes. After 40 edits, retained history was 126,171,944 bytes, below the 128 MiB limit; 25 oldest undo actions had been trimmed. The current 8 MiB document remained intact. RSS includes the two reusable replacement strings and allocator overhead; it is distinct from the tracked-storage counter.

## Correctness and scope

Incremental checkpoints are reused only with an unbroken bounded document-change log, complete matching lexer state, and a proven unchanged suffix. Changes to multiline lexical structure may genuinely invalidate the remaining suffix. First loads, expired change history, and such changes can still need a large scan, performed in a cancellable worker. Initial indexing extends in 8 MiB steps through one per-job latest-value slot, so edits retain completed partial progress rather than restarting a 1 GB scan at byte zero. Progress snapshots do not accumulate in a message queue. Unsupported C/C++ preprocessing retains the conservative plain-text fallback.

Viewport lexical work is bounded across the whole batch, including checkpoint prefixes. Very long physical-line gaps are skipped through checkpoints rather than read. Oversized, stale, or unavailable state remains readable plain text. Cached results are keyed to the immutable index, exact document revision, and row windows.

The memory limit covers tracked document storage and in-flight document allocation reservations, not total process RSS. GUI resources, syntax checkpoints, search buffers, input payloads supplied by callers, and allocator-specific arena overhead are outside that accounting. RSS samples provide a separate observed process-level measurement.
