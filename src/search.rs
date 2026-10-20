//! Streaming, literal search over document pieces.
//!
//! Offsets are UTF-8 byte offsets. Search never flattens a document, and replace
//! all prepares an isolated snapshot so a cancellation cannot partially edit
//! the live document. The caller must check the original revision before
//! committing that snapshot with `Document::apply_snapshot`.

use crate::document::Document;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Resource limits are reported as errors, never as an incomplete result.
pub const MAX_MATCHES: usize = 1_000_000;
pub const MAX_NEEDLE_BYTES: usize = 1024 * 1024;
const CANCEL_STRIDE: usize = 4096;

struct Pattern<'a> {
    bytes: &'a [u8],
    failure: Vec<usize>,
    reverse: bool,
}

impl<'a> Pattern<'a> {
    fn new(needle: &'a str, cancel: &AtomicBool, reverse: bool) -> Result<Self, String> {
        if needle.is_empty() {
            return Err("Enter text to find.".into());
        }
        if needle.len() > MAX_NEEDLE_BYTES {
            return Err("Find text exceeds the 1 MiB search limit.".into());
        }
        let bytes = needle.as_bytes();
        let mut failure = Vec::new();
        failure
            .try_reserve_exact(bytes.len())
            .map_err(|_| "Not enough memory to prepare this search.")?;
        failure.resize(bytes.len(), 0);
        let at = |index: usize| {
            bytes[if reverse {
                bytes.len() - index - 1
            } else {
                index
            }]
        };
        let mut matched = 0;
        for i in 1..bytes.len() {
            if i % CANCEL_STRIDE == 0 {
                check_cancel(cancel)?;
            }
            while matched != 0 && at(i) != at(matched) {
                matched = failure[matched - 1];
            }
            if at(i) == at(matched) {
                matched += 1;
            }
            failure[i] = matched;
        }
        check_cancel(cancel)?;
        Ok(Self {
            bytes,
            failure,
            reverse,
        })
    }

    fn byte(&self, index: usize) -> u8 {
        self.bytes[if self.reverse {
            self.bytes.len() - index - 1
        } else {
            index
        }]
    }

    fn advance(&self, matched: &mut usize, byte: u8) -> bool {
        while *matched != 0 && byte != self.byte(*matched) {
            *matched = self.failure[*matched - 1];
        }
        if byte == self.byte(*matched) {
            *matched += 1;
        }
        *matched == self.bytes.len()
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("Search cancelled; the document was not changed.".into())
    } else {
        Ok(())
    }
}

/// Find the first match in `[start, len)` or, for backwards search, the last
/// complete match in `[0, start)`. There is no implicit wraparound.
///
/// `progress` counts bytes inspected during this call. Backwards search seeks
/// directly to the cursor and scans reversed pieces, stopping at the first
/// complete match. Both directions keep only O(needle length) KMP state.
pub fn search(
    doc: &Document,
    needle: &str,
    start: usize,
    backwards: bool,
    cancel: &AtomicBool,
    progress: &AtomicUsize,
) -> Result<Option<Range<usize>>, String> {
    progress.store(0, Ordering::Relaxed);
    let pattern = Pattern::new(needle, cancel, backwards)?;
    let start = start.min(doc.len());
    if backwards {
        return scan_reverse(doc, &pattern, 0..start, cancel, progress);
    }
    let range = start..doc.len();
    let mut last = None;
    scan(doc, &pattern, range, cancel, progress, true, |found| {
        last = Some(found);
        false
    })?;
    Ok(last)
}

/// Find next/previous with one wrap. The two scans overlap by at most
/// needle.len()-1 bytes so matches spanning the cursor are not lost. A missing
/// needle inspects at most document.len()+needle.len()-1 bytes, not two files.
pub fn search_wrapping(
    doc: &Document,
    needle: &str,
    start: usize,
    backwards: bool,
    cancel: &AtomicBool,
    progress: &AtomicUsize,
) -> Result<Option<Range<usize>>, String> {
    progress.store(0, Ordering::Relaxed);
    let pattern = Pattern::new(needle, cancel, backwards)?;
    let start = start.min(doc.len());
    let mut found = None;
    if backwards {
        found = scan_reverse(doc, &pattern, 0..start, cancel, progress)?;
    } else {
        scan(
            doc,
            &pattern,
            start..doc.len(),
            cancel,
            progress,
            true,
            |range| {
                found = Some(range);
                false
            },
        )?;
    }
    if found.is_some() {
        return Ok(found);
    }
    // Both halves retain bounded cancellation checks. The final counter reports
    // total bytes inspected across the wrap.
    if backwards {
        let from = start.saturating_sub(pattern.bytes.len().saturating_sub(1));
        found = scan_reverse(doc, &pattern, from..doc.len(), cancel, progress)?;
    } else {
        let to = start
            .saturating_add(pattern.bytes.len().saturating_sub(1))
            .min(doc.len());
        scan(doc, &pattern, 0..to, cancel, progress, true, |range| {
            found = Some(range);
            false
        })?;
    }
    Ok(found)
}

/// Find every non-overlapping match, in ascending order, ready for an atomic
/// `Document::replace_ranges` operation. More than `MAX_MATCHES` is an explicit
/// error; callers must not apply a partial result.
pub fn find_all(
    doc: &Document,
    needle: &str,
    cancel: &AtomicBool,
    progress: &AtomicUsize,
) -> Result<Vec<Range<usize>>, String> {
    progress.store(0, Ordering::Relaxed);
    let pattern = Pattern::new(needle, cancel, false)?;
    let mut matches = Vec::new();
    let mut failure = None;
    scan(
        doc,
        &pattern,
        0..doc.len(),
        cancel,
        progress,
        false,
        |found| {
            if matches.len() == MAX_MATCHES {
                failure = Some(format!(
                    "Replace all exceeds the {MAX_MATCHES} match limit; the document was not changed."
                ));
                return false;
            }
            if matches.len() == matches.capacity() && matches.try_reserve(4096).is_err() {
                failure = Some(
                    "Not enough memory to collect matches; the document was not changed.".into(),
                );
                return false;
            }
            matches.push(found);
            true
        },
    )?;
    match failure {
        Some(message) => Err(message),
        None => Ok(matches),
    }
}

/// Prepare replace-all entirely off the UI thread. The live document is never
/// modified. Commit only if its revision still equals the revision used here.
///
/// A returned count of zero needs no commit. Cancellation before publication
/// discards the prepared snapshot rather than exposing a partial replacement.
pub fn replace_all(
    doc: &Document,
    needle: &str,
    replacement: &str,
    cancel: &AtomicBool,
    progress: &AtomicUsize,
) -> Result<(Document, usize), String> {
    let ranges = find_all(doc, needle, cancel, progress)?;
    check_cancel(cancel)?;
    let mut edited = doc.snapshot();
    if !ranges.is_empty() {
        edited.replace_ranges_cancellable(&ranges, replacement, cancel)?;
    }
    check_cancel(cancel)?;
    Ok((edited, ranges.len()))
}

/// Reverse KMP preserves its state across piece boundaries without flattening
/// either the document or its prefix. Reversing UTF-8 bytes is intentional: the
/// needle and document are both traversed in the same direction, and a valid
/// UTF-8 needle cannot match starting inside another codepoint.
fn scan_reverse(
    doc: &Document,
    pattern: &Pattern<'_>,
    range: Range<usize>,
    cancel: &AtomicBool,
    progress: &AtomicUsize,
) -> Result<Option<Range<usize>>, String> {
    check_cancel(cancel)?;
    let mut matched = 0;
    let mut inspected = 0;
    let prior_inspected = progress.load(Ordering::Relaxed);
    for (piece_start, piece) in doc.chunks_reverse(range) {
        for (index, &byte) in piece.iter().enumerate().rev() {
            if inspected % CANCEL_STRIDE == 0 {
                progress.store(prior_inspected + inspected, Ordering::Relaxed);
                check_cancel(cancel)?;
            }
            inspected += 1;
            if pattern.advance(&mut matched, byte) {
                let start = piece_start + index;
                progress.store(prior_inspected + inspected, Ordering::Relaxed);
                check_cancel(cancel)?;
                return Ok(Some(start..start + pattern.bytes.len()));
            }
        }
    }
    progress.store(prior_inspected + inspected, Ordering::Relaxed);
    check_cancel(cancel)?;
    Ok(None)
}

/// Returns immediately when the visitor returns false. KMP state persists
/// across document piece boundaries, including boundaries inside a match.
fn scan(
    doc: &Document,
    pattern: &Pattern<'_>,
    range: Range<usize>,
    cancel: &AtomicBool,
    progress: &AtomicUsize,
    overlap: bool,
    mut found: impl FnMut(Range<usize>) -> bool,
) -> Result<(), String> {
    check_cancel(cancel)?;
    let mut piece_start = 0;
    let mut matched = 0;
    let mut inspected = 0;
    let prior_inspected = progress.load(Ordering::Relaxed);
    for piece in doc.chunks() {
        let piece_end = piece_start + piece.len();
        if piece_start >= range.end {
            break;
        }
        if piece_end <= range.start {
            piece_start = piece_end;
            continue;
        }
        let from = range.start.saturating_sub(piece_start);
        let to = piece.len().min(range.end - piece_start);
        for (index, &byte) in piece[from..to].iter().enumerate() {
            if inspected % CANCEL_STRIDE == 0 {
                progress.store(prior_inspected + inspected, Ordering::Relaxed);
                check_cancel(cancel)?;
            }
            inspected += 1;
            if pattern.advance(&mut matched, byte) {
                let end = piece_start + from + index + 1;
                if !found(end - pattern.bytes.len()..end) {
                    progress.store(prior_inspected + inspected, Ordering::Relaxed);
                    check_cancel(cancel)?;
                    return Ok(());
                }
                matched = if overlap {
                    pattern.failure[matched - 1]
                } else {
                    0
                };
            }
        }
        piece_start = piece_end;
    }
    progress.store(prior_inspected + inspected, Ordering::Relaxed);
    check_cancel(cancel)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(text: &str) -> Document {
        Document::from_bytes(text.as_bytes().to_vec()).unwrap()
    }

    fn find(doc: &Document, needle: &str, start: usize, backwards: bool) -> Option<Range<usize>> {
        search(
            doc,
            needle,
            start,
            backwards,
            &AtomicBool::new(false),
            &AtomicUsize::new(0),
        )
        .unwrap()
    }

    #[test]
    fn forward_backward_and_explicit_bounds() {
        let doc = document("one two one");
        assert_eq!(find(&doc, "one", 0, false), Some(0..3));
        assert_eq!(find(&doc, "one", 3, false), Some(8..11));
        assert_eq!(find(&doc, "one", 8, true), Some(0..3));
        assert_eq!(find(&doc, "one", 11, true), Some(8..11));
        assert_eq!(find(&doc, "one", 0, true), None);
        assert_eq!(find(&doc, "one", 11, false), None);
        assert_eq!(find(&doc, "missing", 0, false), None);
    }

    #[test]
    fn finds_across_piece_boundaries() {
        let mut doc = document("abef");
        doc.replace(2..2, "cd").unwrap();
        assert_eq!(find(&doc, "bcde", 0, false), Some(1..5));
        doc.replace(3..3, "🌻").unwrap();
        assert_eq!(find(&doc, "c🌻d", 0, false), Some(2..8));
    }

    #[test]
    fn handles_repeated_prefixes_without_missing_matches() {
        let doc = document("aaaaabaaaab");
        assert_eq!(find(&doc, "aaaab", 0, false), Some(1..6));
        assert_eq!(find(&doc, "aaaab", doc.len(), true), Some(6..11));
        assert_eq!(find(&document("ababa"), "aba", 5, true), Some(2..5));
    }

    #[test]
    fn find_all_is_non_overlapping_and_utf8_preserving() {
        let cancel = AtomicBool::new(false);
        let progress = AtomicUsize::new(0);
        let doc = document("ééé");
        assert_eq!(
            find_all(&doc, "éé", &cancel, &progress).unwrap(),
            vec![0..4]
        );
        assert_eq!(progress.load(Ordering::Relaxed), doc.len());
    }

    #[test]
    fn cancelled_search_and_empty_pattern_are_errors() {
        let doc = document("sample");
        let progress = AtomicUsize::new(0);
        assert!(search(&doc, "a", 0, false, &AtomicBool::new(true), &progress).is_err());
        assert!(find_all(&doc, "", &AtomicBool::new(false), &progress).is_err());
        assert_eq!(doc.read(0..doc.len()), b"sample");
    }

    #[test]
    fn replaces_in_snapshot_without_mutating_original() {
        let doc = document("cat 🐈 cat");
        let (edited, count) = replace_all(
            &doc,
            "cat",
            "kitten",
            &AtomicBool::new(false),
            &AtomicUsize::new(0),
        )
        .unwrap();
        assert_eq!(count, 2);
        assert_eq!(doc.read(0..doc.len()), "cat 🐈 cat".as_bytes());
        assert_eq!(edited.read(0..edited.len()), "kitten 🐈 kitten".as_bytes());
    }

    #[test]
    fn match_limit_is_an_error_not_a_partial_result() {
        let doc = Document::from_bytes(vec![b'x'; MAX_MATCHES + 1]).unwrap();
        let error = find_all(&doc, "x", &AtomicBool::new(false), &AtomicUsize::new(0)).unwrap_err();
        assert!(error.contains("match limit"));
    }

    #[test]
    fn reverse_search_starts_at_cursor_and_stops_at_nearest_match() {
        let mut bytes = vec![b'x'; crate::document::CHUNK_SIZE * 8];
        bytes.extend_from_slice("🌻needle!".as_bytes());
        let doc = Document::from_bytes(bytes).unwrap();
        let progress = AtomicUsize::new(0);
        let found = search(
            &doc,
            "🌻needle",
            doc.len(),
            true,
            &AtomicBool::new(false),
            &progress,
        )
        .unwrap();
        assert_eq!(found, Some(doc.len() - 11..doc.len() - 1));
        assert_eq!(progress.load(Ordering::Relaxed), 11);
    }

    #[test]
    fn reverse_search_preserves_cross_piece_utf8_and_partial_cursor_semantics() {
        let doc = Document::from_chunks(
            ["aa🌻", "bb", "🌻", "cc"]
                .into_iter()
                .map(|text| std::sync::Arc::<[u8]>::from(text.as_bytes()))
                .collect(),
        )
        .unwrap();
        assert_eq!(find(&doc, "🌻bb🌻", doc.len(), true), Some(2..12));
        assert_eq!(find(&doc, "🌻", 10, true), Some(2..6));
        assert_eq!(find(&doc, "🌻", 12, true), Some(8..12));
        assert_eq!(find(&doc, "bb🌻c", 13, true), Some(6..13));
    }

    #[test]
    fn wrapping_finds_both_directions_and_matches_spanning_cursor() {
        let doc = document("one two one");
        let cancel = AtomicBool::new(false);
        let progress = AtomicUsize::new(0);
        assert_eq!(
            search_wrapping(&doc, "one", doc.len(), false, &cancel, &progress).unwrap(),
            Some(0..3)
        );
        assert_eq!(
            search_wrapping(&doc, "one", 0, true, &cancel, &progress).unwrap(),
            Some(8..11)
        );
        let doc = document("a🌻b");
        for backwards in [false, true] {
            assert_eq!(
                search_wrapping(&doc, "a🌻b", 3, backwards, &cancel, &progress).unwrap(),
                Some(0..6)
            );
        }
    }

    #[test]
    fn wrapping_absent_needle_scans_only_one_document_plus_boundary_overlap() {
        let doc = document(&"abc".repeat(10_000));
        let progress = AtomicUsize::new(0);
        for backwards in [false, true] {
            assert_eq!(
                search_wrapping(
                    &doc,
                    "missing",
                    12_345,
                    backwards,
                    &AtomicBool::new(false),
                    &progress
                )
                .unwrap(),
                None
            );
            assert_eq!(
                progress.load(Ordering::Relaxed),
                doc.len() + "missing".len() - 1
            );
        }
        assert!(search_wrapping(&doc, "a", 10, true, &AtomicBool::new(true), &progress).is_err());
    }

    #[test]
    fn reverse_matches_reference_at_every_byte_cursor() {
        let text = "aéaba🙂aabé🙂ababa";
        let doc = Document::from_chunks(
            text.as_bytes()
                .chunks(3)
                .map(std::sync::Arc::<[u8]>::from)
                .collect(),
        )
        .unwrap();
        for needle in ["a", "aa", "aba", "é", "🙂", "éa", "aabé🙂", "missing"] {
            for start in 0..=doc.len() {
                let expected = text.as_bytes()[..start]
                    .windows(needle.len())
                    .rposition(|window| window == needle.as_bytes())
                    .map(|at| at..at + needle.len());
                assert_eq!(
                    find(&doc, needle, start, true),
                    expected,
                    "needle={needle} start={start}"
                );
            }
        }
    }
}
