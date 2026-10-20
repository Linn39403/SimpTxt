//! Bounded, checkpointed lexical highlighting, independent of the UI.
//!
//! Supported languages are Rust, C/C++ and JSON, plus explicit plain text.
//! Checkpoints carry the complete comment/string state rather than guessing
//! from the first visible line. A revision mismatch, unavailable checkpoint,
//! oversized viewport, or unsupported C/C++ preprocessing construct returns
//! an error: callers must display readable plain text in that case.

use crate::document::Document;
use std::fmt;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const CHECKPOINT_STRIDE: usize = 4096;
const SCAN_BLOCK: usize = 64 * 1024;
// Rust permits at most 255 raw-string hashes; C++ raw delimiters at most 16.
const LOOKAHEAD: usize = 512;
pub const MAX_HIGHLIGHT_BYTES: usize = 128 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Language {
    #[default]
    Plain,
    Rust,
    C,
    Json,
}

impl Language {
    pub fn from_path(path: &Path) -> Self {
        match path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "rs" => Self::Rust,
            "c" | "h" | "cc" | "hh" | "cpp" | "hpp" | "cxx" | "hxx" | "ino" => Self::C,
            "json" | "jsonl" => Self::Json,
            _ => Self::Plain,
        }
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Plain => "Plain text",
            Self::Rust => "Rust",
            Self::C => "C / C++",
            Self::Json => "JSON",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenKind {
    Comment,
    String,
    Number,
    Keyword,
}

pub type HighlightSpan = (Range<usize>, TokenKind);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Normal,
    Identifier(Option<TokenKind>),
    RawIdentifier {
        skip: u16,
    },
    Number(NumberState),
    LineComment,
    BlockComment {
        depth: u32,
        skip: u16,
    },
    Quoted {
        quote: u8,
        escaped: bool,
        skip: u16,
    },
    RustRaw {
        hashes: u16,
        skip: u16,
    },
    CRaw {
        delimiter: [u8; 16],
        length: u8,
        skip: u16,
    },
    Closing {
        remaining: u16,
        kind: TokenKind,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct State {
    mode: Mode,
}

impl Default for State {
    fn default() -> Self {
        Self { mode: Mode::Normal }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NumberState {
    initial_zero: bool,
    radix: bool,
    dot: bool,
    exponent: bool,
    sign_allowed: bool,
    suffix: bool,
}

impl NumberState {
    fn new(byte: u8) -> Self {
        Self {
            initial_zero: byte == b'0',
            radix: false,
            dot: false,
            exponent: false,
            sign_allowed: false,
            suffix: false,
        }
    }

    fn advance(&mut self, byte: u8, next: Option<u8>, language: Language) -> bool {
        if byte.is_ascii_digit() {
            self.initial_zero = false;
            self.sign_allowed = false;
            return true;
        }
        if language == Language::Json {
            if byte == b'.' && !self.dot && !self.exponent {
                self.dot = true;
                return true;
            }
            if matches!(byte, b'e' | b'E') && !self.exponent {
                self.exponent = true;
                self.sign_allowed = true;
                return true;
            }
            if matches!(byte, b'+' | b'-') && self.sign_allowed {
                self.sign_allowed = false;
                return true;
            }
            return false;
        }
        if self.initial_zero && matches!(byte, b'x' | b'X' | b'o' | b'b' | b'B') {
            self.radix = true;
            self.initial_zero = false;
            return true;
        }
        self.initial_zero = false;
        if byte == b'_'
            || (language == Language::C
                && byte == b'\''
                && next.is_some_and(|b| b.is_ascii_hexdigit()))
        {
            self.sign_allowed = false;
            return true;
        }
        if matches!(byte, b'+' | b'-') && self.sign_allowed {
            self.sign_allowed = false;
            return true;
        }
        if is_ident_start(byte) {
            if !self.suffix
                && ((!self.radix && matches!(byte, b'e' | b'E'))
                    || (language == Language::C && self.radix && matches!(byte, b'p' | b'P')))
            {
                self.exponent = true;
                self.sign_allowed = true;
            } else {
                self.sign_allowed = false;
                if !(self.radix && byte.is_ascii_hexdigit()) {
                    self.suffix = true;
                }
            }
            return true;
        }
        if byte == b'.'
            && !self.dot
            && !self.suffix
            && !self.exponent
            && (!self.radix || language == Language::C)
            && next != Some(b'.')
            && !next.is_some_and(is_ident_start)
        {
            self.dot = true;
            self.sign_allowed = false;
            return true;
        }
        false
    }
}

#[derive(Clone, Copy, Debug)]
struct Checkpoint {
    offset: usize,
    state: State,
}

/// Immutable, cheaply cloneable lexical checkpoints for one document revision.
#[derive(Clone, Debug)]
pub struct HighlightIndex {
    language: Language,
    revision: u64,
    document_len: usize,
    indexed_until: usize,
    checkpoints: Arc<[Checkpoint]>,
    scanned_bytes: usize,
}

impl HighlightIndex {
    /// Build on a cheap immutable document snapshot in a background worker.
    pub fn build(doc: &Document, language: Language, cancel: &AtomicBool) -> Result<Self, String> {
        Self::build_prefix(doc, language, doc.len(), cancel)
    }

    /// Make an initial prefix ready without scanning the whole file.
    pub fn build_prefix(
        doc: &Document,
        language: Language,
        through: usize,
        cancel: &AtomicBool,
    ) -> Result<Self, String> {
        Self::refresh(doc, language, None, through, cancel)
    }

    /// Reuse a previous snapshot's checkpoints, relexing only the invalidated
    /// region until complete lexer state converges in an unchanged suffix.
    /// `through` bounds new scanning, not reuse: convergence may make the entire
    /// old indexed suffix ready. Missing change history safely rebuilds from 0.
    /// Work and allocation occur on the worker, never on the rendering thread.
    pub fn refresh(
        doc: &Document,
        language: Language,
        previous: Option<&Self>,
        through: usize,
        cancel: &AtomicBool,
    ) -> Result<Self, String> {
        check_cancel(cancel)?;
        let through = through.min(doc.len());
        let previous = previous.filter(|old| old.language == language);
        if let Some(old) = previous
            && old.is_current(doc)
            && old.indexed_until >= through
        {
            let mut reused = old.clone();
            reused.scanned_bytes = 0;
            return Ok(reused);
        }
        let mut index = Self {
            language,
            revision: doc.revision(),
            document_len: doc.len(),
            indexed_until: if language == Language::Plain {
                doc.len()
            } else {
                through
            },
            checkpoints: Arc::from([]),
            scanned_bytes: 0,
        };
        if language == Language::Plain {
            return Ok(index);
        }
        let mut checkpoints = Vec::new();
        checkpoints
            .try_reserve_exact(through / CHECKPOINT_STRIDE + 2)
            .map_err(|_| "Not enough memory for syntax checkpoints; using plain text.")?;
        let mut state = State::default();
        let mut offset = 0;
        // A candidate is in the byte-identical suffix of the previous revision.
        // State equality alone is not enough without this unchanged-byte proof.
        let mut reuse: Option<(&Self, usize, usize)> = None;
        if let Some(old) = previous {
            if old.is_current(doc) {
                copy_checkpoints(&mut checkpoints, &old.checkpoints, 0, cancel)?;
                if let Some(last) = checkpoints.last() {
                    offset = last.offset;
                    state = last.state;
                }
            } else if let Some((prefix, old_suffix, new_suffix)) = unchanged_regions(old, doc) {
                // Lexing uses bounded lookahead (raw delimiters, keywords,
                // character literals and C preprocessing). Rewind far enough
                // that no reused state could have observed any edited byte.
                let restart = prefix.saturating_sub(LOOKAHEAD).min(through);
                let keep = old.checkpoints.partition_point(|cp| cp.offset <= restart);
                copy_checkpoints(&mut checkpoints, &old.checkpoints[..keep], 0, cancel)?;
                if let Some(last) = checkpoints.last() {
                    offset = last.offset;
                    state = last.state;
                }
                reuse = Some((old, old_suffix, new_suffix));
            }
        }
        if checkpoints.is_empty() {
            checkpoints.push(Checkpoint { offset: 0, state });
        }
        let mut candidate = reuse
            .map(|(old, old_suffix, _)| {
                old.checkpoints.partition_point(|cp| cp.offset < old_suffix)
            })
            .unwrap_or(0);
        while offset < through {
            check_cancel(cancel)?;
            let end = offset.saturating_add(SCAN_BLOCK).min(through);
            let read_end = end.saturating_add(LOOKAHEAD).min(doc.len());
            let bytes = doc.read(offset..read_end);
            for local in 0..end - offset {
                let absolute = offset + local;
                if absolute % CHECKPOINT_STRIDE == 0 {
                    check_cancel(cancel)?;
                }
                if let Some((old, old_suffix, new_suffix)) = reuse {
                    while let Some(cp) = old.checkpoints.get(candidate) {
                        let mapped = new_suffix + (cp.offset - old_suffix);
                        if mapped > absolute {
                            break;
                        }
                        if mapped == absolute && cp.state == state {
                            // Preserve shifted checkpoints instead of forcing
                            // alignment to byte 0 after every insertion/deletion.
                            if checkpoints
                                .last()
                                .is_some_and(|last| last.offset == absolute)
                            {
                                checkpoints.pop();
                            }
                            let delta = new_suffix as isize - old_suffix as isize;
                            copy_checkpoints(
                                &mut checkpoints,
                                &old.checkpoints[candidate..],
                                delta,
                                cancel,
                            )?;
                            index.indexed_until = new_suffix + (old.indexed_until - old_suffix);
                            index.scanned_bytes += local;
                            index.checkpoints = checkpoints.into();
                            check_cancel(cancel)?;
                            // A partial old index may end before the requested
                            // target; extend it without rescanning its prefix.
                            if index.indexed_until < through {
                                let scanned = index.scanned_bytes;
                                let mut extended =
                                    Self::refresh(doc, language, Some(&index), through, cancel)?;
                                extended.scanned_bytes += scanned;
                                return Ok(extended);
                            }
                            return Ok(index);
                        }
                        candidate += 1;
                    }
                }
                if absolute - checkpoints.last().unwrap().offset >= CHECKPOINT_STRIDE {
                    checkpoints.push(Checkpoint {
                        offset: absolute,
                        state,
                    });
                }
                state.step(&bytes[local..], language)?;
            }
            index.scanned_bytes += end - offset;
            offset = end;
        }
        if checkpoints.last().is_none_or(|cp| cp.offset != through) {
            checkpoints.push(Checkpoint {
                offset: through,
                state,
            });
        }
        check_cancel(cancel)?;
        index.checkpoints = checkpoints.into();
        Ok(index)
    }

    pub fn language(&self) -> Language {
        self.language
    }

    pub fn indexed_until(&self) -> usize {
        self.indexed_until
    }

    /// Bytes actually lexed by the most recent construction/refresh. Useful for
    /// checking that edit cost scales with invalidation, not document size.
    pub fn scanned_bytes(&self) -> usize {
        self.scanned_bytes
    }

    pub fn is_current(&self, doc: &Document) -> bool {
        self.revision == doc.revision() && self.document_len == doc.len()
    }

    fn checkpoint(&self, offset: usize) -> Option<&Checkpoint> {
        self.checkpoints
            .partition_point(|cp| cp.offset <= offset)
            .checked_sub(1)
            .and_then(|at| self.checkpoints.get(at))
    }

    /// Colored ranges relative to `range.start`. Bounded by one checkpoint
    /// interval plus 128 KiB of viewport and 512 bytes of lookahead. Stale or
    /// unavailable state is an error, never a guessed lexical state.
    pub fn spans(&self, doc: &Document, range: Range<usize>) -> Result<Vec<HighlightSpan>, String> {
        if !self.is_current(doc) {
            return Err("Syntax is being refreshed after an edit; using plain text.".into());
        }
        if range.start > range.end || range.end > doc.len() {
            return Err("Syntax viewport is outside the document; using plain text.".into());
        }
        if self.language == Language::Plain || range.is_empty() {
            return Ok(Vec::new());
        }
        if range.len() > MAX_HIGHLIGHT_BYTES {
            return Err("Syntax viewport exceeds the safe work budget; using plain text.".into());
        }
        if range.end > self.indexed_until {
            return Err("Correct syntax state is not ready here; using plain text.".into());
        }
        let checkpoint = self
            .checkpoint(range.start)
            .ok_or("Correct syntax state is not ready here; using plain text.")?;
        let mut state = checkpoint.state;
        let read_end = range.end.saturating_add(LOOKAHEAD).min(doc.len());
        let bytes = doc.read(checkpoint.offset..read_end);
        let mut spans: Vec<HighlightSpan> = Vec::new();
        let mut active: Option<(usize, TokenKind)> = None;
        for local in 0..range.end - checkpoint.offset {
            let absolute = checkpoint.offset + local;
            let kind = state.step(&bytes[local..], self.language)?;
            if absolute < range.start {
                continue;
            }
            let relative = absolute - range.start;
            if active.map(|(_, current)| current) != kind {
                if let Some((start, current)) = active.take() {
                    spans.push((start..relative, current));
                }
                if let Some(current) = kind {
                    active = Some((relative, current));
                }
            }
        }
        if let Some((start, kind)) = active {
            spans.push((start..range.len(), kind));
        }
        Ok(spans)
    }

    /// Batch sorted, non-overlapping row windows. Nearby rows are lexed once;
    /// very long physical-line gaps are skipped using checkpoints. The total
    /// lexical work, including checkpoint prefixes, is capped at 128 KiB.
    pub fn spans_batch(
        &self,
        doc: &Document,
        ranges: &[Range<usize>],
    ) -> Result<Vec<Vec<HighlightSpan>>, String> {
        if ranges.len() > 1024 || !self.is_current(doc) {
            return Err(
                "Syntax viewport is stale or exceeds the safe work budget; using plain text."
                    .into(),
            );
        }
        let mut groups: Vec<(Range<usize>, Range<usize>)> = Vec::new();
        for (row, range) in ranges.iter().enumerate() {
            if range.start > range.end
                || range.end > doc.len()
                || (row > 0 && ranges[row - 1].end > range.start)
            {
                return Err("Syntax row windows must be ordered and within the document.".into());
            }
            if let Some((span, rows)) = groups.last_mut()
                && range.start.saturating_sub(span.end) <= CHECKPOINT_STRIDE
            {
                span.end = range.end;
                rows.end = row + 1;
            } else {
                groups.push((range.clone(), row..row + 1));
            }
        }
        let mut budget = 0usize;
        for (range, _) in &groups {
            if range.is_empty() || self.language == Language::Plain {
                continue;
            }
            let cp = self
                .checkpoint(range.start)
                .ok_or("Correct syntax state is not ready here; using plain text.")?;
            budget = budget.saturating_add(range.end - cp.offset);
            if budget > MAX_HIGHLIGHT_BYTES {
                return Err(
                    "Syntax viewport exceeds the safe work budget; using plain text.".into(),
                );
            }
        }
        let mut result = vec![Vec::new(); ranges.len()];
        for (range, rows) in groups {
            let spans = self.spans(doc, range.clone())?;
            for row in rows {
                let window = ranges[row].start - range.start..ranges[row].end - range.start;
                let first = spans.partition_point(|(span, _)| span.end <= window.start);
                for (span, kind) in &spans[first..] {
                    if span.start >= window.end {
                        break;
                    }
                    let start = span.start.max(window.start) - window.start;
                    let end = span.end.min(window.end) - window.start;
                    if start < end {
                        result[row].push((start..end, *kind));
                    }
                }
            }
        }
        Ok(result)
    }
}

fn copy_checkpoints(
    output: &mut Vec<Checkpoint>,
    input: &[Checkpoint],
    delta: isize,
    cancel: &AtomicBool,
) -> Result<(), String> {
    output
        .try_reserve(input.len())
        .map_err(|_| "Not enough memory for syntax checkpoints; using plain text.")?;
    for chunk in input.chunks(1024) {
        check_cancel(cancel)?;
        for cp in chunk {
            let cp = Checkpoint {
                offset: cp
                    .offset
                    .checked_add_signed(delta)
                    .expect("validated unchanged suffix offset"),
                state: cp.state,
            };
            // Repeated shifted edits must not accumulate densely packed
            // checkpoints. Removing the middle of a short three-point run
            // keeps every gap <= STRIDE and the count <= 2*len/STRIDE + 2.
            if output.len() >= 2 && cp.offset - output[output.len() - 2].offset <= CHECKPOINT_STRIDE
            {
                output.pop();
            }
            output.push(cp);
        }
    }
    Ok(())
}

/// Returns the unchanged prefix and old/new starts of the identical suffix.
/// A bounded transition log is evidence, not a content hash or revision guess.
fn unchanged_regions(old: &HighlightIndex, doc: &Document) -> Option<(usize, usize, usize)> {
    let changes = doc.changes_since(old.revision)?;
    let mut revision = old.revision;
    let mut length = old.document_len;
    let mut prefix = length;
    let mut suffix_len = length;
    for change in changes {
        if change.before_revision != revision
            || change.changed_from > change.old_end
            || change.old_end > length
            || change.changed_from > change.new_end
        {
            return None;
        }
        prefix = prefix.min(change.changed_from);
        suffix_len = suffix_len.min(length - change.old_end);
        length = length
            .checked_sub(change.old_end - change.changed_from)?
            .checked_add(change.new_end - change.changed_from)?;
        revision = change.after_revision;
    }
    if revision != doc.revision() || length != doc.len() {
        return None;
    }
    Some((prefix, old.document_len - suffix_len, length - suffix_len))
}

/// One viewport cache, keyed by exact document revision, immutable index and
/// row windows. A repaint never repeats lexical work for an unchanged view.
#[derive(Default)]
pub struct ViewportCache {
    index: Option<HighlightIndex>,
    ranges: Vec<Range<usize>>,
    rows: Vec<Vec<HighlightSpan>>,
}

impl ViewportCache {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn prepare(
        &mut self,
        doc: &Document,
        index: Option<&HighlightIndex>,
        ranges: &[Range<usize>],
    ) {
        let Some(index) = index.filter(|index| index.is_current(doc)) else {
            self.clear();
            return;
        };
        if self.index.as_ref().is_some_and(|cached| {
            cached.is_current(doc)
                && cached.language == index.language
                && Arc::ptr_eq(&cached.checkpoints, &index.checkpoints)
        }) && self.ranges == ranges
        {
            return;
        }
        self.rows = index.spans_batch(doc, ranges).unwrap_or_default();
        self.ranges = if ranges.len() <= 1024 {
            ranges.to_vec()
        } else {
            Vec::new()
        };
        self.index = Some(index.clone());
    }

    pub fn row(&self, row: usize) -> &[HighlightSpan] {
        self.rows.get(row).map(Vec::as_slice).unwrap_or_default()
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("Syntax highlighting cancelled; using plain text.".into())
    } else {
        Ok(())
    }
}

impl State {
    fn step(&mut self, text: &[u8], language: Language) -> Result<Option<TokenKind>, String> {
        let byte = text[0];
        let next = text.get(1).copied();
        if language == Language::C {
            // Translation-phase line splicing can manufacture comment/string
            // delimiters across physical lines. Do not misrepresent it.
            if text.starts_with(b"\\\n") || text.starts_with(b"\\\r\n") {
                return Err(
                    "C/C++ line splicing needs a preprocessing-aware lexer; using plain text."
                        .into(),
                );
            }
            // Old C/C++ dialects translate trigraphs before tokenization.
            if text.starts_with(b"??") && text.get(2).is_some_and(|b| b"=/'()!<>-".contains(b)) {
                return Err(
                    "C/C++ legacy trigraphs need a preprocessing-aware lexer; using plain text."
                        .into(),
                );
            }
        }
        loop {
            match self.mode {
                Mode::Normal => return self.start(text, language),
                Mode::Identifier(kind) => {
                    if is_ident_continue(byte) {
                        return Ok(kind);
                    }
                    self.mode = Mode::Normal;
                }
                Mode::RawIdentifier { skip } => {
                    if skip > 0 {
                        self.mode = Mode::RawIdentifier { skip: skip - 1 };
                        return Ok(None);
                    }
                    self.mode = Mode::Identifier(None);
                }
                Mode::Number(mut number) => {
                    if number.advance(byte, next, language) {
                        self.mode = Mode::Number(number);
                        return Ok(Some(TokenKind::Number));
                    }
                    self.mode = Mode::Normal;
                }
                Mode::LineComment => {
                    if matches!(byte, b'\r' | b'\n') {
                        self.mode = Mode::Normal;
                        return Ok(None);
                    }
                    return Ok(Some(TokenKind::Comment));
                }
                Mode::BlockComment { depth, skip } => {
                    if skip > 0 {
                        self.mode = Mode::BlockComment {
                            depth,
                            skip: skip - 1,
                        };
                    } else if byte == b'/' && next == Some(b'*') && language == Language::Rust {
                        let depth = depth
                            .checked_add(1)
                            .ok_or("Comment nesting exceeds syntax limits; using plain text.")?;
                        self.mode = Mode::BlockComment { depth, skip: 1 };
                    } else if byte == b'*' && next == Some(b'/') {
                        self.mode = if depth == 1 {
                            Mode::Closing {
                                remaining: 1,
                                kind: TokenKind::Comment,
                            }
                        } else {
                            Mode::BlockComment {
                                depth: depth - 1,
                                skip: 1,
                            }
                        };
                    }
                    return Ok(Some(TokenKind::Comment));
                }
                Mode::Quoted {
                    quote,
                    escaped,
                    skip,
                } => {
                    self.mode = if skip > 0 {
                        Mode::Quoted {
                            quote,
                            escaped,
                            skip: skip - 1,
                        }
                    } else if escaped {
                        Mode::Quoted {
                            quote,
                            escaped: false,
                            skip: 0,
                        }
                    } else if byte == b'\\' {
                        Mode::Quoted {
                            quote,
                            escaped: true,
                            skip: 0,
                        }
                    } else if byte == quote {
                        Mode::Normal
                    } else {
                        self.mode
                    };
                    return Ok(Some(TokenKind::String));
                }
                Mode::RustRaw { hashes, skip } => {
                    if skip > 0 {
                        self.mode = Mode::RustRaw {
                            hashes,
                            skip: skip - 1,
                        };
                    } else if byte == b'"'
                        && text
                            .get(1..1 + hashes as usize)
                            .is_some_and(|s| s.iter().all(|&b| b == b'#'))
                    {
                        self.mode = if hashes == 0 {
                            Mode::Normal
                        } else {
                            Mode::Closing {
                                remaining: hashes,
                                kind: TokenKind::String,
                            }
                        };
                    }
                    return Ok(Some(TokenKind::String));
                }
                Mode::CRaw {
                    delimiter,
                    length,
                    skip,
                } => {
                    if skip > 0 {
                        self.mode = Mode::CRaw {
                            delimiter,
                            length,
                            skip: skip - 1,
                        };
                    } else if byte == b')'
                        && text.get(1..1 + length as usize) == Some(&delimiter[..length as usize])
                        && text.get(1 + length as usize) == Some(&b'"')
                    {
                        self.mode = Mode::Closing {
                            remaining: length as u16 + 1,
                            kind: TokenKind::String,
                        };
                    }
                    return Ok(Some(TokenKind::String));
                }
                Mode::Closing { remaining, kind } => {
                    self.mode = if remaining <= 1 {
                        Mode::Normal
                    } else {
                        Mode::Closing {
                            remaining: remaining - 1,
                            kind,
                        }
                    };
                    return Ok(Some(kind));
                }
            }
        }
    }

    fn start(&mut self, text: &[u8], language: Language) -> Result<Option<TokenKind>, String> {
        let byte = text[0];
        let next = text.get(1).copied();
        if language != Language::Json {
            if text.starts_with(b"//") {
                self.mode = Mode::LineComment;
                return Ok(Some(TokenKind::Comment));
            }
            if text.starts_with(b"/*") {
                self.mode = Mode::BlockComment { depth: 1, skip: 1 };
                return Ok(Some(TokenKind::Comment));
            }
        }
        if language == Language::Rust {
            if let Some((prefix, hashes)) = rust_raw_start(text) {
                self.mode = Mode::RustRaw {
                    hashes,
                    skip: prefix - 1,
                };
                return Ok(Some(TokenKind::String));
            }
            if text.starts_with(b"b\"") || text.starts_with(b"c\"") {
                self.mode = Mode::Quoted {
                    quote: b'"',
                    escaped: false,
                    skip: 1,
                };
                return Ok(Some(TokenKind::String));
            }
            if text.starts_with(b"b'") && rust_char_start(&text[1..]) {
                self.mode = Mode::Quoted {
                    quote: b'\'',
                    escaped: false,
                    skip: 1,
                };
                return Ok(Some(TokenKind::String));
            }
            if text.starts_with(b"r#") && text.get(2).copied().is_some_and(is_ident_start) {
                // A raw identifier is not a keyword, including its name.
                self.mode = Mode::RawIdentifier { skip: 1 };
                return Ok(None);
            }
        }
        if language == Language::C {
            if let Some((prefix, delimiter, length)) = c_raw_start(text)? {
                self.mode = Mode::CRaw {
                    delimiter,
                    length,
                    skip: prefix - 1,
                };
                return Ok(Some(TokenKind::String));
            }
            for prefix in [
                b"u8\"".as_slice(),
                b"u\"",
                b"U\"",
                b"L\"",
                b"u8'",
                b"u'",
                b"U'",
                b"L'",
            ] {
                if text.starts_with(prefix) {
                    self.mode = Mode::Quoted {
                        quote: *prefix.last().unwrap(),
                        escaped: false,
                        skip: prefix.len() as u16 - 1,
                    };
                    return Ok(Some(TokenKind::String));
                }
            }
        }
        if byte == b'"'
            || (byte == b'\''
                && (language == Language::C
                    || (language == Language::Rust && rust_char_start(text))))
        {
            self.mode = Mode::Quoted {
                quote: byte,
                escaped: false,
                skip: 0,
            };
            return Ok(Some(TokenKind::String));
        }
        if language == Language::Rust && byte == b'\'' && next.is_some_and(is_ident_start) {
            // Rust lifetimes and loop labels are identifiers, not character
            // literals, and `'static` must not become a `static` keyword.
            self.mode = Mode::Identifier(None);
            return Ok(None);
        }
        if byte.is_ascii_digit()
            || (language == Language::Json
                && byte == b'-'
                && next.is_some_and(|b| b.is_ascii_digit()))
        {
            self.mode = Mode::Number(NumberState::new(byte));
            return Ok(Some(TokenKind::Number));
        }
        if is_ident_start(byte) {
            let kind = if starts_keyword(text, language) {
                Some(TokenKind::Keyword)
            } else {
                None
            };
            self.mode = Mode::Identifier(kind);
            return Ok(kind);
        }
        Ok(None)
    }
}

fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80
}

fn is_ident_continue(byte: u8) -> bool {
    is_ident_start(byte) || byte.is_ascii_digit()
}

fn rust_char_start(text: &[u8]) -> bool {
    if text.first() != Some(&b'\'') {
        return false;
    }
    let Some(&first) = text.get(1) else {
        return false;
    };
    if first == b'\\' {
        return true;
    }
    let width = match first {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return false,
    };
    text.get(1 + width) == Some(&b'\'')
}

fn rust_raw_start(text: &[u8]) -> Option<(u16, u16)> {
    let mut index = if text.starts_with(b"br") || text.starts_with(b"cr") {
        2
    } else if text.starts_with(b"r") {
        1
    } else {
        return None;
    };
    let mut hashes = 0;
    while text.get(index) == Some(&b'#') && hashes <= 255 {
        hashes += 1;
        index += 1;
    }
    if hashes <= 255 && text.get(index) == Some(&b'"') {
        Some((index as u16 + 1, hashes))
    } else {
        None
    }
}

type CRawStart = (u16, [u8; 16], u8);

fn c_raw_start(text: &[u8]) -> Result<Option<CRawStart>, String> {
    let prefixes: &[&[u8]] = &[b"R\"", b"u8R\"", b"uR\"", b"UR\"", b"LR\""];
    let Some(prefix) = prefixes.iter().find(|prefix| text.starts_with(prefix)) else {
        return Ok(None);
    };
    let mut delimiter = [0; 16];
    for length in 0..=16 {
        let Some(&byte) = text.get(prefix.len() + length) else {
            return Err("Incomplete C++ raw-string delimiter; using plain text.".into());
        };
        if byte == b'(' {
            return Ok(Some((
                (prefix.len() + length + 1) as u16,
                delimiter,
                length as u8,
            )));
        }
        if length == 16 || !(0x21..=0x7e).contains(&byte) || matches!(byte, b')' | b'\\') {
            return Err("Unsupported C++ raw-string delimiter; using plain text.".into());
        }
        delimiter[length] = byte;
    }
    unreachable!()
}

fn starts_keyword(text: &[u8], language: Language) -> bool {
    let keywords: &[&[u8]] = match language {
        Language::Rust => &[
            b"as",
            b"async",
            b"await",
            b"break",
            b"const",
            b"continue",
            b"crate",
            b"dyn",
            b"else",
            b"enum",
            b"extern",
            b"false",
            b"fn",
            b"for",
            b"if",
            b"impl",
            b"in",
            b"let",
            b"loop",
            b"match",
            b"mod",
            b"move",
            b"mut",
            b"pub",
            b"ref",
            b"return",
            b"self",
            b"Self",
            b"static",
            b"struct",
            b"super",
            b"trait",
            b"true",
            b"type",
            b"unsafe",
            b"use",
            b"where",
            b"while",
            b"abstract",
            b"become",
            b"box",
            b"do",
            b"final",
            b"macro",
            b"override",
            b"priv",
            b"typeof",
            b"unsized",
            b"virtual",
            b"yield",
            b"try",
            b"gen",
        ],
        Language::C => &[
            b"alignas",
            b"alignof",
            b"and",
            b"and_eq",
            b"asm",
            b"auto",
            b"bitand",
            b"bitor",
            b"bool",
            b"break",
            b"case",
            b"catch",
            b"char",
            b"char8_t",
            b"char16_t",
            b"char32_t",
            b"class",
            b"co_await",
            b"co_return",
            b"co_yield",
            b"compl",
            b"concept",
            b"const",
            b"consteval",
            b"constexpr",
            b"constinit",
            b"const_cast",
            b"continue",
            b"decltype",
            b"default",
            b"delete",
            b"do",
            b"double",
            b"dynamic_cast",
            b"else",
            b"enum",
            b"explicit",
            b"export",
            b"extern",
            b"false",
            b"float",
            b"for",
            b"friend",
            b"goto",
            b"if",
            b"inline",
            b"int",
            b"long",
            b"mutable",
            b"namespace",
            b"new",
            b"noexcept",
            b"not",
            b"not_eq",
            b"nullptr",
            b"operator",
            b"or",
            b"or_eq",
            b"private",
            b"protected",
            b"public",
            b"register",
            b"reinterpret_cast",
            b"requires",
            b"restrict",
            b"return",
            b"short",
            b"signed",
            b"sizeof",
            b"static",
            b"static_assert",
            b"static_cast",
            b"struct",
            b"switch",
            b"template",
            b"this",
            b"thread_local",
            b"throw",
            b"true",
            b"try",
            b"typedef",
            b"typeid",
            b"typename",
            b"union",
            b"unsigned",
            b"using",
            b"virtual",
            b"void",
            b"volatile",
            b"wchar_t",
            b"while",
            b"xor",
            b"xor_eq",
            b"_Alignas",
            b"_Alignof",
            b"_Atomic",
            b"_Bool",
            b"_Complex",
            b"_Generic",
            b"_Imaginary",
            b"_Noreturn",
            b"_Static_assert",
            b"_Thread_local",
        ],
        Language::Json => &[b"true", b"false", b"null"],
        Language::Plain => &[],
    };
    keywords.iter().any(|keyword| {
        text.starts_with(keyword)
            && !text
                .get(keyword.len())
                .copied()
                .is_some_and(is_ident_continue)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(text: &str) -> Document {
        Document::from_bytes(text.as_bytes().to_vec()).unwrap()
    }

    fn colors(text: &str, language: Language) -> Vec<Option<TokenKind>> {
        let doc = document(text);
        let index = HighlightIndex::build(&doc, language, &AtomicBool::new(false)).unwrap();
        let spans = index.spans(&doc, 0..doc.len()).unwrap();
        let mut result = vec![None; text.len()];
        for (range, kind) in spans {
            result[range].fill(Some(kind));
        }
        result
    }

    fn assert_token(text: &str, needle: &str, language: Language, kind: TokenKind) {
        let at = text.find(needle).unwrap();
        let colors = colors(text, language);
        assert!(
            colors[at..at + needle.len()]
                .iter()
                .all(|&value| value == Some(kind))
        );
    }

    #[test]
    fn rust_keywords_comments_strings_numbers() {
        let text = "let value = 12.5e-2; // a comment\nlet s = \"escaped \\\"quote\\\"\";";
        assert_token(text, "let", Language::Rust, TokenKind::Keyword);
        assert_token(text, "12.5e-2", Language::Rust, TokenKind::Number);
        assert_token(text, "// a comment", Language::Rust, TokenKind::Comment);
        assert_token(
            text,
            "\"escaped \\\"quote\\\"\"",
            Language::Rust,
            TokenKind::String,
        );
    }

    #[test]
    fn nested_rust_comments_and_raw_strings() {
        let text = "/* outer /* inner */ still outer */ let s = br##\"quote \"# // body\nend\"##;";
        assert_token(
            text,
            "/* outer /* inner */ still outer */",
            Language::Rust,
            TokenKind::Comment,
        );
        assert_token(
            text,
            "br##\"quote \"# // body\nend\"##",
            Language::Rust,
            TokenKind::String,
        );
        assert_token(text, "let", Language::Rust, TokenKind::Keyword);
    }

    #[test]
    fn rust_characters_lifetimes_and_raw_identifiers() {
        let text = "fn f<'static>(x: &'static str) { let r#type = 'é'; let c = b'\\''; }";
        let values = colors(text, Language::Rust);
        let lifetime = text.find("'static").unwrap();
        assert!(values[lifetime..lifetime + 7].iter().all(Option::is_none));
        let raw_ident = text.find("r#type").unwrap();
        assert!(values[raw_ident..raw_ident + 6].iter().all(Option::is_none));
        assert_token(text, "'é'", Language::Rust, TokenKind::String);
        assert_token(text, "b'\\''", Language::Rust, TokenKind::String);
    }

    #[test]
    fn c_comments_do_not_nest_and_raw_strings_are_multiline() {
        let text = "/* outer /* inner */ int x; auto s = u8R\"tag(a \" // b\n)no\" c)tag\"; char c = '\\'';";
        assert_token(
            text,
            "/* outer /* inner */",
            Language::C,
            TokenKind::Comment,
        );
        assert_token(text, "int", Language::C, TokenKind::Keyword);
        assert_token(
            text,
            "u8R\"tag(a \" // b\n)no\" c)tag\"",
            Language::C,
            TokenKind::String,
        );
        assert_token(text, "'\\''", Language::C, TokenKind::String);
    }

    #[test]
    fn json_has_no_comment_syntax() {
        let text = "{\"url\":\"https://x/*y*/\", \"ok\": true, \"n\": -1.25e+3, \"x\": null}";
        assert_token(
            text,
            "\"https://x/*y*/\"",
            Language::Json,
            TokenKind::String,
        );
        assert_token(text, "true", Language::Json, TokenKind::Keyword);
        assert_token(text, "-1.25e+3", Language::Json, TokenKind::Number);
        assert_token(text, "null", Language::Json, TokenKind::Keyword);
        assert!(!colors("// no comment", Language::Json).contains(&Some(TokenKind::Comment)));
    }

    #[test]
    fn state_crosses_checkpoints_and_scan_blocks() {
        let mut text = " ".repeat(SCAN_BLOCK - 2);
        text.push_str("/* comment");
        text.push_str(&"x".repeat(CHECKPOINT_STRIDE * 3));
        text.push_str(" */ let x = 1;");
        let doc = document(&text);
        let index = HighlightIndex::build(&doc, Language::Rust, &AtomicBool::new(false)).unwrap();
        let start = SCAN_BLOCK + CHECKPOINT_STRIDE;
        assert_eq!(
            index.spans(&doc, start..start + 100).unwrap(),
            vec![(0..100, TokenKind::Comment)]
        );
        let keyword = text.rfind("let").unwrap();
        assert_eq!(
            index.spans(&doc, keyword..keyword + 3).unwrap(),
            vec![(0..3, TokenKind::Keyword)]
        );
    }

    #[test]
    fn raw_delimiter_and_escape_can_cross_checkpoint() {
        for language in [Language::Rust, Language::C] {
            let (open, close) = if language == Language::Rust {
                ("r###\"", "\"###")
            } else {
                ("R\"tag(", ")tag\"")
            };
            let mut text = " ".repeat(CHECKPOINT_STRIDE - 2);
            text.push_str(open);
            text.push_str(&"x".repeat(CHECKPOINT_STRIDE * 2));
            text.push_str(close);
            text.push_str(" let int ");
            let doc = document(&text);
            let index = HighlightIndex::build(&doc, language, &AtomicBool::new(false)).unwrap();
            let start = CHECKPOINT_STRIDE * 2;
            assert_eq!(
                index.spans(&doc, start..start + 20).unwrap(),
                vec![(0..20, TokenKind::String)]
            );
        }
    }

    #[test]
    fn arbitrary_viewports_match_full_document_highlighting() {
        let cases = [
            (
                Language::Rust,
                "fn sample<'a>() { let r#type = br###\"// \"## not closed\"###; /* outer /* inner */ end */ let c = '\\''; }\n",
            ),
            (
                Language::C,
                "int sample() { auto s = R\"tag(/* // \" not closed )ta)tag\"; /* comment */ char c = '\\''; return 1; }\n",
            ),
            (
                Language::Json,
                "{\"key\":\"escaped \\\\\\\" quote\", \"enabled\":true, \"number\":-2.5e+3}\n",
            ),
        ];
        for (language, unit) in cases {
            let text = unit.repeat(100);
            let expected = colors(&text, language);
            let doc = document(&text);
            let index = HighlightIndex::build(&doc, language, &AtomicBool::new(false)).unwrap();
            // Exercise starting within every token near checkpoints, not only
            // at convenient line or delimiter boundaries.
            for anchor in [0, CHECKPOINT_STRIDE, CHECKPOINT_STRIDE * 2] {
                for delta in 0..unit.len() {
                    let start = anchor + delta;
                    if start >= text.len() {
                        continue;
                    }
                    let end = (start + 317).min(text.len());
                    let mut actual = vec![None; end - start];
                    for (range, kind) in index.spans(&doc, start..end).unwrap() {
                        actual[range].fill(Some(kind));
                    }
                    assert_eq!(actual, expected[start..end], "{language}, offset {start}");
                }
            }
        }
    }

    #[test]
    fn maximum_rust_raw_delimiter_crosses_read_block() {
        let hashes = "#".repeat(255);
        let mut text = " ".repeat(SCAN_BLOCK - 3);
        text.push_str(&format!("r{hashes}\"inside // body\"{hashes}; let x = 1;"));
        let doc = document(&text);
        let index = HighlightIndex::build(&doc, Language::Rust, &AtomicBool::new(false)).unwrap();
        let start = text.find("inside").unwrap();
        assert_eq!(
            index.spans(&doc, start..start + 14).unwrap(),
            vec![(0..14, TokenKind::String)]
        );
        let keyword = text.find("let").unwrap();
        assert_eq!(
            index.spans(&doc, keyword..keyword + 3).unwrap(),
            vec![(0..3, TokenKind::Keyword)]
        );
    }

    #[test]
    fn escaped_quote_across_checkpoint_does_not_end_string() {
        let mut text = String::from("\"");
        text.push_str(&"x".repeat(CHECKPOINT_STRIDE - 2));
        text.push_str("\\\"still inside\"; let x = 1;");
        let doc = document(&text);
        let index = HighlightIndex::build(&doc, Language::Rust, &AtomicBool::new(false)).unwrap();
        let start = CHECKPOINT_STRIDE;
        assert_eq!(
            index.spans(&doc, start..start + 12).unwrap(),
            vec![(0..12, TokenKind::String)]
        );
    }

    #[test]
    fn stale_partial_cancelled_and_oversized_indexes_fall_back() {
        let mut doc = document(&format!("/* {} */", "x".repeat(MAX_HIGHLIGHT_BYTES + 8)));
        let index =
            HighlightIndex::build_prefix(&doc, Language::Rust, 100, &AtomicBool::new(false))
                .unwrap();
        assert!(index.spans(&doc, 0..20).is_ok());
        assert!(index.spans(&doc, 100..120).is_err());
        assert!(index.spans(&doc, 0..MAX_HIGHLIGHT_BYTES + 1).is_err());
        doc.replace(0..2, "  ").unwrap();
        assert!(index.spans(&doc, 0..20).is_err());
        assert!(HighlightIndex::build(&doc, Language::Rust, &AtomicBool::new(true)).is_err());
    }

    #[test]
    fn c_preprocessor_hazards_explicitly_fall_back() {
        for text in [
            "// comment \\\ncontinued\nint x;",
            "/\\\r\n* comment */",
            "??/ comment",
            "R\"incomplete",
        ] {
            let doc = document(text);
            assert!(HighlightIndex::build(&doc, Language::C, &AtomicBool::new(false)).is_err());
        }
    }

    #[test]
    fn c_line_splice_across_scan_block_is_detected() {
        let text = format!("{}\\\nint x;", " ".repeat(SCAN_BLOCK - 1));
        let doc = document(&text);
        assert!(HighlightIndex::build(&doc, Language::C, &AtomicBool::new(false)).is_err());
    }

    fn assert_equivalent(doc: &Document, incremental: &HighlightIndex, language: Language) {
        let fresh = HighlightIndex::build(doc, language, &AtomicBool::new(false)).unwrap();
        assert_eq!(incremental.indexed_until(), doc.len());
        for start in (0..doc.len()).step_by(997) {
            let end = (start + 1777).min(doc.len());
            assert_eq!(
                incremental.spans(doc, start..end).unwrap(),
                fresh.spans(doc, start..end).unwrap(),
                "{language}, window {start}..{end}"
            );
        }
    }

    #[test]
    fn middle_edit_reuses_prefix_and_shifted_suffix() {
        let mut doc = document(&"fn main() { let value = 42; } // comment\n".repeat(30_000));
        let cancel = AtomicBool::new(false);
        let old = HighlightIndex::build(&doc, Language::Rust, &cancel).unwrap();
        let at = doc.line_start(doc.line_count() / 2) + 20;
        doc.replace(at..at, "changed").unwrap();
        let index =
            HighlightIndex::refresh(&doc, Language::Rust, Some(&old), doc.len(), &cancel).unwrap();
        assert!(
            index.scanned_bytes() <= 3 * CHECKPOINT_STRIDE,
            "scanned {}",
            index.scanned_bytes()
        );
        assert_eq!(index.indexed_until(), doc.len());
        assert!(!old.is_current(&doc));
        let fresh = HighlightIndex::build(&doc, Language::Rust, &cancel).unwrap();
        for start in [0, at - CHECKPOINT_STRIDE, at, doc.len() - 1000] {
            let end = (start + 777).min(doc.len());
            assert_eq!(
                index.spans(&doc, start..end).unwrap(),
                fresh.spans(&doc, start..end).unwrap()
            );
        }
        // A shifted suffix is itself a valid base for subsequent edits.
        doc.replace(at..at + 7, "x").unwrap();
        let next = HighlightIndex::refresh(&doc, Language::Rust, Some(&index), doc.len(), &cancel)
            .unwrap();
        assert!(next.scanned_bytes() <= 3 * CHECKPOINT_STRIDE);
    }

    #[test]
    fn skipped_edits_undo_redo_and_snapshot_branches_are_safe() {
        let mut doc = document(&"let x = r##\"hello\"##; /* comment */\r\n".repeat(400));
        let cancel = AtomicBool::new(false);
        let base = HighlightIndex::build(&doc, Language::Rust, &cancel).unwrap();
        let stale = doc.snapshot();
        doc.replace(8000..8000, "/*").unwrap();
        doc.replace(8100..8103, "*/let").unwrap();
        doc.replace(5000..5000, "é😀\r\n").unwrap();
        assert!(doc.undo());
        assert!(doc.redo());
        let index =
            HighlightIndex::refresh(&doc, Language::Rust, Some(&base), doc.len(), &cancel).unwrap();
        assert_equivalent(&doc, &index, Language::Rust);
        assert!(index.spans(&stale, 0..10).is_err());
        let mut branch = stale.snapshot();
        branch.replace(0..3, "pub").unwrap();
        let branch_index =
            HighlightIndex::refresh(&branch, Language::Rust, Some(&index), branch.len(), &cancel)
                .unwrap();
        assert_equivalent(&branch, &branch_index, Language::Rust);
        assert!(branch_index.spans(&doc, 0..10).is_err());
    }

    #[test]
    fn deterministic_random_edits_match_full_lexing() {
        let cases = [
            (
                Language::Rust,
                "fn f<'a>() { let é = br##\"a \"# z\"##; /* a /* b */ c */ let x = '\\''; }\r\n",
            ),
            (
                Language::C,
                "int f() { auto s = R\"tag(a /* z */ )tag\"; char x = '\\''; return 2; }\r\n",
            ),
            (
                Language::Json,
                "{\"é\":\"a\\\"b\", \"x\":-2.5e+3, \"yes\":true}\r\n",
            ),
        ];
        for (language, unit) in cases {
            let mut doc = document(&unit.repeat(180));
            let cancel = AtomicBool::new(false);
            let mut index = Some(HighlightIndex::build(&doc, language, &cancel).unwrap());
            let edits = [
                "/*", "*/", "\"", "#", "é😀", "\r\n", "\\", "let", "", "R\"tag(", ")tag\"", "//",
            ];
            let mut random = 0x5eed_1234_u64;
            for turn in 0..120 {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                let mut at = (random as usize) % (doc.len() + 1);
                while !doc.char_boundary(at) {
                    at -= 1;
                }
                let mut end = at
                    .saturating_add((random >> 32) as usize % 7)
                    .min(doc.len());
                while !doc.char_boundary(end) {
                    end -= 1;
                }
                doc.replace(at..end, edits[turn % edits.len()]).unwrap();
                if turn % 3 != 2 {
                    continue;
                }
                let fresh = HighlightIndex::build(&doc, language, &cancel);
                let updated =
                    HighlightIndex::refresh(&doc, language, index.as_ref(), doc.len(), &cancel);
                match (fresh, updated) {
                    (Ok(fresh), Ok(updated)) => {
                        assert_eq!(
                            updated.spans(&doc, 0..doc.len()).unwrap(),
                            fresh.spans(&doc, 0..doc.len()).unwrap(),
                            "{language}, random edit {turn}"
                        );
                        index = Some(updated);
                    }
                    (Err(_), Err(_)) => {
                        index = None;
                    }
                    (fresh, updated) => panic!(
                        "{language}, random edit {turn}: fresh {fresh:?}, incremental {updated:?}"
                    ),
                }
            }
        }
    }

    #[test]
    fn lookahead_edits_at_checkpoints_relex_delimiters_and_utf8() {
        let cancel = AtomicBool::new(false);
        for language in [Language::Rust, Language::C] {
            for delta in [0, 1, 2, 15, 255] {
                let opening = if language == Language::Rust {
                    format!("r{}\"", "#".repeat(255))
                } else {
                    "R\"abcdefghijklmnop(".to_owned()
                };
                let closing = if language == Language::Rust {
                    format!("\"{}", "#".repeat(255))
                } else {
                    ")abcdefghijklmnop\"".to_owned()
                };
                let mut text = " ".repeat(CHECKPOINT_STRIDE - delta);
                text.push_str(&opening);
                text.push_str(&"é😀\r\n".repeat(1000));
                text.push_str(&closing);
                text.push_str("; let int x = 2;");
                let mut doc = document(&text);
                let old = HighlightIndex::build(&doc, language, &cancel).unwrap();
                let at = CHECKPOINT_STRIDE - delta;
                doc.replace(at..at + 1, " ").unwrap();
                let updated =
                    HighlightIndex::refresh(&doc, language, Some(&old), doc.len(), &cancel)
                        .unwrap();
                assert_equivalent(&doc, &updated, language);
            }
        }
    }

    #[test]
    fn edits_inside_lookahead_after_checkpoint_invalidate_earlier_raw_state() {
        let cancel = AtomicBool::new(false);
        for language in [Language::Rust, Language::C] {
            let text = if language == Language::Rust {
                format!(
                    "{}r{}\"{}\"{}; let x = 1;",
                    " ".repeat(CHECKPOINT_STRIDE - 2),
                    "#".repeat(255),
                    "é\r\n".repeat(3000),
                    "#".repeat(255)
                )
            } else {
                format!(
                    "{}R\"abcdefghijklmnop({})abcdefghijklmnop\"; int x = 1;",
                    " ".repeat(CHECKPOINT_STRIDE - 2),
                    "é\r\n".repeat(3000)
                )
            };
            let mut doc = document(&text);
            let old = HighlightIndex::build(&doc, language, &cancel).unwrap();
            let at = CHECKPOINT_STRIDE + if language == Language::Rust { 100 } else { 10 };
            doc.replace(
                at..at + 1,
                if language == Language::Rust { "" } else { "Z" },
            )
            .unwrap();
            let next =
                HighlightIndex::refresh(&doc, language, Some(&old), doc.len(), &cancel).unwrap();
            assert_equivalent(&doc, &next, language);
        }
    }

    #[test]
    fn repeated_shifted_edits_keep_checkpoint_count_bounded() {
        let mut doc = document(&"let value = 123;\n".repeat(2000));
        let cancel = AtomicBool::new(false);
        let mut index = HighlightIndex::build(&doc, Language::Rust, &cancel).unwrap();
        for turn in 0..200 {
            let at = 1000 + (turn * 101) % (doc.len() - 1000);
            doc.replace(at..at, " ").unwrap();
            index = HighlightIndex::refresh(&doc, Language::Rust, Some(&index), doc.len(), &cancel)
                .unwrap();
            assert!(index.checkpoints.len() <= 2 * doc.len() / CHECKPOINT_STRIDE + 2);
            assert!(
                index
                    .checkpoints
                    .windows(2)
                    .all(|pair| pair[1].offset - pair[0].offset <= CHECKPOINT_STRIDE)
            );
        }
        assert_equivalent(&doc, &index, Language::Rust);
    }

    #[test]
    fn partial_refresh_extends_without_relexing_prefix_and_preserves_fallbacks() {
        let mut doc = document(&"/* comment */ let x = 1;\r\n".repeat(2000));
        let cancel = AtomicBool::new(false);
        let prefix = HighlightIndex::build_prefix(&doc, Language::Rust, 7777, &cancel).unwrap();
        let full = HighlightIndex::refresh(&doc, Language::Rust, Some(&prefix), doc.len(), &cancel)
            .unwrap();
        assert_eq!(full.scanned_bytes(), doc.len() - 7777);
        assert_equivalent(&doc, &full, Language::Rust);
        doc.replace(100..100, "/*").unwrap();
        let partial =
            HighlightIndex::refresh(&doc, Language::Rust, Some(&full), 2000, &cancel).unwrap();
        assert!(partial.indexed_until() < doc.len());
        assert!(partial.spans(&doc, 2100..2200).is_err());
        assert!(
            HighlightIndex::refresh(
                &doc,
                Language::Rust,
                Some(&full),
                doc.len(),
                &AtomicBool::new(true)
            )
            .is_err()
        );
        for _ in 0..70 {
            doc.replace(10..10, " ").unwrap();
        }
        let rebuilt =
            HighlightIndex::refresh(&doc, Language::Rust, Some(&full), doc.len(), &cancel).unwrap();
        assert_eq!(rebuilt.scanned_bytes(), doc.len());
        assert_equivalent(&doc, &rebuilt, Language::Rust);
    }

    #[test]
    fn incremental_c_hazards_are_not_hidden_by_suffix_reuse() {
        let cancel = AtomicBool::new(false);
        for hazard in ["\\\r\n", "??/", "R\"bad delimiter("] {
            let mut doc = document(&"int x; ".repeat(1000));
            let old = HighlightIndex::build(&doc, Language::C, &cancel).unwrap();
            let at = CHECKPOINT_STRIDE - 1;
            doc.replace(at..at, hazard).unwrap();
            assert!(HighlightIndex::build(&doc, Language::C, &cancel).is_err());
            assert!(
                HighlightIndex::refresh(&doc, Language::C, Some(&old), doc.len(), &cancel).is_err()
            );
        }
    }

    #[test]
    fn batch_cache_matches_rows_and_bounds_huge_line_gaps() {
        let mut doc = document(&"let é = 1; /* comment */\r\n".repeat(100));
        let cancel = AtomicBool::new(false);
        let index = HighlightIndex::build(&doc, Language::Rust, &cancel).unwrap();
        let ranges: Vec<_> = (10..50)
            .map(|line| doc.line_start(line)..doc.line_start(line + 1))
            .collect();
        let mut cache = ViewportCache::default();
        cache.prepare(&doc, Some(&index), &ranges);
        for (row, range) in ranges.iter().enumerate() {
            assert_eq!(cache.row(row), index.spans(&doc, range.clone()).unwrap());
        }
        let allocation = cache.rows.as_ptr();
        cache.prepare(&doc, Some(&index), &ranges);
        assert_eq!(allocation, cache.rows.as_ptr());
        doc.replace(0..0, " ").unwrap();
        cache.prepare(&doc, Some(&index), &ranges);
        assert!(cache.row(0).is_empty());

        let doc = document(&format!(
            "/*{}*/\nlet x = 42;",
            "x".repeat(MAX_HIGHLIGHT_BYTES * 4)
        ));
        let index = HighlightIndex::build(&doc, Language::Rust, &cancel).unwrap();
        let ranges = [1000..1100, doc.len() - 11..doc.len()];
        let batch = index.spans_batch(&doc, &ranges).unwrap();
        for (range, result) in ranges.iter().zip(batch) {
            assert_eq!(result, index.spans(&doc, range.clone()).unwrap());
        }
        assert!(
            index
                .spans_batch(&doc, std::slice::from_ref(&(0..MAX_HIGHLIGHT_BYTES + 1)))
                .is_err()
        );
        assert!(index.spans_batch(&doc, &[100..200, 0..10]).is_err());
        assert!(index.spans_batch(&doc, &vec![0..0; 1025]).is_err());
    }

    #[test]
    fn recognizes_extensions_without_claiming_unknown_languages() {
        assert_eq!(Language::from_path(Path::new("main.cpp")), Language::C);
        assert_eq!(Language::from_path(Path::new("firmware.ino")), Language::C);
        assert_eq!(Language::from_path(Path::new("main.RS")), Language::Rust);
        assert_eq!(Language::from_path(Path::new("data.json")), Language::Json);
        assert_eq!(Language::from_path(Path::new("main.py")), Language::Plain);
    }
}
