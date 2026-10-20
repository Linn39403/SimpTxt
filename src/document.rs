//! UTF-8 piece-table storage. Small edits share unchanged chunks, with bounded
//! local coalescing. Highly fragmented bulk output streams into fresh chunks.
//!
//! File loading should use `from_chunks_cancellable`: the convenience `from_bytes`
//! constructor necessarily starts with an already contiguous input allocation.

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use memchr::memchr_iter;

pub const CHUNK_SIZE: usize = 256 * 1024;
pub const HISTORY_LIMIT: usize = 1_000;
/// Prototype resource ceiling, leaving room for edits to a 1 GB source file.
/// This bounds logical document size, not total memory including undo history.
pub const MAX_DOCUMENT_BYTES: usize = 2 * 1024 * 1024 * 1024;
const NEWLINE_STRIDE: usize = 128;
const BULK_COMPACT_PIECES: usize = 16 * 1024;
const LOCAL_COMPACT_BYTES: usize = 64 * 1024;
const LOCAL_COMPACT_PIECES: usize = 32;
const FOREGROUND_PIECE_LIMIT: usize = 64 * 1024;

static NEXT_REVISION: AtomicU64 = AtomicU64::new(1);

fn fresh_revision() -> u64 {
    NEXT_REVISION.fetch_add(1, Ordering::Relaxed)
}

fn checked_document_len(current: usize, removed: usize, inserted: usize) -> Result<usize, String> {
    let len = current
        .checked_sub(removed)
        .and_then(|len| len.checked_add(inserted))
        .ok_or_else(|| "Document size calculation overflowed".to_owned())?;
    if len > MAX_DOCUMENT_BYTES {
        return Err(
            "Document would exceed the 2 GiB prototype size limit; the document was not changed"
                .to_owned(),
        );
    }
    Ok(len)
}

#[derive(Clone, Debug)]
struct Piece {
    bytes: Arc<[u8]>,
    range: Range<usize>,
    newlines: usize,
    /// Absolute backing-buffer offsets of newline 0, 128, 256, ...
    newline_checkpoints: Arc<[usize]>,
    /// Ordinal of this piece's first newline in the shared checkpoint index.
    first_newline: usize,
}

impl Piece {
    fn new(bytes: Arc<[u8]>, range: Range<usize>) -> Self {
        let mut checkpoints = Vec::new();
        let mut newlines = 0;
        for offset in memchr_iter(b'\n', &bytes[range.clone()]) {
            if newlines % NEWLINE_STRIDE == 0 {
                checkpoints.push(range.start + offset);
            }
            newlines += 1;
        }
        Self {
            bytes,
            range,
            newlines,
            newline_checkpoints: Arc::from(checkpoints),
            first_newline: 0,
        }
    }

    fn len(&self) -> usize {
        self.range.end - self.range.start
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes[self.range.clone()]
    }

    fn slice(&self, range: Range<usize>) -> Self {
        if range.start == 0 && range.end == self.len() {
            return self.clone();
        }
        let first_newline =
            self.first_newline + memchr_iter(b'\n', &self.as_slice()[..range.start]).count();
        let newlines = memchr_iter(b'\n', &self.as_slice()[range.clone()]).count();
        Self {
            bytes: self.bytes.clone(),
            range: self.range.start + range.start..self.range.start + range.end,
            newlines,
            newline_checkpoints: self.newline_checkpoints.clone(),
            first_newline,
        }
    }

    fn newline_offset(&self, ordinal: usize) -> usize {
        debug_assert!(ordinal < self.newlines);
        let ordinal = self.first_newline + ordinal;
        let checkpoint = self.newline_checkpoints[ordinal / NEWLINE_STRIDE];
        let relative = memchr_iter(b'\n', &self.bytes[checkpoint..self.range.end])
            .nth(ordinal % NEWLINE_STRIDE)
            .expect("shared newline checkpoints match the immutable bytes");
        checkpoint + relative - self.range.start
    }
}

#[derive(Debug)]
struct Content {
    pieces: Vec<Piece>,
    /// Exclusive cumulative byte offsets, one per piece.
    byte_ends: Vec<usize>,
    /// Cumulative LF counts, one per piece. These may contain duplicates.
    newline_ends: Vec<usize>,
    len: usize,
    newlines: usize,
}

impl Content {
    fn from_pieces(pieces: Vec<Piece>) -> Self {
        let mut byte_ends = Vec::with_capacity(pieces.len());
        let mut newline_ends = Vec::with_capacity(pieces.len());
        let mut len = 0;
        let mut newlines = 0;
        for piece in &pieces {
            debug_assert!(!piece.as_slice().is_empty());
            len += piece.len();
            newlines += piece.newlines;
            byte_ends.push(len);
            newline_ends.push(newlines);
        }
        Self {
            pieces,
            byte_ends,
            newline_ends,
            len,
            newlines,
        }
    }

    fn from_pieces_cancellable(pieces: Vec<Piece>, cancel: &AtomicBool) -> Result<Self, String> {
        let mut byte_ends = Vec::new();
        let mut newline_ends = Vec::new();
        byte_ends
            .try_reserve_exact(pieces.len())
            .map_err(|_| "Not enough memory for the byte index".to_owned())?;
        newline_ends
            .try_reserve_exact(pieces.len())
            .map_err(|_| "Not enough memory for the line index".to_owned())?;
        let mut len = 0usize;
        let mut newlines = 0usize;
        for (index, piece) in pieces.iter().enumerate() {
            if index % 1024 == 0 && cancel.load(Ordering::Relaxed) {
                return Err("Operation cancelled".to_owned());
            }
            len = len
                .checked_add(piece.len())
                .ok_or_else(|| "Document is too large".to_owned())?;
            newlines += piece.newlines;
            byte_ends.push(len);
            newline_ends.push(newlines);
        }
        Ok(Self {
            pieces,
            byte_ends,
            newline_ends,
            len,
            newlines,
        })
    }

    fn piece_at(&self, offset: usize) -> usize {
        self.byte_ends.partition_point(|end| *end <= offset)
    }

    fn piece_start(&self, index: usize) -> usize {
        if index == 0 {
            0
        } else {
            self.byte_ends[index - 1]
        }
    }

    fn append_range(&self, range: Range<usize>, output: &mut Vec<Piece>) {
        if range.is_empty() {
            return;
        }
        let mut index = self.piece_at(range.start);
        let mut offset = range.start;
        while offset < range.end {
            let piece = &self.pieces[index];
            let start = offset - self.piece_start(index);
            let end = piece.len().min(range.end - self.piece_start(index));
            push_piece(output, piece.slice(start..end));
            offset += end - start;
            index += 1;
        }
    }

    fn append_range_cancellable(
        &self,
        range: Range<usize>,
        output: &mut Vec<Piece>,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let mut index = self.piece_at(range.start);
        let mut offset = range.start;
        while offset < range.end {
            if cancel.load(Ordering::Relaxed) {
                return Err("Replacement cancelled".to_owned());
            }
            let piece = &self.pieces[index];
            let start = offset - self.piece_start(index);
            let end = piece.len().min(range.end - self.piece_start(index));
            push_piece(output, piece.slice(start..end));
            offset += end - start;
            index += 1;
        }
        Ok(())
    }
}

/// Emits valid UTF-8 chunks directly, without an intermediate per-match piece
/// list or contiguous whole-document byte buffer.
struct CompactBuilder<'a> {
    pieces: Vec<Piece>,
    buffer: Vec<u8>,
    cancel: &'a AtomicBool,
}

impl<'a> CompactBuilder<'a> {
    fn new(output_len: usize, cancel: &'a AtomicBool) -> Result<Self, String> {
        let mut pieces = Vec::new();
        pieces
            .try_reserve_exact(output_len.div_ceil(CHUNK_SIZE - 3))
            .map_err(|_| "Not enough memory for replacement chunks".to_owned())?;
        Ok(Self {
            pieces,
            buffer: Vec::new(),
            cancel,
        })
    }

    fn write(&mut self, mut bytes: &[u8]) -> Result<(), String> {
        while !bytes.is_empty() {
            if self.cancel.load(Ordering::Relaxed) {
                return Err("Replacement cancelled".to_owned());
            }
            if self.buffer.capacity() < CHUNK_SIZE {
                self.buffer
                    .try_reserve_exact(CHUNK_SIZE - self.buffer.len())
                    .map_err(|_| "Not enough memory for a replacement chunk".to_owned())?;
            }
            let mut take = (CHUNK_SIZE - self.buffer.len()).min(bytes.len());
            if take < bytes.len() {
                while take > 0 && bytes[take] & 0b1100_0000 == 0b1000_0000 {
                    take -= 1;
                }
            }
            if take == 0 {
                self.flush()?;
                continue;
            }
            self.buffer.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buffer.len() == CHUNK_SIZE {
                self.flush()?;
            }
        }
        Ok(())
    }

    fn copy_range(&mut self, content: &Content, range: Range<usize>) -> Result<(), String> {
        let mut index = content.piece_at(range.start);
        let mut offset = range.start;
        while offset < range.end {
            let piece = &content.pieces[index];
            let start = offset - content.piece_start(index);
            let end = piece.len().min(range.end - content.piece_start(index));
            self.write(&piece.as_slice()[start..end])?;
            offset += end - start;
            index += 1;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), String> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err("Replacement cancelled".to_owned());
        }
        if !self.buffer.is_empty() {
            self.pieces
                .try_reserve(1)
                .map_err(|_| "Not enough memory for replacement chunks".to_owned())?;
            let bytes: Arc<[u8]> = Arc::from(std::mem::take(&mut self.buffer));
            let len = bytes.len();
            self.pieces.push(Piece::new(bytes, 0..len));
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<Piece>, String> {
        self.flush()?;
        Ok(self.pieces)
    }
}

fn push_piece(output: &mut Vec<Piece>, piece: Piece) {
    if piece.len() == 0 {
        return;
    }
    if let Some(previous) = output.last_mut()
        && Arc::ptr_eq(&previous.bytes, &piece.bytes)
        && Arc::ptr_eq(&previous.newline_checkpoints, &piece.newline_checkpoints)
        && previous.first_newline + previous.newlines == piece.first_newline
        && previous.range.end == piece.range.start
        && previous.len() + piece.len() <= CHUNK_SIZE
    {
        previous.range.end = piece.range.end;
        previous.newlines += piece.newlines;
        return;
    }
    output.push(piece);
}

/// Repeated typing should not leave one allocation and piece per keystroke.
/// Coalesce only a small neighborhood and only when enough fragments accumulate.
/// Undo records logical splices, so changing these internal boundaries is safe.
fn coalesce_near(pieces: &mut Vec<Piece>, center: usize) {
    if pieces.len() < LOCAL_COMPACT_PIECES {
        return;
    }
    let center = center.min(pieces.len() - 1);
    if pieces[center].len() > LOCAL_COMPACT_BYTES {
        return;
    }
    let mut start = center;
    let mut end = center + 1;
    let mut len = pieces[center].len();
    while start > 0 && pieces[start - 1].len() <= LOCAL_COMPACT_BYTES - len {
        start -= 1;
        len += pieces[start].len();
    }
    while end < pieces.len() && pieces[end].len() <= LOCAL_COMPACT_BYTES - len {
        len += pieces[end].len();
        end += 1;
    }
    if end - start < LOCAL_COMPACT_PIECES {
        return;
    }
    let mut bytes = Vec::new();
    // Coalescing is optional. Allocation failure must not lose a valid edit.
    if bytes.try_reserve_exact(len).is_err() {
        return;
    }
    for piece in &pieces[start..end] {
        bytes.extend_from_slice(piece.as_slice());
    }
    let bytes: Arc<[u8]> = Arc::from(bytes);
    pieces.splice(start..end, [Piece::new(bytes, 0..len)]);
}

#[derive(Debug)]
enum Change {
    Splice {
        offset: usize,
        removed: Vec<Piece>,
        inserted: Vec<Piece>,
    },
    /// Bulk transactions retain shared metadata and chunks, not contiguous text.
    Snapshot {
        before: Arc<Content>,
        after: Arc<Content>,
    },
}

#[derive(Debug)]
struct Edit {
    change: Change,
    before_revision: u64,
    after_revision: u64,
}

#[derive(Debug)]
pub struct Document {
    content: Arc<Content>,
    undo: VecDeque<Edit>,
    redo: Vec<Edit>,
    revision: u64,
    saved_revision: u64,
}

/// Cloning is a cheap, independent snapshot. Undo/redo history is deliberately
/// not copied into background work or saving snapshots.
impl Clone for Document {
    fn clone(&self) -> Self {
        Self {
            content: self.content.clone(),
            undo: VecDeque::new(),
            redo: Vec::new(),
            revision: self.revision,
            saved_revision: self.saved_revision,
        }
    }
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    pub fn new() -> Self {
        Self::from_content(Content::from_pieces(Vec::new()))
    }

    fn from_content(content: Content) -> Self {
        let revision = fresh_revision();
        Self {
            content: Arc::new(content),
            undo: VecDeque::new(),
            redo: Vec::new(),
            revision,
            saved_revision: revision,
        }
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, String> {
        checked_document_len(0, 0, bytes.len())?;
        // Arbitrary input boundaries are repaired by the chunk constructor.
        let chunks = bytes.chunks(CHUNK_SIZE).map(Arc::<[u8]>::from).collect();
        Self::from_chunks(chunks)
    }

    pub fn from_chunks(chunks: Vec<Arc<[u8]>>) -> Result<Self, String> {
        Self::from_chunks_cancellable(chunks, &AtomicBool::new(false))
    }

    /// Validate and index bounded slices without flattening the input. Complete
    /// chunks stay shared. A codepoint crossing input boundaries is copied into
    /// a tiny separate chunk, leaving all other input bytes untouched.
    pub fn from_chunks_cancellable(
        chunks: Vec<Arc<[u8]>>,
        cancel: &AtomicBool,
    ) -> Result<Self, String> {
        let mut input_len = 0usize;
        for chunk in &chunks {
            if cancel.load(Ordering::Relaxed) {
                return Err("Loading cancelled".to_owned());
            }
            input_len = checked_document_len(input_len, 0, chunk.len())?;
        }
        let mut pieces = Vec::with_capacity(chunks.len());
        let mut pending = Vec::with_capacity(4);
        let mut pending_offset = 0;
        let mut absolute = 0usize;

        for chunk in chunks {
            let mut offset = 0;
            while offset < chunk.len() {
                if cancel.load(Ordering::Relaxed) {
                    return Err("Loading cancelled".to_owned());
                }
                if !pending.is_empty() {
                    let width = utf8_width(pending[0]);
                    let take = (width - pending.len()).min(chunk.len() - offset);
                    pending.extend_from_slice(&chunk[offset..offset + take]);
                    offset += take;
                    if pending.len() < width {
                        continue;
                    }
                    std::str::from_utf8(&pending)
                        .map_err(|_| format!("Invalid UTF-8 at byte {pending_offset}"))?;
                    let bytes: Arc<[u8]> = Arc::from(pending.as_slice());
                    let len = bytes.len();
                    push_piece(&mut pieces, Piece::new(bytes, 0..len));
                    pending.clear();
                    continue;
                }

                let end = (offset + CHUNK_SIZE).min(chunk.len());
                match std::str::from_utf8(&chunk[offset..end]) {
                    Ok(_) => {
                        push_piece(&mut pieces, Piece::new(chunk.clone(), offset..end));
                    }
                    Err(error) => {
                        let valid_end = offset + error.valid_up_to();
                        if error.error_len().is_some() {
                            return Err(format!("Invalid UTF-8 at byte {}", absolute + valid_end));
                        }
                        if valid_end > offset {
                            push_piece(&mut pieces, Piece::new(chunk.clone(), offset..valid_end));
                        }
                        pending_offset = absolute + valid_end;
                        pending.extend_from_slice(&chunk[valid_end..end]);
                    }
                }
                offset = end;
            }
            absolute = absolute
                .checked_add(chunk.len())
                .ok_or_else(|| "Document is too large".to_owned())?;
        }
        if cancel.load(Ordering::Relaxed) {
            return Err("Loading cancelled".to_owned());
        }
        if !pending.is_empty() {
            return Err(format!("Incomplete UTF-8 at byte {pending_offset}"));
        }
        let content = Content::from_pieces_cancellable(pieces, cancel)?;
        if cancel.load(Ordering::Relaxed) {
            return Err("Loading cancelled".to_owned());
        }
        Ok(Self::from_content(content))
    }

    pub fn len(&self) -> usize {
        self.content.len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A final LF starts an empty last line; an empty document has one line.
    pub fn line_count(&self) -> usize {
        self.content.newlines + 1
    }

    /// Zero-based line start. Out-of-range line numbers return EOF.
    pub fn line_start(&self, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        if line > self.content.newlines {
            return self.len();
        }
        let index = self
            .content
            .newline_ends
            .partition_point(|count| *count < line);
        let prior = if index == 0 {
            0
        } else {
            self.content.newline_ends[index - 1]
        };
        let within = self.content.pieces[index].newline_offset(line - prior - 1);
        self.content.piece_start(index) + within + 1
    }

    /// Return the zero-based line containing the byte offset, clamping at EOF.
    pub fn line_of_offset(&self, offset: usize) -> usize {
        let offset = offset.min(self.len());
        if offset == self.len() {
            return self.content.newlines;
        }
        let index = self.content.piece_at(offset);
        let prior = if index == 0 {
            0
        } else {
            self.content.newline_ends[index - 1]
        };
        let within = offset - self.content.piece_start(index);
        prior + memchr_iter(b'\n', &self.content.pieces[index].as_slice()[..within]).count()
    }

    /// Read only the requested bytes. Inverted or out-of-bounds ranges are
    /// clipped to an empty range or EOF rather than panicking.
    pub fn read(&self, range: Range<usize>) -> Vec<u8> {
        let start = range.start.min(self.len());
        let end = range.end.min(self.len()).max(start);
        let mut output = Vec::with_capacity(end - start);
        if start == end {
            return output;
        }
        let mut index = self.content.piece_at(start);
        let mut offset = start;
        while offset < end {
            let piece = &self.content.pieces[index];
            let within = offset - self.content.piece_start(index);
            let take = (piece.len() - within).min(end - offset);
            output.extend_from_slice(&piece.as_slice()[within..within + take]);
            offset += take;
            index += 1;
        }
        output
    }

    pub fn chunks(&self) -> impl Iterator<Item = &[u8]> {
        self.content.pieces.iter().map(Piece::as_slice)
    }

    pub fn char_boundary(&self, offset: usize) -> bool {
        if offset > self.len() {
            return false;
        }
        if offset == self.len() {
            return true;
        }
        let index = self.content.piece_at(offset);
        let byte = self.content.pieces[index].as_slice()[offset - self.content.piece_start(index)];
        byte & 0b1100_0000 != 0b1000_0000
    }

    pub fn previous_char(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.len());
        if offset == 0 {
            return 0;
        }
        offset -= 1;
        while !self.char_boundary(offset) {
            offset -= 1;
        }
        offset
    }

    pub fn next_char(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.len());
        if offset == self.len() {
            return offset;
        }
        offset += 1;
        while !self.char_boundary(offset) {
            offset += 1;
        }
        offset
    }

    fn validate_range(&self, range: &Range<usize>) -> Result<(), String> {
        if range.start > range.end || range.end > self.len() {
            return Err("Edit range is outside the document".to_owned());
        }
        if !self.char_boundary(range.start) || !self.char_boundary(range.end) {
            return Err("Edit range splits a UTF-8 character".to_owned());
        }
        Ok(())
    }

    fn range_equals(&self, range: &Range<usize>, bytes: &[u8]) -> bool {
        if range.end - range.start != bytes.len() {
            return false;
        }
        let mut offset = range.start;
        let mut index = self.content.piece_at(offset);
        let mut consumed = 0;
        while offset < range.end {
            let piece = &self.content.pieces[index];
            let within = offset - self.content.piece_start(index);
            let take = (piece.len() - within).min(range.end - offset);
            if piece.as_slice()[within..within + take] != bytes[consumed..consumed + take] {
                return false;
            }
            offset += take;
            consumed += take;
            index += 1;
        }
        true
    }

    fn range_equals_cancellable(
        &self,
        range: &Range<usize>,
        bytes: &[u8],
        cancel: &AtomicBool,
    ) -> Result<bool, String> {
        if range.end - range.start != bytes.len() {
            return Ok(false);
        }
        let mut offset = range.start;
        let mut index = self.content.piece_at(offset);
        let mut consumed = 0;
        while offset < range.end {
            if cancel.load(Ordering::Relaxed) {
                return Err("Replacement cancelled".to_owned());
            }
            let piece = &self.content.pieces[index];
            let within = offset - self.content.piece_start(index);
            let take = (piece.len() - within).min(range.end - offset);
            if piece.as_slice()[within..within + take] != bytes[consumed..consumed + take] {
                return Ok(false);
            }
            offset += take;
            consumed += take;
            index += 1;
        }
        Ok(true)
    }

    pub fn replace(&mut self, range: Range<usize>, text: &str) -> Result<(), String> {
        self.validate_range(&range)?;
        if self.range_equals(&range, text.as_bytes()) {
            return Ok(());
        }
        checked_document_len(self.len(), range.end - range.start, text.len())?;
        let inserted = text_pieces(text);
        let start_index = self.content.piece_at(range.start);
        let end_index = self.content.piece_at(range.end);
        let prefix = start_index + usize::from(range.start > self.content.piece_start(start_index));
        let suffix = self.content.pieces.len() - end_index;
        if prefix.saturating_add(inserted.len()).saturating_add(suffix) > FOREGROUND_PIECE_LIMIT {
            return Err(
                "Document is too fragmented for an interactive edit; save and reopen it first"
                    .to_owned(),
            );
        }
        let mut removed = Vec::new();
        self.content.append_range(range.clone(), &mut removed);
        self.splice(range.clone(), &inserted);
        self.record_change(Change::Splice {
            offset: range.start,
            removed,
            inserted,
        });
        Ok(())
    }

    /// Replace sorted, non-overlapping ranges, using original byte coordinates,
    /// in a single undoable transaction. Sparse edits share inserted bytes;
    /// densely fragmented output streams directly into bounded owned chunks.
    /// All validation and work happen before the current document changes.
    pub fn replace_ranges(&mut self, ranges: &[Range<usize>], text: &str) -> Result<(), String> {
        self.replace_ranges_cancellable(ranges, text, &AtomicBool::new(false))
    }

    pub fn replace_many(&mut self, ranges: &[Range<usize>], text: &str) -> Result<(), String> {
        self.replace_ranges(ranges, text)
    }

    pub fn replace_ranges_cancellable(
        &mut self,
        ranges: &[Range<usize>],
        text: &str,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let mut previous_end = 0;
        let mut removed_bytes = 0usize;
        let mut effective = Vec::new();
        effective
            .try_reserve_exact(ranges.len())
            .map_err(|_| "Not enough memory for replacement ranges".to_owned())?;
        for range in ranges {
            if cancel.load(Ordering::Relaxed) {
                return Err("Replacement cancelled".to_owned());
            }
            self.validate_range(range)?;
            if range.start < previous_end {
                return Err("Replacement ranges must be sorted and non-overlapping".to_owned());
            }
            previous_end = range.end;
            if !self.range_equals_cancellable(range, text.as_bytes(), cancel)? {
                removed_bytes = removed_bytes
                    .checked_add(range.end - range.start)
                    .ok_or_else(|| "Document size calculation overflowed".to_owned())?;
                effective.push(range);
            }
        }
        if effective.is_empty() {
            return Ok(());
        }
        let inserted_bytes = text
            .len()
            .checked_mul(effective.len())
            .ok_or_else(|| "Document size calculation overflowed".to_owned())?;
        let new_len = checked_document_len(self.len(), removed_bytes, inserted_bytes)?;
        let estimated = self.content.pieces.len().saturating_add(
            effective
                .len()
                .saturating_mul(text.len().div_ceil(CHUNK_SIZE - 3).saturating_add(1)),
        );
        let pieces = if estimated > BULK_COMPACT_PIECES {
            // Decide before generating pieces, not after creating millions of
            // metadata entries that would make the next keystroke expensive.
            let mut builder = CompactBuilder::new(new_len, cancel)?;
            let mut offset = 0;
            for range in effective {
                if cancel.load(Ordering::Relaxed) {
                    return Err("Replacement cancelled".to_owned());
                }
                builder.copy_range(&self.content, offset..range.start)?;
                builder.write(text.as_bytes())?;
                offset = range.end;
            }
            builder.copy_range(&self.content, offset..self.len())?;
            builder.finish()?
        } else {
            let inserted = text_pieces_cancellable(text, cancel)?;
            let mut pieces = Vec::new();
            pieces
                .try_reserve_exact(estimated)
                .map_err(|_| "Not enough memory for replacement".to_owned())?;
            let mut offset = 0;
            for range in effective {
                if cancel.load(Ordering::Relaxed) {
                    return Err("Replacement cancelled".to_owned());
                }
                self.content
                    .append_range_cancellable(offset..range.start, &mut pieces, cancel)?;
                for piece in &inserted {
                    push_piece(&mut pieces, piece.clone());
                }
                offset = range.end;
            }
            self.content
                .append_range_cancellable(offset..self.len(), &mut pieces, cancel)?;
            pieces
        };
        let after = Arc::new(Content::from_pieces_cancellable(pieces, cancel)?);
        if cancel.load(Ordering::Relaxed) {
            return Err("Replacement cancelled".to_owned());
        }
        debug_assert_eq!(after.len, new_len);
        let before = std::mem::replace(&mut self.content, after.clone());
        self.record_change(Change::Snapshot { before, after });
        Ok(())
    }

    fn splice(&mut self, range: Range<usize>, inserted: &[Piece]) {
        let mut pieces = Vec::with_capacity(self.content.pieces.len() + inserted.len() + 2);
        self.content.append_range(0..range.start, &mut pieces);
        let edit_piece = pieces.len();
        for piece in inserted {
            push_piece(&mut pieces, piece.clone());
        }
        self.content
            .append_range(range.end..self.len(), &mut pieces);
        coalesce_near(&mut pieces, edit_piece);
        self.content = Arc::new(Content::from_pieces(pieces));
    }

    fn record_change(&mut self, change: Change) {
        let before_revision = self.revision;
        self.revision = fresh_revision();
        self.undo.push_back(Edit {
            change,
            before_revision,
            after_revision: self.revision,
        });
        if self.undo.len() > HISTORY_LIMIT {
            self.undo.pop_front();
        }
        self.redo.clear();
    }

    pub fn undo(&mut self) -> bool {
        let Some(edit) = self.undo.pop_back() else {
            return false;
        };
        match &edit.change {
            Change::Splice {
                offset,
                removed,
                inserted,
            } => {
                let inserted_len: usize = inserted.iter().map(Piece::len).sum();
                self.splice(*offset..*offset + inserted_len, removed);
            }
            Change::Snapshot { before, .. } => self.content = before.clone(),
        }
        self.revision = edit.before_revision;
        self.redo.push(edit);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(edit) = self.redo.pop() else {
            return false;
        };
        match &edit.change {
            Change::Splice {
                offset,
                removed,
                inserted,
            } => {
                let removed_len: usize = removed.iter().map(Piece::len).sum();
                self.splice(*offset..*offset + removed_len, inserted);
            }
            Change::Snapshot { after, .. } => self.content = after.clone(),
        }
        self.revision = edit.after_revision;
        self.undo.push_back(edit);
        true
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn is_modified(&self) -> bool {
        self.revision != self.saved_revision
    }

    pub fn mark_saved(&mut self) {
        self.saved_revision = self.revision;
    }

    /// Mark the revision actually written by a background save, which may be
    /// older than the currently edited revision.
    pub fn mark_saved_revision(&mut self, revision: u64) {
        self.saved_revision = revision;
    }

    pub fn snapshot(&self) -> Self {
        self.clone()
    }

    /// Adopt a finished background edit as one undo action. The caller must
    /// first verify that the original revision has not changed during work.
    pub fn apply_snapshot(&mut self, snapshot: &Document) -> bool {
        if Arc::ptr_eq(&self.content, &snapshot.content) {
            return false;
        }
        let after = snapshot.content.clone();
        let before = std::mem::replace(&mut self.content, after.clone());
        self.record_change(Change::Snapshot { before, after });
        true
    }
}

fn utf8_width(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => unreachable!("only an incomplete valid UTF-8 prefix is carried"),
    }
}

fn text_pieces(text: &str) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + CHUNK_SIZE).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let bytes: Arc<[u8]> = Arc::from(&text.as_bytes()[start..end]);
        let len = bytes.len();
        pieces.push(Piece::new(bytes, 0..len));
        start = end;
    }
    pieces
}

fn text_pieces_cancellable(text: &str, cancel: &AtomicBool) -> Result<Vec<Piece>, String> {
    let mut pieces = Vec::new();
    pieces
        .try_reserve_exact(text.len().div_ceil(CHUNK_SIZE - 3))
        .map_err(|_| "Not enough memory for inserted text".to_owned())?;
    let mut start = 0;
    while start < text.len() {
        if cancel.load(Ordering::Relaxed) {
            return Err("Replacement cancelled".to_owned());
        }
        let mut end = (start + CHUNK_SIZE).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(end - start)
            .map_err(|_| "Not enough memory for an inserted text chunk".to_owned())?;
        owned.extend_from_slice(&text.as_bytes()[start..end]);
        let bytes: Arc<[u8]> = Arc::from(owned);
        let len = bytes.len();
        pieces.push(Piece::new(bytes, 0..len));
        start = end;
    }
    Ok(pieces)
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    fn text(document: &Document) -> String {
        String::from_utf8(document.read(0..document.len())).unwrap()
    }

    fn assert_indices(document: &Document, reference: &str) {
        assert_eq!(text(document), reference);
        assert_eq!(document.len(), reference.len());
        let starts: Vec<_> = std::iter::once(0)
            .chain(memchr_iter(b'\n', reference.as_bytes()).map(|at| at + 1))
            .collect();
        assert_eq!(document.line_count(), starts.len());
        for (line, start) in starts.iter().enumerate() {
            assert_eq!(document.line_start(line), *start);
        }
        assert_eq!(document.line_start(starts.len() + 3), reference.len());
        for offset in 0..=reference.len() {
            assert_eq!(
                document.line_of_offset(offset),
                memchr_iter(b'\n', &reference.as_bytes()[..offset]).count()
            );
            assert_eq!(
                document.char_boundary(offset),
                reference.is_char_boundary(offset)
            );
        }
        for chunk in document.chunks() {
            assert!(chunk.len() <= CHUNK_SIZE);
            assert!(std::str::from_utf8(chunk).is_ok());
        }
    }

    #[test]
    fn empty_and_clamped_access() {
        let document = Document::new();
        assert!(document.is_empty());
        assert_eq!(document.line_count(), 1);
        assert_eq!(document.line_start(0), 0);
        assert_eq!(document.line_start(10), 0);
        assert_eq!(document.line_of_offset(99), 0);
        assert_eq!(document.read(20..30), Vec::<u8>::new());
        assert!(document.char_boundary(0));
        assert!(!document.char_boundary(1));
        assert_eq!(document.next_char(99), 0);
        assert_eq!(document.previous_char(99), 0);
    }

    #[test]
    fn byte_exact_bom_crlf_mixed_endings_and_final_newline() {
        for bytes in [
            b"\xef\xbb\xbfone\r\ntwo\nthree\rfour".as_slice(),
            b"\xef\xbb\xbf\r\n\nlast\r\n".as_slice(),
            b"no final newline".as_slice(),
            b"".as_slice(),
        ] {
            let mut document = Document::from_bytes(bytes.to_vec()).unwrap();
            let output: Vec<_> = document.chunks().flatten().copied().collect();
            assert_eq!(output, bytes);
            assert_indices(&document, std::str::from_utf8(bytes).unwrap());
            let revision = document.revision();
            document
                .replace(0..document.len(), std::str::from_utf8(bytes).unwrap())
                .unwrap();
            assert_eq!(document.revision(), revision);
            assert!(!document.is_modified());
            assert!(!document.undo());
        }
    }

    #[test]
    fn utf8_crosses_every_input_boundary() {
        let reference = "Aé日🙂\r\nB";
        let chunks = reference
            .as_bytes()
            .iter()
            .map(|byte| Arc::<[u8]>::from([*byte]))
            .collect();
        let mut document = Document::from_chunks(chunks).unwrap();
        assert_indices(&document, reference);
        let at = reference.find('日').unwrap();
        document.replace(at..at + '日'.len_utf8(), "λ\n").unwrap();
        assert_indices(&document, "Aéλ\n🙂\r\nB");
        assert!(document.undo());
        assert_indices(&document, reference);
        assert!(document.redo());
        assert_indices(&document, "Aéλ\n🙂\r\nB");
    }

    #[test]
    fn repairs_codepoint_at_target_chunk_size() {
        let mut bytes = vec![b'x'; CHUNK_SIZE - 1];
        bytes.extend_from_slice("🙂\nlast".as_bytes());
        let reference = String::from_utf8(bytes.clone()).unwrap();
        let mut document = Document::from_bytes(bytes).unwrap();
        assert_eq!(text(&document), reference);
        assert_eq!(document.line_start(1), CHUNK_SIZE + 4);
        assert_eq!(document.line_of_offset(CHUNK_SIZE), 0);
        assert_eq!(document.line_of_offset(CHUNK_SIZE + 4), 1);
        assert_eq!(document.previous_char(CHUNK_SIZE + 3), CHUNK_SIZE - 1);
        assert_eq!(document.next_char(CHUNK_SIZE - 1), CHUNK_SIZE + 3);
        for chunk in document.chunks() {
            assert!(std::str::from_utf8(chunk).is_ok());
            assert!(chunk.len() <= CHUNK_SIZE);
        }
        document
            .replace(CHUNK_SIZE - 2..CHUNK_SIZE + 4, "é\r\n")
            .unwrap();
        assert_eq!(document.line_start(1), CHUNK_SIZE + 2);
        assert!(document.undo());
        assert_eq!(text(&document), reference);
    }

    #[test]
    fn invalid_utf8_never_enters_document() {
        for bytes in [
            vec![0xff],
            vec![0xc0, 0x80],
            vec![0xf0, 0x9f],
            vec![0xed, 0xa0, 0x80],
            vec![0xf4, 0x90, 0x80, 0x80],
        ] {
            assert!(Document::from_bytes(bytes.clone()).is_err());
            let chunks = bytes.into_iter().map(|b| Arc::<[u8]>::from([b])).collect();
            assert!(Document::from_chunks(chunks).is_err());
        }
    }

    #[test]
    fn unchanged_chunk_storage_is_shared_across_edits_and_snapshots() {
        let original: Arc<[u8]> = Arc::from(b"left\nmiddle\nright".as_slice());
        let mut document = Document::from_chunks(vec![original.clone()]).unwrap();
        let snapshot = document.snapshot();
        assert!(Arc::ptr_eq(&document.content, &snapshot.content));
        document.replace(5..11, "new").unwrap();
        assert_eq!(text(&snapshot), "left\nmiddle\nright");
        assert_eq!(text(&document), "left\nnew\nright");
        assert!(Arc::ptr_eq(&document.content.pieces[0].bytes, &original));
        assert!(Arc::ptr_eq(&document.content.pieces[2].bytes, &original));
        assert!(snapshot.undo.is_empty());
        assert!(snapshot.redo.is_empty());
        assert!(document.undo());
        assert_eq!(document.content.pieces.len(), 1);
        assert!(Arc::ptr_eq(&document.content.pieces[0].bytes, &original));
    }

    #[test]
    fn rejects_invalid_edit_ranges_without_changing_state() {
        let mut document = Document::from_bytes("a🙂b".as_bytes().to_vec()).unwrap();
        let revision = document.revision();
        for range in [2..3, 0..99, Range { start: 4, end: 1 }] {
            assert!(document.replace(range, "x").is_err());
        }
        assert_eq!(document.revision(), revision);
        assert_eq!(text(&document), "a🙂b");
        assert!(!document.undo());
    }

    #[test]
    fn undo_redo_and_background_saved_revision() {
        let mut document = Document::from_bytes(b"abc".to_vec()).unwrap();
        document.replace(1..2, "B").unwrap();
        let saved = document.snapshot();
        document.replace(3..3, "!").unwrap();
        document.mark_saved_revision(saved.revision());
        assert!(document.is_modified());
        assert!(document.undo());
        assert_eq!(text(&document), "aBc");
        assert!(!document.is_modified());
        assert!(document.undo());
        assert_eq!(text(&document), "abc");
        assert!(document.is_modified());
        assert!(document.redo());
        assert!(!document.is_modified());
        document.replace(0..1, "A").unwrap();
        assert!(!document.redo());
        document.mark_saved();
        assert!(!document.is_modified());
    }

    #[test]
    fn history_is_limited_and_snapshots_have_no_history() {
        let mut document = Document::new();
        for index in 0..HISTORY_LIMIT + 7 {
            document.replace(index..index, "x").unwrap();
        }
        let mut snapshot = document.snapshot();
        assert!(!snapshot.undo());
        let mut count = 0;
        while document.undo() {
            count += 1;
        }
        assert_eq!(count, HISTORY_LIMIT);
        assert_eq!(document.len(), 7);
        for _ in 0..HISTORY_LIMIT {
            assert!(document.redo());
        }
        assert!(!document.redo());
    }

    #[test]
    fn replace_ranges_is_one_undo_transaction_with_original_coordinates() {
        let mut document = Document::from_bytes("α one\none\r\none Ω".as_bytes().to_vec()).unwrap();
        let original = text(&document);
        let ranges: Vec<_> = original
            .match_indices("one")
            .map(|(at, s)| at..at + s.len())
            .collect();
        document.replace_ranges(&ranges, "🙂\nx").unwrap();
        assert_indices(&document, &original.replace("one", "🙂\nx"));
        assert!(document.undo());
        assert_indices(&document, &original);
        assert!(!document.undo());
        assert!(document.redo());
        assert_indices(&document, &original.replace("one", "🙂\nx"));
    }

    #[test]
    fn failed_or_cancelled_bulk_edits_are_atomic() {
        let mut document = Document::from_bytes(b"abcdef".to_vec()).unwrap();
        let revision = document.revision();
        assert!(document.replace_ranges(&[1..3, 2..4], "x").is_err());
        assert!(document.replace_ranges(&[4..5, 0..1], "x").is_err());
        assert!(document.replace_ranges(&[0..99], "x").is_err());
        assert!(
            document
                .replace_ranges_cancellable(&[1..2], "x", &AtomicBool::new(true))
                .is_err()
        );
        assert_eq!(document.revision(), revision);
        assert_eq!(text(&document), "abcdef");
        assert!(!document.undo());
        assert!(
            Document::from_chunks_cancellable(
                vec![Arc::from(b"abc".as_slice())],
                &AtomicBool::new(true),
            )
            .is_err()
        );
    }

    #[test]
    fn prepared_snapshot_adoption_is_one_undo_action() {
        let mut document = Document::from_bytes(b"one two one".to_vec()).unwrap();
        let mut prepared = document.snapshot();
        assert!(!document.apply_snapshot(&prepared));
        prepared.replace_ranges(&[0..3, 8..11], "three").unwrap();
        assert!(document.apply_snapshot(&prepared));
        assert_eq!(text(&document), "three two three");
        assert!(document.undo());
        assert_eq!(text(&document), "one two one");
        assert!(!document.undo());
        assert!(document.redo());
        assert_eq!(text(&document), "three two three");
    }

    #[test]
    fn empty_replacement_and_insertions_at_both_ends() {
        let mut document = Document::from_bytes(b"abc".to_vec()).unwrap();
        document.replace_ranges(&[0..0, 3..3], "🙂").unwrap();
        assert_indices(&document, "🙂abc🙂");
        document.replace(0..document.len(), "").unwrap();
        assert_indices(&document, "");
        assert!(document.undo());
        assert_indices(&document, "🙂abc🙂");
        assert!(document.undo());
        assert_indices(&document, "abc");
    }

    #[test]
    fn generated_edits_match_string_reference_and_restore_exactly() {
        let mut document = Document::from_bytes("a\r\né日🙂\nend".as_bytes().to_vec()).unwrap();
        let mut reference = text(&document);
        let original = reference.clone();
        let fragments = ["", "x", "\n", "\r\n", "🙂", "é日"];
        let mut state = 0x1234_5678u64;
        let mut count = 0;
        for _ in 0..120 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let boundaries: Vec<_> = reference
                .char_indices()
                .map(|(at, _)| at)
                .chain(std::iter::once(reference.len()))
                .collect();
            let a = boundaries[(state as usize) % boundaries.len()];
            let b = boundaries[((state >> 16) as usize) % boundaries.len()];
            let range = a.min(b)..a.max(b);
            let inserted = fragments[((state >> 32) as usize) % fragments.len()];
            let revision = document.revision();
            document.replace(range.clone(), inserted).unwrap();
            reference.replace_range(range, inserted);
            if document.revision() != revision {
                count += 1;
            }
            assert_indices(&document, &reference);
        }
        let last = reference;
        for _ in 0..count {
            assert!(document.undo());
        }
        assert_eq!(text(&document), original);
        assert!(!document.undo());
        for _ in 0..count {
            assert!(document.redo());
        }
        assert_eq!(text(&document), last);
    }

    #[test]
    fn million_match_replacement_streams_to_a_few_chunks_before_commit() {
        let count = 1_000_000;
        let original = "ab".repeat(count);
        let ranges: Vec<_> = (0..count).map(|index| 2 * index..2 * index + 1).collect();
        let mut document = Document::from_bytes(original.as_bytes().to_vec()).unwrap();
        document.replace_ranges(&ranges, "x").unwrap();
        assert_eq!(
            document.chunks().count(),
            document.len().div_ceil(CHUNK_SIZE)
        );
        assert_eq!(text(&document), "xb".repeat(count));
        // Foreground edits and undo now traverse ~8 pieces, not 2M pieces.
        document.replace(1..1, "🙂").unwrap();
        assert!(document.chunks().count() <= 11);
        assert!(document.undo());
        assert_eq!(text(&document), "xb".repeat(count));
        assert!(document.undo());
        assert_eq!(text(&document), original);
        assert!(!document.undo());
        assert!(document.redo());
        assert_eq!(text(&document), "xb".repeat(count));
    }

    #[test]
    fn bulk_compaction_keeps_utf8_and_newline_indices_valid() {
        let count = 100_000;
        let original = "ab".repeat(count);
        let ranges: Vec<_> = (0..count).map(|index| 2 * index..2 * index + 1).collect();
        let mut document = Document::from_bytes(original.into_bytes()).unwrap();
        document.replace_ranges(&ranges, "🙂\r\n").unwrap();
        assert_eq!(text(&document), "🙂\r\nb".repeat(count));
        assert_eq!(document.line_count(), count + 1);
        assert!(document.chunks().count() <= document.len().div_ceil(CHUNK_SIZE - 3));
        for chunk in document.chunks() {
            assert!(chunk.len() <= CHUNK_SIZE);
            assert!(std::str::from_utf8(chunk).is_ok());
        }
        for line in [0, 1, 127, 128, 129, 40_000, count] {
            let expected = if line == 0 { 0 } else { line * 7 - 1 };
            assert_eq!(document.line_start(line), expected);
            assert_eq!(document.line_of_offset(expected), line);
        }
    }

    #[test]
    fn dense_line_checkpoints_are_sparse_shared_and_survive_slices() {
        let original = "x\n".repeat(CHUNK_SIZE * 2);
        let mut document = Document::from_bytes(original.into_bytes()).unwrap();
        let checkpoints = document.content.pieces[0].newline_checkpoints.clone();
        assert_eq!(checkpoints.len(), (CHUNK_SIZE / 2).div_ceil(NEWLINE_STRIDE));
        for line in 130_000..130_100 {
            assert_eq!(document.line_start(line), line * 2);
        }
        document.replace(10..12, "🙂\n").unwrap();
        assert!(Arc::ptr_eq(
            &document.content.pieces[0].newline_checkpoints,
            &checkpoints,
        ));
        assert!(Arc::ptr_eq(
            &document.content.pieces[2].newline_checkpoints,
            &checkpoints,
        ));
        for line in [0, 1, 5, 6, 127, 128, 129, 130_000, 200_000] {
            assert_eq!(
                document.line_start(line),
                line * 2 + if line > 5 { 3 } else { 0 }
            );
        }
        assert!(document.undo());
        for line in 130_000..130_100 {
            assert_eq!(document.line_start(line), line * 2);
        }
    }

    #[test]
    fn sequential_typing_coalesces_locally_and_remains_undoable() {
        let mut document = Document::new();
        for offset in 0..100_000 {
            document.replace(offset..offset, "x").unwrap();
        }
        assert_eq!(document.len(), 100_000);
        assert!(
            document.chunks().count() < 64,
            "typing must not accumulate one piece per keystroke"
        );
        for _ in 0..HISTORY_LIMIT {
            assert!(document.undo());
        }
        assert_eq!(document.len(), 100_000 - HISTORY_LIMIT);
        assert!(!document.undo());
        for _ in 0..HISTORY_LIMIT {
            assert!(document.redo());
        }
        assert_eq!(document.read(0..document.len()), vec![b'x'; 100_000]);
        assert!(document.chunks().count() < 64);
    }

    #[test]
    fn compact_builder_cancels_between_bounded_writes() {
        let cancel = AtomicBool::new(false);
        let mut builder = CompactBuilder::new(CHUNK_SIZE * 2, &cancel).unwrap();
        builder.write(&vec![b'x'; CHUNK_SIZE - 1]).unwrap();
        builder.write("🙂".as_bytes()).unwrap();
        assert_eq!(builder.pieces.len(), 1);
        assert_eq!(builder.pieces[0].len(), CHUNK_SIZE - 1);
        assert!(std::str::from_utf8(builder.pieces[0].as_slice()).is_ok());
        cancel.store(true, Ordering::Relaxed);
        assert!(builder.write(b"next").is_err());
        assert!(builder.finish().is_err());
    }

    #[test]
    fn oversized_bulk_expansion_is_rejected_before_allocating_output() {
        let count = MAX_DOCUMENT_BYTES / CHUNK_SIZE + 1;
        let original = "a".repeat(count);
        let ranges: Vec<_> = (0..count).map(|offset| offset..offset + 1).collect();
        let replacement = "x".repeat(CHUNK_SIZE);
        let mut document = Document::from_bytes(original.as_bytes().to_vec()).unwrap();
        let revision = document.revision();
        let error = document.replace_ranges(&ranges, &replacement).unwrap_err();
        assert!(error.contains("2 GiB"));
        assert_eq!(document.revision(), revision);
        assert_eq!(text(&document), original);
        assert!(!document.undo());
    }

    #[test]
    fn oversized_shared_chunk_input_is_rejected_before_indexing() {
        // The logical input exceeds the cap, but all entries share one 256 KiB
        // backing allocation. This regression needs no multi-gigabyte fixture.
        let bytes: Arc<[u8]> = Arc::from(vec![b'x'; CHUNK_SIZE]);
        let chunks = vec![bytes; MAX_DOCUMENT_BYTES / CHUNK_SIZE + 1];
        let error = Document::from_chunks(chunks).unwrap_err();
        assert!(error.contains("2 GiB"));
    }

    #[test]
    fn projected_size_checks_boundary_underflow_and_overflow_without_allocation() {
        assert_eq!(
            checked_document_len(MAX_DOCUMENT_BYTES, 1, 1).unwrap(),
            MAX_DOCUMENT_BYTES,
        );
        assert_eq!(
            checked_document_len(MAX_DOCUMENT_BYTES - 1, 0, 1).unwrap(),
            MAX_DOCUMENT_BYTES,
        );
        assert!(checked_document_len(MAX_DOCUMENT_BYTES, 0, 1).is_err());
        assert!(checked_document_len(usize::MAX, 0, 1).is_err());
        assert!(checked_document_len(0, 1, 0).is_err());
    }
}
