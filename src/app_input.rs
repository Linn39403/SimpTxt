//! Ordinary editor input. Byte coordinates remain authoritative; only visual
//! vertical motion and IME requests use Unicode-scalar coordinates.
use super::*;
use std::collections::VecDeque;

const TYPING_PAUSE: Duration = Duration::from_secs(1);
const MAX_BLOCK_LINES: usize = 16_384;
const MAX_AUTOINDENT: usize = 4096;
const MAX_IME_SCALARS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CursorState {
    cursor: usize,
    anchor: Option<usize>,
}
#[derive(Clone, Copy)]
struct CursorEdit {
    before_revision: u64,
    after_revision: u64,
    before: CursorState,
    after: CursorState,
}
#[derive(Default)]
pub(super) struct CursorHistory {
    undo: VecDeque<CursorEdit>,
    redo: Vec<CursorEdit>,
}
pub(super) struct TypingGroup {
    revision: u64,
    cursor: usize,
    last_input: Instant,
}

impl EditorApp {
    pub(super) fn cursor_state(&self) -> CursorState {
        CursorState {
            cursor: self.cursor,
            anchor: self.anchor,
        }
    }

    pub(super) fn break_typing_group(&mut self) {
        self.typing_group = None;
    }

    pub(super) fn record_cursor_edit(&mut self, revision: u64, before: CursorState) {
        let after_revision = self.doc.revision();
        if revision == after_revision {
            return;
        }
        let after = self.cursor_state();
        let group_revision = self.doc.undo_group_revision().unwrap_or(revision);
        self.cursor_history.redo.clear();
        if let Some(previous) = self.cursor_history.undo.back_mut()
            && previous.before_revision == group_revision
            && previous.after_revision == revision
        {
            previous.after_revision = after_revision;
            previous.after = after;
            return;
        }
        self.cursor_history.undo.push_back(CursorEdit {
            before_revision: revision,
            after_revision,
            before,
            after,
        });
        while self.cursor_history.undo.len() > simptxt::document::HISTORY_LIMIT {
            self.cursor_history.undo.pop_front();
        }
    }

    pub(super) fn restore_history(&mut self, redo: bool) -> bool {
        self.break_typing_group();
        if !self.ready {
            return false;
        }
        let old_revision = self.doc.revision();
        let changed = if redo {
            self.doc.redo()
        } else {
            self.doc.undo()
        };
        if !changed {
            return false;
        }
        let revision = self.doc.revision();
        let state = if redo {
            self.cursor_history
                .redo
                .iter()
                .rposition(|edit| {
                    edit.before_revision == old_revision && edit.after_revision == revision
                })
                .map(|index| {
                    let edit = self.cursor_history.redo.remove(index);
                    let state = edit.after;
                    self.cursor_history.undo.push_back(edit);
                    state
                })
        } else {
            self.cursor_history
                .undo
                .iter()
                .rposition(|edit| {
                    edit.after_revision == old_revision && edit.before_revision == revision
                })
                .map(|index| {
                    let edit = self
                        .cursor_history
                        .undo
                        .remove(index)
                        .expect("located cursor edit");
                    let state = edit.before;
                    self.cursor_history.redo.push(edit);
                    state
                })
        };
        if let Some(state) = state {
            self.cursor = state.cursor;
            self.anchor = state.anchor;
        } else {
            // Background or legacy edits without cursor metadata remain safe.
            self.anchor = None;
        }
        self.changed();
        true
    }

    pub(super) fn insert(&mut self, text: &str) {
        self.break_typing_group();
        self.insert_text(text, false);
    }

    pub(super) fn insert_typed(&mut self, text: &str) {
        // Paste and IME commits use insert(), so only adjacent ordinary typing
        // can merge. Newlines (including synthetic multi-line Text) are atomic.
        let groupable =
            !text.is_empty() && !text.contains(['\r', '\n']) && self.selection().is_empty();
        let join = groupable
            && self.typing_group.as_ref().is_some_and(|group| {
                group.revision == self.doc.revision()
                    && group.cursor == self.cursor
                    && group.last_input.elapsed() < TYPING_PAUSE
            });
        if !groupable {
            self.break_typing_group();
        }
        let old_revision = self.doc.revision();
        self.insert_text(text, join);
        if groupable && self.doc.revision() != old_revision {
            self.typing_group = Some(TypingGroup {
                revision: self.doc.revision(),
                cursor: self.cursor,
                last_input: Instant::now(),
            });
        } else {
            self.break_typing_group();
        }
    }

    fn insert_text(&mut self, text: &str, join: bool) {
        if !self.ready || (text.is_empty() && self.selection().is_empty()) {
            return;
        }
        if text.contains('\0') {
            self.error = Some(
                "NUL bytes are not supported in editable UTF-8 text. The document was not changed."
                    .into(),
            );
            return;
        }
        if text.len() > view::CLIPBOARD_LIMIT {
            self.error = Some("One insertion exceeds the 16 MiB safety limit. Paste smaller regions; the document was not changed.".into());
            return;
        }
        let before = self.cursor_state();
        let revision = self.doc.revision();
        let range = self.selection();
        let next = range.start.saturating_add(text.len());
        match self.doc.replace_grouped(range, text, join) {
            Ok(()) => {
                self.cursor = next;
                self.anchor = None;
                self.changed();
                self.record_cursor_edit(revision, before);
            }
            Err(e) => self.error = Some(e),
        }
    }

    pub(super) fn delete_selection(&mut self) -> bool {
        let range = self.selection();
        if range.is_empty() || !self.ready {
            return false;
        }
        self.delete_range(range)
    }

    fn delete_range(&mut self, range: Range<usize>) -> bool {
        self.break_typing_group();
        if range.is_empty() || !self.ready {
            return false;
        }
        let before = self.cursor_state();
        let revision = self.doc.revision();
        let position = range.start;
        match self.doc.replace(range, "") {
            Ok(()) => {
                self.cursor = position;
                self.anchor = None;
                self.changed();
                self.record_cursor_edit(revision, before);
                true
            }
            Err(e) => {
                self.error = Some(e);
                false
            }
        }
    }

    pub(super) fn delete_direction(&mut self, backwards: bool, word: bool, line_end: bool) {
        self.break_typing_group();
        if !self.selection().is_empty() {
            self.delete_selection();
            return;
        }
        let target = if line_end {
            let range = view::line_range(&self.doc, self.doc.line_of_offset(self.cursor));
            if backwards { range.start } else { range.end }
        } else if word {
            let Some(target) = self.word_target(backwards) else {
                return;
            };
            target
        } else if backwards {
            view::previous(&self.doc, self.cursor)
        } else {
            view::next(&self.doc, self.cursor)
        };
        self.delete_range(target.min(self.cursor)..target.max(self.cursor));
    }

    pub(super) fn word_target(&mut self, backwards: bool) -> Option<usize> {
        self.break_typing_group();
        match view::word_target(&self.doc, self.cursor, backwards) {
            Ok(target) => Some(target),
            Err(message) => {
                self.status = message.into();
                None
            }
        }
    }

    pub(super) fn move_vertical(&mut self, target_line: usize, select: bool) {
        let source = view::line_range(&self.doc, self.doc.line_of_offset(self.cursor));
        let column = self.preferred_column.unwrap_or_else(|| {
            self.doc
                .scalar_count(source.start..self.cursor.min(source.end))
        });
        let target = view::line_range(&self.doc, target_line);
        // A very large preferred column on a short line need not look into
        // subsequent lines. A scalar always occupies at least one byte.
        let offset = if column >= target.len() {
            target.end
        } else {
            self.doc
                .offset_at_scalar(target.start, column)
                .min(target.end)
        };
        self.move_to(offset, select);
        self.preferred_column = Some(column);
    }

    pub(super) fn delete_surrounding(&mut self, before_chars: usize, after_chars: usize) {
        self.break_typing_group();
        if before_chars > MAX_IME_SCALARS || after_chars > MAX_IME_SCALARS {
            self.status =
                "IME deletion exceeds the 4096-character safety limit; nothing was changed".into();
            return;
        }
        let minimum = view::content_start(&self.doc);
        let selection = self.selection();
        let mut start = selection.start;
        let mut end = selection.end;
        // IME protocol counts Unicode scalar values, not editor cursor steps:
        // CR and LF count separately even though navigation treats them as one.
        for _ in 0..before_chars {
            start = self.doc.previous_char(start).max(minimum);
            if start == minimum {
                break;
            }
        }
        for _ in 0..after_chars {
            end = self.doc.next_char(end);
            if end == self.doc.len() {
                break;
            }
        }
        if start == selection.start && end == selection.end {
            return;
        }
        // Do not expand a half-pair request: the IME owns scalar counts, so
        // deleting just CR or LF must not remove an unrequested partner.
        if selection.is_empty() {
            self.delete_range(start..end);
        } else {
            // Like egui's text buffer, delete around (not through) an existing
            // selection, retain its direction, and make both sides one undo.
            let mut ranges = Vec::with_capacity(2);
            if start < selection.start {
                ranges.push(start..selection.start);
            }
            if selection.end < end {
                ranges.push(selection.end..end);
            }
            let before = self.cursor_state();
            let revision = self.doc.revision();
            let cursor = view::map_offset(self.cursor, &ranges, 0);
            let anchor = self
                .anchor
                .map(|anchor| view::map_offset(anchor, &ranges, 0));
            match self.doc.replace_ranges(&ranges, "") {
                Ok(()) => {
                    self.cursor = cursor;
                    self.anchor = anchor;
                    self.changed();
                    self.record_cursor_edit(revision, before);
                }
                Err(error) => self.error = Some(error),
            }
        }
    }

    pub(super) fn insert_newline(&mut self) {
        self.break_typing_group();
        let position = self.selection().start;
        let line = self.doc.line_of_offset(position);
        let range = view::line_range(&self.doc, line);
        let end = position
            .min(range.end)
            .min(range.start.saturating_add(MAX_AUTOINDENT + 1));
        let prefix = self.doc.read(range.start..end);
        let indentation = prefix
            .iter()
            .take_while(|byte| matches!(**byte, b' ' | b'\t'))
            .count();
        if indentation > MAX_AUTOINDENT {
            self.status =
                "Automatic indentation exceeds the 4096-byte safety limit; nothing was changed"
                    .into();
            return;
        }
        let mut text = self.newline_at(position).to_owned();
        text.push_str(std::str::from_utf8(&prefix[..indentation]).expect("ASCII indentation"));
        self.insert(&text);
    }

    pub(super) fn indent_selection(&mut self, outdent: bool) {
        self.break_typing_group();
        if !self.ready {
            return;
        }
        let selection = self.selection();
        if selection.is_empty() && !outdent {
            self.insert("\t");
            return;
        }
        let first = self.doc.line_of_offset(selection.start);
        let mut last = self.doc.line_of_offset(selection.end);
        // A selection ending at the next line start does not touch that line.
        if !selection.is_empty() && last > first && selection.end == self.doc.line_start(last) {
            last -= 1;
        }
        if last.saturating_sub(first) >= MAX_BLOCK_LINES {
            self.status =
                "Indentation is limited to 16384 lines at once; nothing was changed".into();
            return;
        }
        let mut ranges = Vec::with_capacity(last - first + 1);
        for line in first..=last {
            let range = view::line_range(&self.doc, line);
            if outdent {
                let prefix = self.doc.read(range.start..range.end.min(range.start + 4));
                let count = if prefix.first() == Some(&b'\t') {
                    1
                } else {
                    prefix.iter().take_while(|byte| **byte == b' ').count()
                };
                if count > 0 {
                    ranges.push(range.start..range.start + count);
                }
            } else {
                ranges.push(range.start..range.start);
            }
        }
        if ranges.is_empty() {
            return;
        }
        let text = if outdent { "" } else { "\t" };
        let before = self.cursor_state();
        let revision = self.doc.revision();
        let cursor = view::map_offset(self.cursor, &ranges, text.len());
        let anchor = self
            .anchor
            .map(|anchor| view::map_offset(anchor, &ranges, text.len()));
        match self.doc.replace_ranges(&ranges, text) {
            Ok(()) => {
                self.cursor = cursor;
                self.anchor = anchor;
                self.changed();
                self.record_cursor_edit(revision, before);
            }
            Err(error) => self.error = Some(error),
        }
    }
}
