//! Byte-addressed, bounded viewport helpers. File bytes are never normalized.
use simptxt::document::Document;
use std::ops::Range;

pub const MAX_ROW_BYTES: usize = 4096;
pub const CLIPBOARD_LIMIT: usize = 16 * 1024 * 1024;

pub fn content_start(doc: &Document) -> usize {
    if doc.len() >= 3 && doc.read(0..3) == [0xef, 0xbb, 0xbf] {
        3
    } else {
        0
    }
}

pub fn line_range(doc: &Document, line: usize) -> Range<usize> {
    let start = doc.line_start(line).max(content_start(doc));
    let mut end = if line + 1 < doc.line_count() {
        doc.line_start(line + 1)
    } else {
        doc.len()
    };
    if end > start && doc.read(end - 1..end) == b"\n" {
        end -= 1;
        if end > start && doc.read(end - 1..end) == b"\r" {
            end -= 1;
        }
    }
    start.min(end)..end
}

pub fn row_range(
    doc: &Document,
    line: usize,
    horizontal_byte: usize,
    budget: usize,
) -> Range<usize> {
    let line = line_range(doc, line);
    let mut start = line.start.saturating_add(horizontal_byte).min(line.end);
    while start > line.start && !doc.char_boundary(start) {
        start -= 1;
    }
    let mut end = start
        .saturating_add(budget.min(MAX_ROW_BYTES))
        .min(line.end);
    while end > start && !doc.char_boundary(end) {
        end -= 1;
    }
    start..end
}

pub fn previous(doc: &Document, offset: usize) -> usize {
    let min = content_start(doc);
    if offset <= min {
        return min;
    }
    if offset >= 2 && doc.read(offset - 2..offset) == b"\r\n" {
        offset - 2
    } else {
        doc.previous_char(offset).max(min)
    }
}
pub fn next(doc: &Document, offset: usize) -> usize {
    if offset + 2 <= doc.len() && doc.read(offset..offset + 2) == b"\r\n" {
        offset + 2
    } else {
        doc.next_char(offset)
    }
}

pub fn byte_at_char(text: &str, character: usize) -> usize {
    text.char_indices()
        .nth(character)
        .map_or(text.len(), |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn viewport_respects_unicode_and_long_lines() {
        let text = "α💚".repeat(100_000);
        let doc = Document::from_bytes(text.into_bytes()).unwrap();
        for h in [0, 1, 2, 3, 50_001, 599_999] {
            let r = row_range(&doc, 0, h, 256);
            assert!(r.len() <= 256);
            assert!(std::str::from_utf8(&doc.read(r)).is_ok());
        }
    }
    #[test]
    fn bom_and_crlf_are_not_cursor_targets() {
        let doc = Document::from_bytes("\u{feff}a\r\nb\n".as_bytes().to_vec()).unwrap();
        assert_eq!(content_start(&doc), 3);
        assert_eq!(line_range(&doc, 0), 3..4);
        assert_eq!(next(&doc, 4), 6);
        assert_eq!(previous(&doc, 6), 4);
    }
}
