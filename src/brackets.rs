//! Caret-adjacent bracket matching with a fixed foreground work budget.
//!
//! This module never changes the document and deliberately does not auto-close
//! brackets. In source languages a current lexical index is required: guessing
//! a string/comment state would be worse than temporarily showing no pair.
//! Matching is byte-based because the six delimiters are ASCII. No line is
//! flattened, no UTF-8 character is split, and BOM/CRLF bytes are left alone.

use crate::{
    document::Document,
    syntax::{HighlightIndex, HighlightSpan, Language, TokenKind},
};

/// Includes the adjacent bracket itself. The matcher scans at most 24 KiB;
/// the lexical scan adds at most that plus a 4095-byte checkpoint replay.
/// Even together, they visit fewer than 64 KiB. The lexer may additionally
/// read 512 lookahead bytes, without scanning them. These limits do not depend
/// on file size, line length, nesting depth, or the distance to EOF.
pub const MAX_MATCH_BYTES: usize = 24 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PairResult {
    /// Byte offsets of the one-byte opening and closing delimiters.
    Match { open: usize, close: usize },
    /// No adjacent code bracket, a mismatched delimiter, or no mate before EOF.
    #[default]
    NoPair,
    /// A mate might exist outside the bounded search window.
    UnknownBudget,
    /// Reliable lexical state is unavailable for part of the necessary range.
    UnknownSyntax,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HighlightVersion {
    current: bool,
    language: Language,
    indexed_until: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CacheKey {
    revision: u64,
    cursor: usize,
    language: Language,
    highlight: Option<HighlightVersion>,
}

/// One cached result per editor/tab. Repainting an unchanged caret does no
/// lexical or document scan. Document revisions are globally unique, so a
/// cache is also safe across document replacement and undo/redo.
///
/// A highlight index's semantic version is its validated document revision,
/// language, and indexed extent. Rebuilding the same extent for the same
/// revision cannot change the deterministic lexical result; replacing a
/// prefix with a more complete index does invalidate the cache.
#[derive(Debug, Default)]
pub struct BracketCache {
    key: Option<CacheKey>,
    result: PairResult,
    #[cfg(test)]
    scans: usize,
}

impl BracketCache {
    pub fn resolve(
        &mut self,
        doc: &Document,
        cursor: usize,
        language: Language,
        highlight: Option<&HighlightIndex>,
    ) -> PairResult {
        let key = CacheKey {
            revision: doc.revision(),
            cursor,
            language,
            highlight: highlight.map(|index| HighlightVersion {
                current: index.is_current(doc),
                language: index.language(),
                indexed_until: index.indexed_until(),
            }),
        };
        if self.key != Some(key) {
            self.result = matching_pair(doc, cursor, language, highlight);
            self.key = Some(key);
            #[cfg(test)]
            {
                self.scans += 1;
            }
        }
        self.result
    }
}

/// Match the bracket at `cursor`, otherwise the bracket immediately before it.
/// At a boundary between two brackets the one on the right takes precedence.
/// Plain text treats all brackets literally. Rust, C/C++, and JSON exclude
/// strings and comments only when a valid, matching-language index proves the
/// lexical state; a missing, stale, or incomplete index never triggers a guess.
pub fn matching_pair(
    doc: &Document,
    cursor: usize,
    language: Language,
    highlight: Option<&HighlightIndex>,
) -> PairResult {
    if !doc.char_boundary(cursor) {
        return PairResult::NoPair;
    }
    let adjacent_start = cursor.saturating_sub(1);
    let adjacent = doc.read(adjacent_start..cursor.saturating_add(1).min(doc.len()));
    let candidate = adjacent
        .get(cursor - adjacent_start)
        .copied()
        .filter(|&byte| is_bracket(byte))
        .map(|byte| (cursor, byte))
        .or_else(|| {
            (cursor > 0)
                .then(|| adjacent.first().copied())
                .flatten()
                .filter(|&byte| is_bracket(byte))
                .map(|byte| (cursor - 1, byte))
        });
    let Some((at, bracket)) = candidate else {
        return PairResult::NoPair;
    };
    let forward = is_open(bracket);
    let mut full_range = if forward {
        at..at.saturating_add(MAX_MATCH_BYTES).min(doc.len())
    } else {
        at.saturating_sub(MAX_MATCH_BYTES - 1)..at + 1
    };
    // The lexer supports raw byte ranges, but keeping both ends on character
    // boundaries also makes this safe for future lexical APIs. At most three
    // continuation bytes are trimmed; no ASCII bracket can be lost here.
    while !doc.char_boundary(full_range.start) {
        full_range.start += 1;
    }
    while !doc.char_boundary(full_range.end) {
        full_range.end -= 1;
    }
    let mut range = full_range.clone();
    let spans = if language == Language::Plain {
        Vec::new()
    } else {
        let Some(index) = highlight.filter(|index| {
            index.is_current(doc) && index.language() == language && at < index.indexed_until()
        }) else {
            return PairResult::UnknownSyntax;
        };
        // A usable prefix can still prove a nearby pair. Do not discard it
        // merely because the full potential search window is not indexed yet.
        range.end = range.end.min(index.indexed_until());
        while !doc.char_boundary(range.end) {
            range.end -= 1;
        }
        match index.spans(doc, range.clone()) {
            Ok(spans) => spans,
            Err(_) => return PairResult::UnknownSyntax,
        }
    };
    let bytes = doc.read(range.clone());
    let mut stack = Vec::with_capacity(32);
    let mut span_index = if forward { 0 } else { spans.len() };
    for step in 0..bytes.len() {
        let local = if forward {
            step
        } else {
            bytes.len() - 1 - step
        };
        if ignored(local, &spans, &mut span_index, forward) {
            if step == 0 {
                return PairResult::NoPair;
            }
            continue;
        }
        let byte = bytes[local];
        if !is_bracket(byte) {
            continue;
        }
        if is_open(byte) == forward {
            stack.push(byte);
        } else {
            let Some(previous) = stack.pop() else {
                return PairResult::NoPair;
            };
            if mate(previous) != byte {
                return PairResult::NoPair;
            }
            if stack.is_empty() {
                let other = range.start + local;
                return if forward {
                    PairResult::Match {
                        open: at,
                        close: other,
                    }
                } else {
                    PairResult::Match {
                        open: other,
                        close: at,
                    }
                };
            }
        }
    }
    if range.end < full_range.end {
        PairResult::UnknownSyntax
    } else if (forward && range.end < doc.len()) || (!forward && range.start > 0) {
        PairResult::UnknownBudget
    } else {
        PairResult::NoPair
    }
}

fn is_bracket(byte: u8) -> bool {
    matches!(byte, b'(' | b')' | b'[' | b']' | b'{' | b'}')
}

fn is_open(byte: u8) -> bool {
    matches!(byte, b'(' | b'[' | b'{')
}

fn mate(byte: u8) -> u8 {
    match byte {
        b'(' => b')',
        b')' => b'(',
        b'[' => b']',
        b']' => b'[',
        b'{' => b'}',
        b'}' => b'{',
        _ => 0,
    }
}

// Both directional cursors traverse the sorted lexer output only once.
fn ignored(local: usize, spans: &[HighlightSpan], index: &mut usize, forward: bool) -> bool {
    let span = if forward {
        while *index < spans.len() && spans[*index].0.end <= local {
            *index += 1;
        }
        spans.get(*index)
    } else {
        while *index > 0 && spans[*index - 1].0.start > local {
            *index -= 1;
        }
        index.checked_sub(1).and_then(|index| spans.get(index))
    };
    span.is_some_and(|(range, kind)| {
        range.contains(&local) && matches!(kind, TokenKind::Comment | TokenKind::String)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn document(text: &str) -> Document {
        Document::from_bytes(text.as_bytes().to_vec()).unwrap()
    }

    fn index(doc: &Document, language: Language) -> HighlightIndex {
        HighlightIndex::build(doc, language, &AtomicBool::new(false)).unwrap()
    }

    fn pair(open: usize, close: usize) -> PairResult {
        PairResult::Match { open, close }
    }

    #[test]
    fn all_delimiters_match_in_both_directions_with_mixed_nesting() {
        let doc = document("({[abc]})");
        for (open, close) in [(0, 8), (1, 7), (2, 6)] {
            assert_eq!(
                matching_pair(&doc, open, Language::Plain, None),
                pair(open, close)
            );
            assert_eq!(
                matching_pair(&doc, close, Language::Plain, None),
                pair(open, close)
            );
        }
        assert_eq!(
            matching_pair(&doc, doc.len(), Language::Plain, None),
            pair(0, 8)
        );
    }

    #[test]
    fn caret_prefers_right_bracket_and_can_match_the_immediate_left() {
        let doc = document("{}[] (word) tail");
        assert_eq!(matching_pair(&doc, 2, Language::Plain, None), pair(2, 3));
        assert_eq!(matching_pair(&doc, 6, Language::Plain, None), pair(5, 10));
        assert_eq!(matching_pair(&doc, 11, Language::Plain, None), pair(5, 10));
        assert_eq!(
            matching_pair(&doc, 12, Language::Plain, None),
            PairResult::NoPair
        );
    }

    #[test]
    fn mismatched_nesting_and_unmatched_brackets_do_not_form_pairs() {
        for text in ["([)]", "{]", "((", "))", "[}"] {
            let doc = document(text);
            for cursor in [0, doc.len()] {
                assert_eq!(
                    matching_pair(&doc, cursor, Language::Plain, None),
                    PairResult::NoPair
                );
            }
        }
    }

    #[test]
    fn utf8_bom_crlf_and_invalid_carets_are_safe_and_unchanged() {
        let text = "\u{feff}é(\r\n[中文🙂]\r\n)é";
        let doc = document(text);
        let open = text.find('(').unwrap();
        let close = text.find(')').unwrap();
        assert_eq!(
            matching_pair(&doc, open, Language::Plain, None),
            pair(open, close)
        );
        assert_eq!(
            matching_pair(&doc, close + 1, Language::Plain, None),
            pair(open, close)
        );
        assert_eq!(
            matching_pair(&doc, 1, Language::Plain, None),
            PairResult::NoPair
        );
        assert_eq!(
            matching_pair(&doc, doc.len() + 1, Language::Plain, None),
            PairResult::NoPair
        );
        assert_eq!(
            matching_pair(&doc, usize::MAX, Language::Plain, None),
            PairResult::NoPair
        );
        assert_eq!(doc.read(0..doc.len()), text.as_bytes());
    }

    #[test]
    fn strings_comments_characters_and_raw_strings_are_excluded() {
        let cases = [
            (
                Language::Rust,
                "(/* ) /* ] */ } */ [r##\"})\"##, ']', \"(\"])",
            ),
            (Language::C, "(/* ) ] } */ [R\"tag(})tag\", ']', \"(\"])"),
            (Language::Json, "{\"key\": [\"})]\", \"(\\\"[\"]}"),
        ];
        for (language, text) in cases {
            let doc = document(text);
            let highlight = index(&doc, language);
            assert_eq!(
                matching_pair(&doc, 0, language, Some(&highlight)),
                pair(0, doc.len() - 1)
            );
            assert_eq!(
                matching_pair(&doc, doc.len(), language, Some(&highlight)),
                pair(0, doc.len() - 1)
            );
            let inside = text.find("})").unwrap();
            assert_eq!(
                matching_pair(&doc, inside, language, Some(&highlight)),
                PairResult::NoPair
            );
        }
    }

    #[test]
    fn line_comments_and_multiline_lexical_state_are_respected() {
        let text = format!("/*{}( ]*/\n(\n// )\r\n[1])", "x".repeat(9000));
        let doc = document(&text);
        let highlight = index(&doc, Language::Rust);
        let comment_bracket = text.find('(').unwrap();
        let open = text.rfind("\n(").unwrap() + 1;
        assert_eq!(
            matching_pair(&doc, comment_bracket, Language::Rust, Some(&highlight)),
            PairResult::NoPair
        );
        assert_eq!(
            matching_pair(&doc, open, Language::Rust, Some(&highlight)),
            pair(open, doc.len() - 1)
        );
        assert_eq!(
            matching_pair(&doc, doc.len(), Language::Rust, Some(&highlight)),
            pair(open, doc.len() - 1)
        );
    }

    #[test]
    fn plain_text_does_not_infer_string_or_comment_syntax() {
        let doc = document("\"()\" // []");
        assert_eq!(matching_pair(&doc, 1, Language::Plain, None), pair(1, 2));
        assert_eq!(matching_pair(&doc, 8, Language::Plain, None), pair(8, 9));
    }

    #[test]
    fn source_requires_current_matching_language_index() {
        let mut doc = document("()");
        let rust = index(&doc, Language::Rust);
        let json = index(&doc, Language::Json);
        assert_eq!(
            matching_pair(&doc, 0, Language::Rust, None),
            PairResult::UnknownSyntax
        );
        assert_eq!(
            matching_pair(&doc, 0, Language::Rust, Some(&json)),
            PairResult::UnknownSyntax
        );
        doc.replace(1..1, "[]").unwrap();
        assert_eq!(
            matching_pair(&doc, 0, Language::Rust, Some(&rust)),
            PairResult::UnknownSyntax
        );
        let empty = document("");
        assert_eq!(
            matching_pair(&empty, 0, Language::Rust, None),
            PairResult::NoPair
        );
    }

    #[test]
    fn exact_budget_boundary_is_inclusive_but_long_lines_are_bounded() {
        let doc = document(&format!("({})", " ".repeat(MAX_MATCH_BYTES - 2)));
        assert_eq!(
            matching_pair(&doc, 0, Language::Plain, None),
            pair(0, doc.len() - 1)
        );
        assert_eq!(
            matching_pair(&doc, doc.len(), Language::Plain, None),
            pair(0, doc.len() - 1)
        );
        let long = document(&format!("({})", " ".repeat(MAX_MATCH_BYTES * 20)));
        assert_eq!(
            matching_pair(&long, 0, Language::Plain, None),
            PairResult::UnknownBudget
        );
        assert_eq!(
            matching_pair(&long, long.len(), Language::Plain, None),
            PairResult::UnknownBudget
        );
        let nested = document(&"(".repeat(MAX_MATCH_BYTES + 1));
        assert_eq!(
            matching_pair(&nested, 0, Language::Plain, None),
            PairResult::UnknownBudget
        );
    }

    #[test]
    fn large_unicode_windows_and_partial_index_ends_keep_nearby_pairs() {
        // Both directional window boundaries land inside an é, as does the
        // prefix index's end. The valid nearby ASCII pairs still highlight.
        let text = format!("(x){}(x)", "é".repeat(MAX_MATCH_BYTES));
        let doc = document(&text);
        let full = index(&doc, Language::Rust);
        assert_eq!(
            matching_pair(&doc, 0, Language::Rust, Some(&full)),
            pair(0, 2)
        );
        assert_eq!(
            matching_pair(&doc, doc.len(), Language::Rust, Some(&full)),
            pair(doc.len() - 3, doc.len() - 1)
        );
        let prefix = HighlightIndex::build_prefix(
            &doc,
            Language::Rust,
            MAX_MATCH_BYTES,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            matching_pair(&doc, 0, Language::Rust, Some(&prefix)),
            pair(0, 2)
        );

        let far = document(&format!("({})", "🙂".repeat(MAX_MATCH_BYTES)));
        let full = index(&far, Language::Rust);
        for cursor in [0, far.len()] {
            assert_eq!(
                matching_pair(&far, cursor, Language::Rust, Some(&full)),
                PairResult::UnknownBudget
            );
        }
    }

    #[test]
    fn a_prefix_can_prove_a_pair_but_never_guess_beyond_its_extent() {
        let doc = document("([])     (  )");
        let prefix =
            HighlightIndex::build_prefix(&doc, Language::Rust, 11, &AtomicBool::new(false))
                .unwrap();
        assert_eq!(
            matching_pair(&doc, 0, Language::Rust, Some(&prefix)),
            pair(0, 3)
        );
        assert_eq!(
            matching_pair(&doc, 9, Language::Rust, Some(&prefix)),
            PairResult::UnknownSyntax
        );
        assert_eq!(
            matching_pair(&doc, doc.len(), Language::Rust, Some(&prefix)),
            PairResult::UnknownSyntax
        );
    }

    #[test]
    fn unchanged_frames_hit_cache_and_edits_cursor_and_language_invalidate() {
        let mut doc = document("([])");
        let mut cache = BracketCache::default();
        for _ in 0..1000 {
            assert_eq!(cache.resolve(&doc, 0, Language::Plain, None), pair(0, 3));
        }
        assert_eq!(cache.scans, 1);
        assert_eq!(cache.resolve(&doc, 1, Language::Plain, None), pair(1, 2));
        assert_eq!(cache.scans, 2);
        assert_eq!(
            cache.resolve(&doc, 1, Language::Rust, None),
            PairResult::UnknownSyntax
        );
        assert_eq!(cache.scans, 3);
        doc.replace(2..3, "}").unwrap();
        assert_eq!(
            cache.resolve(&doc, 1, Language::Plain, None),
            PairResult::NoPair
        );
        assert_eq!(cache.scans, 4);
        let replacement = document("(  )");
        assert_eq!(
            cache.resolve(&replacement, 1, Language::Plain, None),
            pair(0, 3)
        );
        assert_eq!(cache.scans, 5);
    }

    #[test]
    fn ready_more_complete_and_stale_highlighting_invalidate_cached_results() {
        let mut doc = document("(    )");
        let mut cache = BracketCache::default();
        assert_eq!(
            cache.resolve(&doc, 0, Language::Rust, None),
            PairResult::UnknownSyntax
        );
        let prefix =
            HighlightIndex::build_prefix(&doc, Language::Rust, 3, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            cache.resolve(&doc, 0, Language::Rust, Some(&prefix)),
            PairResult::UnknownSyntax
        );
        let full = index(&doc, Language::Rust);
        assert_eq!(
            cache.resolve(&doc, 0, Language::Rust, Some(&full)),
            pair(0, 5)
        );
        assert_eq!(cache.scans, 3);
        doc.replace(2..2, " ").unwrap();
        assert_eq!(
            cache.resolve(&doc, 0, Language::Rust, Some(&full)),
            PairResult::UnknownSyntax
        );
        let fresh = index(&doc, Language::Rust);
        assert_eq!(
            cache.resolve(&doc, 0, Language::Rust, Some(&fresh)),
            pair(0, 6)
        );
        assert_eq!(cache.scans, 5);
    }
}
