//! An intentionally small, bounded Vim-style editing layer.
//!
//! This is not a Vim emulator: there are no macros, marks, text objects, named
//! registers, search commands, dot-repeat, insert counts, or visual-line mode.
//! The parser never edits a document. The application executes explicit actions
//! and owns saving, close confirmation, undo, and the insert-mode text input.
//! All planning is atomic: a limit error returns no partial edit or movement.
use crate::document::Document;
use std::fmt;
use std::ops::Range;

pub const MAX_COUNT: usize = 1_000;
pub const SCAN_LIMIT: usize = 64 * 1024;
pub const EDIT_LIMIT: usize = 16 * 1024 * 1024;
const COMMAND_LIMIT: usize = 32;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Visual,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Char(char),
    Escape,
    Enter,
    Backspace,
    Left,
    Right,
    Up,
    Down,
    CtrlR,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    LineStart,
    FirstNonBlank,
    LineEnd,
    WordForward,
    WordBackward,
    WordEnd,
    /// With a count, the one-based absolute destination line.
    FirstLine,
    LastLine,
    /// Operator-only: the current line and count - 1 following lines.
    Line,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operator {
    Delete,
    Change,
    Yank,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InsertKind {
    Before,
    FirstNonBlank,
    After,
    LineEnd,
    OpenBelow,
    OpenAbove,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExCommand {
    Write,
    /// Request a safe close, never discard modified content implicitly.
    Quit,
    /// Close only after a successful save, not merely after starting a save.
    WriteQuit,
    /// The explicit :q! command authorizes discarding the current buffer.
    ForceQuit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    None,
    PassThrough,
    Move {
        motion: Motion,
        count: usize,
    },
    Insert(InsertKind),
    Normal {
        from_insert: bool,
    },
    StartVisual,
    EndVisual,
    Operate {
        operator: Operator,
        motion: Motion,
        count: usize,
    },
    OperateVisual(Operator),
    DeleteChars(usize),
    Paste {
        before: bool,
        count: usize,
    },
    Undo(usize),
    Redo(usize),
    Ex(ExCommand),
    Message(&'static str),
}

#[derive(Clone, Debug, Default)]
pub struct VimState {
    mode: Mode,
    count: Option<usize>,
    operator: Option<(Operator, Option<usize>)>,
    pending_g: bool,
    command: Option<String>,
}

impl VimState {
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Text after the colon, including an empty string while entering a command.
    pub fn command_line(&self) -> Option<&str> {
        self.command.as_deref()
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Also cancels an unfinished count, operator, or command.
    pub fn set_mode(&mut self, mode: Mode) {
        self.reset();
        self.mode = mode;
    }

    fn clear_pending(&mut self) {
        self.count = None;
        self.operator = None;
        self.pending_g = false;
    }

    fn reject(&mut self, message: &'static str) -> Action {
        self.clear_pending();
        Action::Message(message)
    }

    pub fn handle(&mut self, key: Key) -> Action {
        if key == Key::Escape {
            let from_insert = self.mode == Mode::Insert;
            self.reset();
            return Action::Normal { from_insert };
        }
        if let Some(command) = self.command.as_mut() {
            return match key {
                Key::Enter => {
                    let command = self.command.take().unwrap();
                    match command.trim() {
                        "w" => Action::Ex(ExCommand::Write),
                        "q" => Action::Ex(ExCommand::Quit),
                        "wq" => Action::Ex(ExCommand::WriteQuit),
                        "q!" => Action::Ex(ExCommand::ForceQuit),
                        "" => Action::None,
                        _ => Action::Message("Supported commands: :w, :q, :wq, :q!"),
                    }
                }
                Key::Backspace => {
                    if command.pop().is_none() {
                        self.command = None;
                    }
                    Action::None
                }
                Key::Char(c) if !c.is_control() => {
                    if command.len() + c.len_utf8() <= COMMAND_LIMIT {
                        command.push(c);
                        Action::None
                    } else {
                        Action::Message("Vim command is too long")
                    }
                }
                _ => Action::None,
            };
        }
        if self.mode == Mode::Insert {
            return Action::PassThrough;
        }
        if self.pending_g {
            self.pending_g = false;
            if key == Key::Char('g') {
                return self.finish_motion(Motion::FirstLine);
            }
            return self.reject("Only gg is supported after g");
        }
        if let Key::Char(c @ '0'..='9') = key
            && (c != '0' || self.count.is_some())
        {
            let count = self.count.unwrap_or(0) * 10 + c.to_digit(10).unwrap() as usize;
            if count > MAX_COUNT {
                return self.reject("Vim count exceeds the 1000-step safety limit");
            }
            self.count = Some(count);
            return Action::None;
        }
        let motion = match key {
            Key::Char('h') | Key::Left => Some(Motion::Left),
            Key::Char('j') | Key::Down => Some(Motion::Down),
            Key::Char('k') | Key::Up => Some(Motion::Up),
            Key::Char('l') | Key::Right => Some(Motion::Right),
            Key::Char('0') => Some(Motion::LineStart),
            Key::Char('^') => Some(Motion::FirstNonBlank),
            Key::Char('$') => Some(Motion::LineEnd),
            Key::Char('w') => Some(Motion::WordForward),
            Key::Char('b') => Some(Motion::WordBackward),
            Key::Char('e') => Some(Motion::WordEnd),
            Key::Char('G') => Some(
                if self.count.is_some() || self.operator.is_some_and(|(_, n)| n.is_some()) {
                    Motion::FirstLine
                } else {
                    Motion::LastLine
                },
            ),
            _ => None,
        };
        if let Some(motion) = motion {
            return self.finish_motion(motion);
        }
        if key == Key::Char('g') {
            self.pending_g = true;
            return Action::None;
        }
        let operator = match key {
            Key::Char('d') => Some(Operator::Delete),
            Key::Char('c') => Some(Operator::Change),
            Key::Char('y') => Some(Operator::Yank),
            _ => None,
        };
        if let Some(operator) = operator {
            if self.mode == Mode::Visual {
                self.set_mode(if operator == Operator::Change {
                    Mode::Insert
                } else {
                    Mode::Normal
                });
                return Action::OperateVisual(operator);
            }
            if let Some((pending, _)) = self.operator {
                if pending == operator {
                    return self.finish_motion(Motion::Line);
                }
                return self.reject("Unsupported Vim operator combination");
            }
            self.operator = Some((operator, self.count.take()));
            return Action::None;
        }
        if self.operator.is_some() {
            return self.reject("Unsupported Vim motion; operator cancelled");
        }
        let count = self.count.take().unwrap_or(1);
        match key {
            Key::Char('v') => {
                if self.mode == Mode::Visual {
                    self.set_mode(Mode::Normal);
                    Action::EndVisual
                } else {
                    self.set_mode(Mode::Visual);
                    Action::StartVisual
                }
            }
            Key::Char('x') if self.mode == Mode::Visual => {
                self.set_mode(Mode::Normal);
                Action::OperateVisual(Operator::Delete)
            }
            Key::Char('x') => Action::DeleteChars(count),
            Key::Char('p' | 'P') if self.mode == Mode::Visual => {
                Action::Message("Visual replacement paste is not supported; use Escape then p")
            }
            Key::Char('p') => Action::Paste {
                before: false,
                count,
            },
            Key::Char('P') => Action::Paste {
                before: true,
                count,
            },
            Key::Char('u') if self.mode == Mode::Normal => Action::Undo(count),
            Key::CtrlR if self.mode == Mode::Normal => Action::Redo(count),
            Key::Char(':') if self.mode == Mode::Normal => {
                self.command = Some(String::new());
                Action::None
            }
            Key::Char(c @ ('i' | 'I' | 'a' | 'A' | 'o' | 'O')) if self.mode == Mode::Normal => {
                if count != 1 {
                    return Action::Message("Counted insert/open commands are not supported");
                }
                self.set_mode(Mode::Insert);
                Action::Insert(match c {
                    'i' => InsertKind::Before,
                    'I' => InsertKind::FirstNonBlank,
                    'a' => InsertKind::After,
                    'A' => InsertKind::LineEnd,
                    'o' => InsertKind::OpenBelow,
                    _ => InsertKind::OpenAbove,
                })
            }
            _ => Action::Message("Key is not part of SimpTxt's Vim subset"),
        }
    }

    fn finish_motion(&mut self, motion: Motion) -> Action {
        let count = self.count.take().unwrap_or(1);
        let operator = self.operator.take();
        if let Some((operator, prefix_count)) = operator {
            let count = count.saturating_mul(prefix_count.unwrap_or(1));
            if count > MAX_COUNT {
                return self.reject("Combined Vim count exceeds the 1000-step safety limit");
            }
            if operator == Operator::Change {
                self.mode = Mode::Insert;
            }
            Action::Operate {
                operator,
                motion,
                count,
            }
        } else {
            Action::Move { motion, count }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VimError {
    CountLimit,
    ScanLimit,
    EditLimit,
    InvalidNewline,
}

impl fmt::Display for VimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CountLimit => "Vim count exceeds the 1000-step safety limit; nothing changed",
            Self::ScanLimit => "Vim motion exceeds the 64 KiB scan limit; use a nearer position",
            Self::EditLimit => "Vim operation exceeds the 16 MiB limit; select a smaller region",
            Self::InvalidNewline => "Expected LF or CRLF line ending; nothing changed",
        })
    }
}
impl std::error::Error for VimError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperatorRange {
    /// Bytes to delete/change. Last-line deletion can include the preceding EOL.
    pub range: Range<usize>,
    /// Bytes to store in the register, without that preceding EOL.
    pub register_range: Range<usize>,
    pub linewise: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Register {
    pub text: String,
    pub linewise: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InsertPlan {
    pub offset: usize,
    pub text: String,
    pub cursor: usize,
}

struct Budget(usize);
impl Budget {
    fn charge(&mut self, bytes: usize) -> Result<(), VimError> {
        self.0 = self
            .0
            .checked_sub(bytes.max(1))
            .ok_or(VimError::ScanLimit)?;
        Ok(())
    }
}

fn checked_count(count: usize) -> Result<usize, VimError> {
    if count == 0 || count > MAX_COUNT {
        Err(VimError::CountLimit)
    } else {
        Ok(count)
    }
}

pub fn content_start(doc: &Document) -> usize {
    if doc.len() >= 3 && doc.read(0..3) == [0xef, 0xbb, 0xbf] {
        3
    } else {
        0
    }
}

/// Content excluding a BOM and an LF/CRLF terminator. Uses the document index,
/// never a linear scan of the whole line.
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

fn boundary(doc: &Document, cursor: usize) -> usize {
    let mut cursor = cursor.clamp(content_start(doc), doc.len());
    while !doc.char_boundary(cursor) {
        cursor -= 1;
    }
    if cursor > 0 && cursor < doc.len() && doc.read(cursor - 1..cursor + 1) == b"\r\n" {
        cursor -= 1;
    }
    cursor
}

/// Clamp to a Normal/Visual cursor: a complete character, or an empty line.
pub fn normal_cursor(doc: &Document, cursor: usize) -> usize {
    let cursor = boundary(doc, cursor);
    let range = line_range(doc, doc.line_of_offset(cursor));
    if cursor >= range.end && !range.is_empty() {
        doc.previous_char(range.end)
    } else {
        cursor.max(range.start)
    }
}

fn next(doc: &Document, cursor: usize) -> usize {
    if cursor + 2 <= doc.len() && doc.read(cursor..cursor + 2) == b"\r\n" {
        cursor + 2
    } else {
        doc.next_char(cursor)
    }
}
fn previous(doc: &Document, cursor: usize) -> usize {
    if cursor >= 2 && doc.read(cursor - 2..cursor) == b"\r\n" {
        (cursor - 2).max(content_start(doc))
    } else {
        doc.previous_char(cursor).max(content_start(doc))
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Class {
    Space,
    Word,
    Punctuation,
}
fn class_at(doc: &Document, cursor: usize) -> Class {
    let bytes = doc.read(cursor..doc.next_char(cursor));
    let ch = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|s| s.chars().next())
        .unwrap_or('\n');
    if ch.is_whitespace() {
        Class::Space
    } else if ch.is_alphanumeric() || ch == '_' {
        Class::Word
    } else {
        Class::Punctuation
    }
}
fn step_next(doc: &Document, cursor: usize, budget: &mut Budget) -> Result<usize, VimError> {
    let target = next(doc, cursor);
    budget.charge(target - cursor)?;
    Ok(target)
}
fn step_previous(doc: &Document, cursor: usize, budget: &mut Budget) -> Result<usize, VimError> {
    let target = previous(doc, cursor);
    budget.charge(cursor - target)?;
    Ok(target)
}
fn first_nonblank(doc: &Document, line: usize, budget: &mut Budget) -> Result<usize, VimError> {
    let range = line_range(doc, line);
    let mut cursor = range.start;
    while cursor < range.end && class_at(doc, cursor) == Class::Space {
        cursor = step_next(doc, cursor, budget)?;
    }
    Ok(cursor)
}

fn word_forward(doc: &Document, mut cursor: usize, budget: &mut Budget) -> Result<usize, VimError> {
    if cursor >= doc.len() {
        return Ok(cursor);
    }
    let class = class_at(doc, cursor);
    while cursor < doc.len() && class_at(doc, cursor) == class {
        cursor = step_next(doc, cursor, budget)?;
    }
    while cursor < doc.len() && class_at(doc, cursor) == Class::Space {
        cursor = step_next(doc, cursor, budget)?;
    }
    Ok(cursor)
}
fn word_backward(
    doc: &Document,
    mut cursor: usize,
    budget: &mut Budget,
) -> Result<usize, VimError> {
    let start = content_start(doc);
    if cursor <= start {
        return Ok(start);
    }
    cursor = step_previous(doc, cursor, budget)?;
    while cursor > start && class_at(doc, cursor) == Class::Space {
        cursor = step_previous(doc, cursor, budget)?;
    }
    let class = class_at(doc, cursor);
    while cursor > start {
        let prior = previous(doc, cursor);
        if class_at(doc, prior) != class {
            break;
        }
        budget.charge(cursor - prior)?;
        cursor = prior;
    }
    Ok(cursor)
}
fn word_end(doc: &Document, mut cursor: usize, budget: &mut Budget) -> Result<usize, VimError> {
    if cursor >= doc.len() {
        return Ok(cursor);
    }
    cursor = step_next(doc, cursor, budget)?;
    while cursor < doc.len() && class_at(doc, cursor) == Class::Space {
        cursor = step_next(doc, cursor, budget)?;
    }
    if cursor == doc.len() {
        return Ok(previous(doc, cursor));
    }
    let class = class_at(doc, cursor);
    loop {
        let after = next(doc, cursor);
        if after == doc.len() || class_at(doc, after) != class {
            return Ok(cursor);
        }
        budget.charge(after - cursor)?;
        cursor = after;
    }
}

fn raw_motion(
    doc: &Document,
    cursor: usize,
    motion: Motion,
    count: usize,
    budget: &mut Budget,
) -> Result<usize, VimError> {
    checked_count(count)?;
    let line = doc.line_of_offset(cursor);
    let range = line_range(doc, line);
    let mut target = cursor;
    match motion {
        Motion::Left | Motion::Right => {
            for _ in 0..count {
                target = if motion == Motion::Left {
                    if target <= range.start {
                        break;
                    }
                    step_previous(doc, target, budget)?
                } else {
                    if target >= range.end {
                        break;
                    }
                    step_next(doc, target, budget)?.min(range.end)
                };
            }
        }
        Motion::Up | Motion::Down => {
            let target_line = if motion == Motion::Up {
                line.saturating_sub(count)
            } else {
                line.saturating_add(count).min(doc.line_count() - 1)
            };
            if target_line == line {
                return Ok(cursor);
            }
            // Scalar columns, never byte columns that could split UTF-8.
            let mut column = 0usize;
            let mut pos = range.start;
            while pos < cursor {
                pos = step_next(doc, pos, budget)?;
                column += 1;
            }
            let target_range = line_range(doc, target_line);
            target = target_range.start;
            for _ in 0..column {
                if target == target_range.end {
                    break;
                }
                target = step_next(doc, target, budget)?;
            }
        }
        Motion::LineStart => target = range.start,
        Motion::FirstNonBlank => target = first_nonblank(doc, line, budget)?,
        Motion::LineEnd => {
            let last_line = line.saturating_add(count - 1).min(doc.line_count() - 1);
            let last = line_range(doc, last_line);
            target = if last.is_empty() {
                last.start
            } else {
                doc.previous_char(last.end)
            };
        }
        Motion::FirstLine | Motion::LastLine => {
            let line = if motion == Motion::LastLine {
                doc.line_count() - 1
            } else {
                (count - 1).min(doc.line_count() - 1)
            };
            target = first_nonblank(doc, line, budget)?;
        }
        Motion::WordForward | Motion::WordBackward | Motion::WordEnd => {
            for _ in 0..count {
                target = match motion {
                    Motion::WordForward => word_forward(doc, target, budget)?,
                    Motion::WordBackward => word_backward(doc, target, budget)?,
                    _ => word_end(doc, target, budget)?,
                };
            }
        }
        Motion::Line => target = range.start,
    }
    Ok(target)
}

pub fn motion_target(
    doc: &Document,
    cursor: usize,
    motion: Motion,
    count: usize,
) -> Result<usize, VimError> {
    let cursor = normal_cursor(doc, cursor);
    let target = raw_motion(doc, cursor, motion, count, &mut Budget(SCAN_LIMIT))?;
    Ok(normal_cursor(doc, target))
}

fn bounded_range(range: Range<usize>) -> Result<Range<usize>, VimError> {
    if range.len() > EDIT_LIMIT {
        Err(VimError::EditLimit)
    } else {
        Ok(range)
    }
}

/// Inclusive display range. This computes endpoints only and does not allocate
/// selected text, so large selections may still be painted while edits refuse.
pub fn visual_selection_range(doc: &Document, anchor: usize, cursor: usize) -> Range<usize> {
    let anchor = normal_cursor(doc, anchor);
    let cursor = normal_cursor(doc, cursor);
    anchor.min(cursor)..next(doc, anchor.max(cursor))
}

pub fn visual_range(
    doc: &Document,
    anchor: usize,
    cursor: usize,
) -> Result<Range<usize>, VimError> {
    bounded_range(visual_selection_range(doc, anchor, cursor))
}

pub fn delete_chars_range(
    doc: &Document,
    cursor: usize,
    count: usize,
) -> Result<Range<usize>, VimError> {
    let cursor = normal_cursor(doc, cursor);
    let end = raw_motion(doc, cursor, Motion::Right, count, &mut Budget(SCAN_LIMIT))?;
    bounded_range(cursor..end)
}

pub fn operator_range(
    doc: &Document,
    cursor: usize,
    operator: Operator,
    motion: Motion,
    count: usize,
) -> Result<OperatorRange, VimError> {
    checked_count(count)?;
    let cursor = normal_cursor(doc, cursor);
    let line = doc.line_of_offset(cursor);
    let linewise = matches!(
        motion,
        Motion::Line | Motion::Up | Motion::Down | Motion::FirstLine | Motion::LastLine
    );
    if linewise {
        let target_line = match motion {
            Motion::Line => line.saturating_add(count - 1).min(doc.line_count() - 1),
            Motion::Up => line.saturating_sub(count),
            Motion::Down => line.saturating_add(count).min(doc.line_count() - 1),
            Motion::FirstLine => (count - 1).min(doc.line_count() - 1),
            _ => doc.line_count() - 1,
        };
        let first = line.min(target_line);
        let last = line.max(target_line);
        let start = line_range(doc, first).start;
        let end = if last + 1 < doc.line_count() {
            doc.line_start(last + 1)
        } else {
            doc.len()
        };
        let register_range = bounded_range(start..end)?;
        let range = if operator == Operator::Change {
            start..line_range(doc, last).end
        } else if operator == Operator::Delete && last + 1 == doc.line_count() && first > 0 {
            line_range(doc, first - 1).end..end
        } else {
            start..end
        };
        return Ok(OperatorRange {
            range: bounded_range(range)?,
            register_range,
            linewise,
        });
    }
    let mut budget = Budget(SCAN_LIMIT);
    // cw changes the word, not its following whitespace, when on a word.
    let change_word = operator == Operator::Change
        && motion == Motion::WordForward
        && class_at(doc, cursor) != Class::Space;
    let target = if change_word {
        // Unlike e, cw on the last character of a word changes that character.
        let mut pos = cursor;
        for index in 0..count {
            if index > 0 {
                pos = word_forward(doc, pos, &mut budget)?;
            }
            if pos >= doc.len() {
                break;
            }
            let class = class_at(doc, pos);
            loop {
                let after = next(doc, pos);
                if after == doc.len() || class_at(doc, after) != class {
                    break;
                }
                budget.charge(after - pos)?;
                pos = after;
            }
        }
        pos
    } else {
        raw_motion(doc, cursor, motion, count, &mut budget)?
    };
    let inclusive = matches!(motion, Motion::WordEnd | Motion::LineEnd) || change_word;
    let start = cursor.min(target);
    let mut end = cursor.max(target);
    if inclusive && end < doc.len() {
        // $ on an empty line must not delete its newline.
        let end_line = line_range(doc, doc.line_of_offset(end));
        if end < end_line.end {
            end = next(doc, end);
        }
    }
    let range = bounded_range(start..end)?;
    Ok(OperatorRange {
        register_range: range.clone(),
        range,
        linewise: false,
    })
}

fn valid_newline(newline: &str) -> Result<(), VimError> {
    if matches!(newline, "\n" | "\r\n") {
        Ok(())
    } else {
        Err(VimError::InvalidNewline)
    }
}

pub fn insert_plan(
    doc: &Document,
    cursor: usize,
    kind: InsertKind,
    newline: &str,
) -> Result<InsertPlan, VimError> {
    valid_newline(newline)?;
    let cursor = normal_cursor(doc, cursor);
    let line = doc.line_of_offset(cursor);
    let range = line_range(doc, line);
    let mut text = String::new();
    let (offset, cursor) = match kind {
        InsertKind::Before => (cursor, cursor),
        InsertKind::FirstNonBlank => {
            let pos = first_nonblank(doc, line, &mut Budget(SCAN_LIMIT))?;
            (pos, pos)
        }
        InsertKind::After => {
            let pos = if cursor < range.end {
                next(doc, cursor)
            } else {
                cursor
            };
            (pos, pos)
        }
        InsertKind::LineEnd => (range.end, range.end),
        InsertKind::OpenAbove => {
            text.push_str(newline);
            (range.start, range.start)
        }
        InsertKind::OpenBelow => {
            text.push_str(newline);
            if line + 1 < doc.line_count() {
                let pos = doc.line_start(line + 1);
                (pos, pos)
            } else {
                (range.end, range.end + newline.len())
            }
        }
    };
    Ok(InsertPlan {
        offset,
        text,
        cursor,
    })
}

pub fn paste_plan(
    doc: &Document,
    cursor: usize,
    register: &Register,
    before: bool,
    count: usize,
    newline: &str,
) -> Result<InsertPlan, VimError> {
    checked_count(count)?;
    valid_newline(newline)?;
    let cursor = normal_cursor(doc, cursor);
    let line = doc.line_of_offset(cursor);
    let range = line_range(doc, line);
    if !register.linewise {
        if register.text.is_empty() {
            return Ok(InsertPlan {
                offset: cursor,
                text: String::new(),
                cursor,
            });
        }
        let total = register
            .text
            .len()
            .checked_mul(count)
            .ok_or(VimError::EditLimit)?;
        if total > EDIT_LIMIT {
            return Err(VimError::EditLimit);
        }
        let offset = if before || cursor >= range.end {
            cursor
        } else {
            next(doc, cursor)
        };
        let text = register.text.repeat(count);
        let last = text.char_indices().next_back().map_or(0, |(i, _)| i);
        let target = if text.ends_with("\r\n") {
            offset + text.len() - 2
        } else {
            offset + last
        };
        return Ok(InsertPlan {
            offset,
            text,
            cursor: target,
        });
    }
    // Registers preserve original line endings. Add only missing separators,
    // using the current line's newline convention.
    let needs_end = !register.text.ends_with('\n');
    let unit_len = register
        .text
        .len()
        .checked_add(if needs_end { newline.len() } else { 0 })
        .ok_or(VimError::EditLimit)?;
    let total = unit_len
        .checked_mul(count)
        .and_then(|n| n.checked_add(newline.len()))
        .ok_or(VimError::EditLimit)?;
    if total > EDIT_LIMIT {
        return Err(VimError::EditLimit);
    }
    // An entirely empty buffer has no line to paste below. Reuse its empty
    // first line instead of introducing a surprising leading blank line.
    let below_eof = !before && line + 1 == doc.line_count() && doc.len() > content_start(doc);
    let offset = if before {
        range.start
    } else if below_eof {
        range.end
    } else {
        doc.line_start(line + 1)
    };
    let mut text = String::with_capacity(total);
    if below_eof {
        text.push_str(newline);
    }
    let target = offset + text.len();
    for _ in 0..count {
        text.push_str(&register.text);
        if needs_end {
            text.push_str(newline);
        }
    }
    if below_eof {
        // Keep an unterminated final line unterminated. The saved register is
        // unchanged and can still be pasted into the middle of another buffer.
        if text.ends_with("\r\n") {
            text.truncate(text.len() - 2);
        } else if text.ends_with('\n') {
            text.pop();
        }
    }
    Ok(InsertPlan {
        offset,
        text,
        cursor: target,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn doc(text: &str) -> Document {
        Document::from_bytes(text.as_bytes().to_vec()).unwrap()
    }
    fn keys(state: &mut VimState, keys: &str) -> Action {
        keys.chars()
            .map(|c| state.handle(Key::Char(c)))
            .last()
            .unwrap_or(Action::None)
    }
    fn edit(
        text: &str,
        cursor: usize,
        operator: Operator,
        motion: Motion,
        count: usize,
    ) -> (String, String) {
        let mut d = doc(text);
        let plan = operator_range(&d, cursor, operator, motion, count).unwrap();
        let register = String::from_utf8(d.read(plan.register_range)).unwrap();
        if operator != Operator::Yank {
            d.replace(plan.range, "").unwrap();
        }
        (String::from_utf8(d.read(0..d.len())).unwrap(), register)
    }
    #[test]
    fn parser_counts_operators_and_cancellation() {
        let mut s = VimState::default();
        assert_eq!(
            keys(&mut s, "2d3w"),
            Action::Operate {
                operator: Operator::Delete,
                motion: Motion::WordForward,
                count: 6
            }
        );
        assert_eq!(
            keys(&mut s, "dd"),
            Action::Operate {
                operator: Operator::Delete,
                motion: Motion::Line,
                count: 1
            }
        );
        assert_eq!(
            keys(&mut s, "10j"),
            Action::Move {
                motion: Motion::Down,
                count: 10
            }
        );
        assert_eq!(
            keys(&mut s, "0"),
            Action::Move {
                motion: Motion::LineStart,
                count: 1
            }
        );
        keys(&mut s, "2d");
        s.handle(Key::Escape);
        assert_eq!(
            keys(&mut s, "w"),
            Action::Move {
                motion: Motion::WordForward,
                count: 1
            }
        );
        assert!(matches!(keys(&mut s, "1001"), Action::Message(_)));
        assert!(matches!(keys(&mut s, "100d100w"), Action::Message(_)));
    }
    #[test]
    fn parser_modes_and_commands() {
        let mut s = VimState::default();
        assert_eq!(keys(&mut s, "i"), Action::Insert(InsertKind::Before));
        assert_eq!(s.mode(), Mode::Insert);
        assert_eq!(keys(&mut s, "x"), Action::PassThrough);
        assert_eq!(s.handle(Key::Escape), Action::Normal { from_insert: true });
        assert_eq!(keys(&mut s, "v"), Action::StartVisual);
        assert_eq!(keys(&mut s, "c"), Action::OperateVisual(Operator::Change));
        assert_eq!(s.mode(), Mode::Insert);
        s.handle(Key::Escape);
        for (command, expected) in [
            (":w", ExCommand::Write),
            (":q", ExCommand::Quit),
            (":wq", ExCommand::WriteQuit),
            (":q!", ExCommand::ForceQuit),
        ] {
            keys(&mut s, command);
            assert_eq!(s.handle(Key::Enter), Action::Ex(expected));
        }
        keys(&mut s, ":unknown");
        assert!(matches!(s.handle(Key::Enter), Action::Message(_)));
        assert_eq!(
            keys(&mut s, "gg"),
            Action::Move {
                motion: Motion::FirstLine,
                count: 1
            }
        );
        assert_eq!(
            keys(&mut s, "5G"),
            Action::Move {
                motion: Motion::FirstLine,
                count: 5
            }
        );
        assert_eq!(
            keys(&mut s, "G"),
            Action::Move {
                motion: Motion::LastLine,
                count: 1
            }
        );
    }
    #[test]
    fn unicode_bom_crlf_and_horizontal_limits() {
        let d = doc("\u{feff}aβ💚\r\né\r\n");
        assert_eq!(content_start(&d), 3);
        assert_eq!(normal_cursor(&d, 1), 3);
        assert_eq!(normal_cursor(&d, 11), 6); // LF byte snaps before CRLF, then to last character.
        assert_eq!(motion_target(&d, 3, Motion::Right, 100).unwrap(), 6);
        assert_eq!(motion_target(&d, 6, Motion::Left, 100).unwrap(), 3);
        assert_eq!(delete_chars_range(&d, 6, 100).unwrap(), 6..10);
        let r = visual_range(&d, 6, 3).unwrap();
        assert_eq!(String::from_utf8(d.read(r)).unwrap(), "aβ💚");
        assert_eq!(
            edit("\u{feff}a\r\nb", 3, Operator::Delete, Motion::Line, 1).0,
            "\u{feff}b"
        );
    }
    #[test]
    fn word_motions_and_operator_endpoints() {
        let d = doc("one two, β!\r\nlast");
        assert_eq!(motion_target(&d, 0, Motion::WordForward, 1).unwrap(), 4);
        assert_eq!(motion_target(&d, 0, Motion::WordEnd, 1).unwrap(), 2);
        assert_eq!(motion_target(&d, 4, Motion::WordBackward, 1).unwrap(), 0);
        assert_eq!(motion_target(&d, 4, Motion::WordForward, 2).unwrap(), 9);
        assert_eq!(
            edit("one two", 0, Operator::Delete, Motion::WordForward, 1).0,
            "two"
        );
        assert_eq!(
            edit("one two", 0, Operator::Change, Motion::WordForward, 1).0,
            " two"
        );
        assert_eq!(
            edit("one two", 2, Operator::Change, Motion::WordForward, 1).0,
            "on two"
        );
        assert_eq!(
            edit("one two", 0, Operator::Change, Motion::WordForward, 2).0,
            ""
        );
        assert_eq!(
            edit("one two", 0, Operator::Delete, Motion::WordEnd, 1).0,
            " two"
        );
        assert_eq!(edit("abc", 2, Operator::Delete, Motion::Right, 1).0, "ab");
        assert_eq!(edit("abc", 2, Operator::Delete, Motion::Left, 1).0, "ac");
    }
    #[test]
    fn line_operations_preserve_endings_and_register_payload() {
        assert_eq!(
            edit("a\r\nb\r\nc", 3, Operator::Delete, Motion::Line, 1),
            ("a\r\nc".into(), "b\r\n".into())
        );
        assert_eq!(
            edit("a\r\nb", 3, Operator::Delete, Motion::Line, 1),
            ("a".into(), "b".into())
        );
        assert_eq!(
            edit("a\r\nb\r\nc", 3, Operator::Change, Motion::Line, 1).0,
            "a\r\n\r\nc"
        );
        assert_eq!(
            edit("a\nb\nc", 2, Operator::Change, Motion::Line, 2).0,
            "a\n"
        );
        assert_eq!(
            edit("a\nb\nc", 2, Operator::Delete, Motion::FirstLine, 1).0,
            "c"
        );
    }
    #[test]
    fn vertical_movement_uses_scalar_columns() {
        let d = doc("é💚z\nabc\nx\n");
        assert_eq!(motion_target(&d, 6, Motion::Down, 1).unwrap(), 10);
        assert_eq!(motion_target(&d, 6, Motion::Down, 2).unwrap(), 12);
        assert_eq!(motion_target(&d, 10, Motion::Up, 1).unwrap(), 6);
    }
    #[test]
    fn open_and_paste_plans_preserve_bom_crlf() {
        let mut d = doc("\u{feff}a\r\nb");
        let plan = insert_plan(&d, 3, InsertKind::OpenAbove, "\r\n").unwrap();
        assert_eq!(
            plan,
            InsertPlan {
                offset: 3,
                text: "\r\n".into(),
                cursor: 3
            }
        );
        let plan = insert_plan(&d, 3, InsertKind::OpenBelow, "\r\n").unwrap();
        assert_eq!(
            plan,
            InsertPlan {
                offset: 6,
                text: "\r\n".into(),
                cursor: 6
            }
        );
        let plan = paste_plan(
            &d,
            6,
            &Register {
                text: "β\r\n".into(),
                linewise: true,
            },
            false,
            2,
            "\r\n",
        )
        .unwrap();
        d.replace(plan.offset..plan.offset, &plan.text).unwrap();
        assert_eq!(
            String::from_utf8(d.read(0..d.len())).unwrap(),
            "\u{feff}a\r\nb\r\nβ\r\nβ"
        );
        assert_eq!(plan.cursor, 9);
        let plan = paste_plan(
            &d,
            3,
            &Register {
                text: "💚".into(),
                linewise: false,
            },
            true,
            2,
            "\r\n",
        )
        .unwrap();
        assert_eq!(plan.offset, 3);
        assert_eq!(plan.cursor, 7);
        let empty = paste_plan(&d, 3, &Register::default(), false, 1, "\r\n").unwrap();
        assert_eq!(empty.cursor, 3);
        assert!(empty.text.is_empty());
    }
    #[test]
    fn empty_buffers_and_line_ends_are_safe() {
        for text in ["", "\u{feff}", "\n", "\r\n"] {
            let d = doc(text);
            for motion in [
                Motion::Left,
                Motion::Right,
                Motion::Up,
                Motion::Down,
                Motion::LineStart,
                Motion::LineEnd,
                Motion::WordForward,
                Motion::WordBackward,
                Motion::WordEnd,
                Motion::FirstLine,
                Motion::LastLine,
            ] {
                let cursor = motion_target(&d, 0, motion, 1).unwrap();
                assert!(d.char_boundary(cursor));
                assert!(cursor >= content_start(&d));
            }
            assert!(delete_chars_range(&d, 0, 1).unwrap().is_empty());
        }
        let d = doc("\u{feff}");
        let plan = paste_plan(
            &d,
            3,
            &Register {
                text: "a\n".into(),
                linewise: true,
            },
            false,
            1,
            "\n",
        )
        .unwrap();
        assert_eq!(
            plan,
            InsertPlan {
                offset: 3,
                text: "a\n".into(),
                cursor: 3
            }
        );
    }
    #[test]
    fn bounded_work_refuses_long_word_and_large_payload() {
        let d = doc(&format!("{}\nz", "x".repeat(SCAN_LIMIT + 10)));
        assert_eq!(
            motion_target(&d, 0, Motion::WordForward, 1),
            Err(VimError::ScanLimit)
        );
        assert_eq!(
            motion_target(&d, SCAN_LIMIT + 5, Motion::Down, 1),
            Err(VimError::ScanLimit)
        );
        // Indexed $ and line operations do not scan the long line.
        assert_eq!(
            motion_target(&d, 0, Motion::LineEnd, 1).unwrap(),
            SCAN_LIMIT + 9
        );
        assert!(operator_range(&d, 0, Operator::Delete, Motion::Line, 1).is_ok());
        assert_eq!(
            motion_target(&d, 0, Motion::Right, MAX_COUNT + 1),
            Err(VimError::CountLimit)
        );
        assert_eq!(
            paste_plan(
                &d,
                0,
                &Register {
                    text: "x".repeat(EDIT_LIMIT / 2 + 1),
                    linewise: false
                },
                false,
                2,
                "\n"
            ),
            Err(VimError::EditLimit)
        );
    }
    #[test]
    fn arbitrary_offsets_never_split_utf8_crlf_or_bom() {
        for text in ["\u{feff}α💚\r\n\né! β\r\n", "\u{feff}", "a\r\nb", "\r\n"] {
            let d = doc(text);
            let valid = |offset: usize| {
                assert!(offset >= content_start(&d) && offset <= d.len());
                assert!(d.char_boundary(offset));
                assert!(
                    offset == 0 || offset == d.len() || d.read(offset - 1..offset + 1) != b"\r\n"
                );
            };
            for cursor in 0..=d.len() + 1 {
                for motion in [
                    Motion::Left,
                    Motion::Right,
                    Motion::Up,
                    Motion::Down,
                    Motion::LineStart,
                    Motion::FirstNonBlank,
                    Motion::LineEnd,
                    Motion::WordForward,
                    Motion::WordBackward,
                    Motion::WordEnd,
                    Motion::FirstLine,
                    Motion::LastLine,
                    Motion::Line,
                ] {
                    for count in [1, 2, 5] {
                        valid(motion_target(&d, cursor, motion, count).unwrap());
                        for operator in [Operator::Delete, Operator::Change, Operator::Yank] {
                            let plan = operator_range(&d, cursor, operator, motion, count).unwrap();
                            for range in [plan.range, plan.register_range] {
                                assert!(range.start <= range.end);
                                valid(range.start);
                                valid(range.end);
                                assert!(std::str::from_utf8(&d.read(range)).is_ok());
                            }
                        }
                    }
                }
            }
        }
    }
}
