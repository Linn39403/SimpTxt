//! UTF-8 piece-table storage. Small edits share unchanged chunks, with bounded
//! local coalescing. Highly fragmented bulk output streams into fresh chunks.
//!
//! File loading should use `from_chunks_cancellable`: the convenience `from_bytes`
//! constructor necessarily starts with an already contiguous input allocation.

use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use memchr::memchr_iter;

pub const CHUNK_SIZE: usize = 256 * 1024;
pub const HISTORY_LIMIT: usize = 1_000;
/// Unique undo/redo storage plus retained bookkeeping metadata. An operation
/// whose newest transaction cannot fit in either replay direction is rejected
/// before publication; older entries may be evicted to keep this budget.
pub const HISTORY_BYTE_LIMIT: usize = 128 * 1024 * 1024;
const CHANGE_LOG_LIMIT: usize = 64;
pub const DOCUMENT_MEMORY_LIMIT: usize = 2 * 1024 * 1024 * 1024;
static RESERVED_BYTES: AtomicUsize = AtomicUsize::new(0);

/// In-flight document storage admission. Dropping it releases only the
/// reservation; published allocations stay charged through their own tokens.
#[derive(Debug)]
pub struct DocumentMemoryReservation(usize);

impl Drop for DocumentMemoryReservation {
    fn drop(&mut self) {
        RESERVED_BYTES.fetch_sub(self.0, Ordering::SeqCst);
    }
}

impl DocumentMemoryReservation {
    fn ensure(&mut self, bytes: usize) -> Result<(), String> {
        let additional = bytes.saturating_sub(self.0);
        if additional == 0 {
            return Ok(());
        }
        RESERVED_BYTES
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |reserved| {
                let proposed = reserved.checked_add(additional)?;
                (proposed.saturating_add(resident_document_bytes()) <= DOCUMENT_MEMORY_LIMIT)
                    .then_some(proposed)
            })
            .map_err(|_| "The operation would exceed the 2 GiB tracked document-memory budget. Close another tab or wait for background work; the document was not changed.".to_owned())?;
        self.0 += additional;
        Ok(())
    }

    /// Grow a raw-file load reservation before reading additional bytes.
    pub fn ensure_input_bytes(&mut self, input_len: usize) -> Result<(), String> {
        self.ensure(estimated_document_bytes(input_len))
    }
}

pub fn reserved_document_bytes() -> usize {
    RESERVED_BYTES.load(Ordering::SeqCst)
}

pub fn reserve_load_memory(input_len: usize) -> Result<DocumentMemoryReservation, String> {
    reserve_memory(estimated_document_bytes(input_len))
}

fn reserve_memory(bytes: usize) -> Result<DocumentMemoryReservation, String> {
    let mut reservation = DocumentMemoryReservation(0);
    reservation.ensure(bytes)?;
    Ok(reservation)
}

const SCALAR_STRIDE: usize = 256;

static RESIDENT_BYTES: AtomicUsize = AtomicUsize::new(0);
type AllocationKey = (usize, usize);
static BACKINGS: OnceLock<Mutex<HashMap<AllocationKey, Weak<MemoryCharge>>>> = OnceLock::new();

/// Conservative resident storage estimate for every live Document and snapshot
/// in this process. Shared immutable allocations are counted once. This includes
/// retained undo/redo, sparse indices and allocated metadata capacity, but not
/// unrelated GUI/search buffers or allocator-specific arena overhead.
pub fn resident_document_bytes() -> usize {
    RESIDENT_BYTES.load(Ordering::Relaxed)
}

/// Conservative additional allocation reservation for ordinary chunked UTF-8
/// input, including worst-case sparse newline/scalar indices and piece metadata.
/// Callers must reserve in-flight raw input separately until it becomes a Document.
pub fn estimated_document_bytes(input_len: usize) -> usize {
    let pieces = input_len.div_ceil(CHUNK_SIZE - 3).saturating_add(1);
    input_len
        .saturating_add(
            input_len
                .div_ceil(NEWLINE_STRIDE)
                .saturating_mul(std::mem::size_of::<usize>()),
        )
        .saturating_add(
            input_len
                .div_ceil(SCALAR_STRIDE)
                .saturating_mul(std::mem::size_of::<usize>()),
        )
        .saturating_add(pieces.saturating_mul(2048))
        // Loader/compact-builder scratch and the Vec-to-Arc chunk conversion.
        .saturating_add(2 * CHUNK_SIZE)
}

#[derive(Debug)]
struct MemoryCharge {
    bytes: usize,
    backing: Option<AllocationKey>,
}

impl MemoryCharge {
    fn unique(bytes: usize) -> Arc<Self> {
        // Arc headers and this small accounting object are retained allocations too.
        let bytes =
            bytes.saturating_add(std::mem::size_of::<Self>() + 2 * std::mem::size_of::<usize>());
        RESIDENT_BYTES.fetch_add(bytes, Ordering::Relaxed);
        Arc::new(Self {
            bytes,
            backing: None,
        })
    }

    fn backing(bytes: &Arc<[u8]>) -> Arc<Self> {
        let key = (bytes.as_ptr() as usize, bytes.len());
        let mut registry = BACKINGS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap();
        if let Some(charge) = registry.get(&key).and_then(Weak::upgrade) {
            return charge;
        }
        // Include the backing Arc header and a conservative registry bucket allowance.
        let size = bytes
            .len()
            .saturating_add(128 + std::mem::size_of::<Self>());
        RESIDENT_BYTES.fetch_add(size, Ordering::Relaxed);
        let charge = Arc::new(Self {
            bytes: size,
            backing: Some(key),
        });
        registry.insert(key, Arc::downgrade(&charge));
        charge
    }
}

impl Drop for MemoryCharge {
    fn drop(&mut self) {
        if let Some(key) = self.backing {
            let mut registry = BACKINGS.get().unwrap().lock().unwrap();
            if registry
                .get(&key)
                .is_some_and(|weak| std::ptr::eq(weak.as_ptr(), self))
            {
                registry.remove(&key);
                // Do not leave an ever-growing registry after transient large jobs.
                if registry.capacity() > registry.len().saturating_mul(4).max(64) {
                    registry.shrink_to_fit();
                }
            }
        }
        RESIDENT_BYTES.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocumentChange {
    pub before_revision: u64,
    pub after_revision: u64,
    pub changed_from: usize,
    pub old_end: usize,
    pub new_end: usize,
}

#[derive(Debug, Default)]
struct AllocationUse {
    bytes: usize,
    live: usize,
    history: usize,
}

/// Incremental reference accounting: transitions visit piece metadata, never
/// text bytes or all previous edits. History snapshots share allocations here.
#[derive(Debug, Default)]
struct HistoryUsage {
    allocations: HashMap<usize, AllocationUse>,
    retained: usize,
}

impl HistoryUsage {
    fn adjust(&mut self, charge: &Arc<MemoryCharge>, history: bool, add: bool) {
        let key = Arc::as_ptr(charge) as usize;
        let entry = self
            .allocations
            .entry(key)
            .or_insert_with(|| AllocationUse {
                bytes: charge.bytes,
                ..Default::default()
            });
        if entry.history > 0 && entry.live == 0 {
            self.retained -= entry.bytes;
        }
        let count = if history {
            &mut entry.history
        } else {
            &mut entry.live
        };
        if add {
            *count += 1;
        } else {
            *count -= 1;
        }
        if entry.history > 0 && entry.live == 0 {
            self.retained += entry.bytes;
        }
        if entry.history == 0 && entry.live == 0 {
            self.allocations.remove(&key);
        }
    }

    fn content(&mut self, content: &Content, history: bool, add: bool) {
        content.visit_allocations(|charge| self.adjust(charge, history, add));
    }

    fn edit(&mut self, edit: &Edit, add: bool) {
        self.adjust(&edit.memory, true, add);
        match &edit.change {
            Change::Splice {
                removed, inserted, ..
            } => {
                for piece in removed.iter().chain(inserted) {
                    piece.visit_allocations(|charge| self.adjust(charge, true, add));
                }
            }
            Change::Snapshot { before, after } => {
                self.content(before, true, add);
                self.content(after, true, add);
            }
        }
    }
}

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

fn bulk_allocation_bytes(
    output_len: usize,
    inserted_text_len: usize,
    estimated_pieces: usize,
) -> usize {
    if estimated_pieces > BULK_COMPACT_PIECES {
        estimated_document_bytes(output_len)
    } else {
        estimated_document_bytes(inserted_text_len)
            .saturating_add(estimated_pieces.saturating_mul(2048))
    }
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
    scalars: usize,
    scalar_checkpoints: Arc<[usize]>,
    first_scalar: usize,
    ascii: bool,
    bytes_memory: Arc<MemoryCharge>,
    newline_memory: Arc<MemoryCharge>,
    scalar_memory: Arc<MemoryCharge>,
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
        let ascii = bytes[range.clone()].is_ascii();
        let mut scalar_checkpoints = Vec::new();
        let scalars = if ascii {
            range.len()
        } else {
            let mut count = 0;
            for (offset, byte) in bytes[range.clone()].iter().enumerate() {
                if byte & 0xc0 != 0x80 {
                    if count % SCALAR_STRIDE == 0 {
                        scalar_checkpoints.push(range.start + offset);
                    }
                    count += 1;
                }
            }
            count
        };
        let bytes_memory = MemoryCharge::backing(&bytes);
        let newline_memory = MemoryCharge::unique(
            checkpoints.len() * std::mem::size_of::<usize>() + 2 * std::mem::size_of::<usize>(),
        );
        let scalar_memory = MemoryCharge::unique(
            scalar_checkpoints.len() * std::mem::size_of::<usize>()
                + 2 * std::mem::size_of::<usize>(),
        );
        Self {
            bytes,
            range,
            newlines,
            newline_checkpoints: Arc::from(checkpoints),
            first_newline: 0,
            scalars,
            scalar_checkpoints: Arc::from(scalar_checkpoints),
            first_scalar: 0,
            ascii,
            bytes_memory,
            newline_memory,
            scalar_memory,
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
        let first_scalar = self.first_scalar + self.scalar_count(0..range.start);
        let scalars = self.scalar_count(range.clone());
        Self {
            bytes: self.bytes.clone(),
            range: self.range.start + range.start..self.range.start + range.end,
            newlines,
            newline_checkpoints: self.newline_checkpoints.clone(),
            first_newline,
            scalars,
            first_scalar,
            ascii: self.ascii,
            scalar_checkpoints: self.scalar_checkpoints.clone(),
            bytes_memory: self.bytes_memory.clone(),
            newline_memory: self.newline_memory.clone(),
            scalar_memory: self.scalar_memory.clone(),
        }
    }

    fn visit_allocations(&self, mut visit: impl FnMut(&Arc<MemoryCharge>)) {
        visit(&self.bytes_memory);
        visit(&self.newline_memory);
        visit(&self.scalar_memory);
    }

    fn scalar_count(&self, range: Range<usize>) -> usize {
        if self.ascii {
            return range.len();
        }
        self.as_slice()[range]
            .iter()
            .filter(|byte| **byte & 0xc0 != 0x80)
            .count()
    }

    fn scalar_offset(&self, ordinal: usize) -> usize {
        if ordinal >= self.scalars {
            return self.len();
        }
        if self.ascii {
            return ordinal;
        }
        let ordinal = self.first_scalar + ordinal;
        let checkpoint = self.scalar_checkpoints[ordinal / SCALAR_STRIDE];
        let relative = self.bytes[checkpoint..self.range.end]
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte & 0xc0 != 0x80)
            .nth(ordinal % SCALAR_STRIDE)
            .unwrap()
            .0;
        checkpoint + relative - self.range.start
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
    memory: Arc<MemoryCharge>,
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
        let memory = Self::charge(&pieces, &byte_ends, &newline_ends);
        Self {
            pieces,
            byte_ends,
            newline_ends,
            len,
            newlines,
            memory,
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
        let memory = Self::charge(&pieces, &byte_ends, &newline_ends);
        Ok(Self {
            pieces,
            byte_ends,
            newline_ends,
            len,
            newlines,
            memory,
        })
    }

    fn charge(
        pieces: &Vec<Piece>,
        byte_ends: &Vec<usize>,
        newline_ends: &Vec<usize>,
    ) -> Arc<MemoryCharge> {
        MemoryCharge::unique(
            std::mem::size_of::<Self>()
                + 2 * std::mem::size_of::<usize>()
                + pieces.capacity() * std::mem::size_of::<Piece>()
                + (byte_ends.capacity() + newline_ends.capacity()) * std::mem::size_of::<usize>(),
        )
    }

    fn visit_allocations(&self, mut visit: impl FnMut(&Arc<MemoryCharge>)) {
        visit(&self.memory);
        for piece in &self.pieces {
            piece.visit_allocations(&mut visit);
        }
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
        && Arc::ptr_eq(&previous.scalar_checkpoints, &piece.scalar_checkpoints)
        && previous.first_newline + previous.newlines == piece.first_newline
        && previous.first_scalar + previous.scalars == piece.first_scalar
        && previous.range.end == piece.range.start
        && previous.len() + piece.len() <= CHUNK_SIZE
    {
        previous.range.end = piece.range.end;
        previous.newlines += piece.newlines;
        previous.scalars += piece.scalars;
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

#[derive(Clone, Debug)]
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
    envelope: Range<usize>,
    inserted_len: usize,
    memory: Arc<MemoryCharge>,
}

impl Edit {
    fn charge(change: &Change) -> Arc<MemoryCharge> {
        let pieces = match change {
            Change::Splice {
                removed, inserted, ..
            } => removed.capacity() + inserted.capacity(),
            Change::Snapshot { .. } => 0,
        };
        MemoryCharge::unique(std::mem::size_of::<Self>() + pieces * std::mem::size_of::<Piece>())
    }
}

#[derive(Debug)]
pub struct Document {
    content: Arc<Content>,
    undo: VecDeque<Edit>,
    redo: Vec<Edit>,
    revision: u64,
    saved_revision: u64,
    usage: HistoryUsage,
    history_byte_limit: usize,
    history_evictions: u64,
    changes: VecDeque<DocumentChange>,
    metadata_memory: Arc<MemoryCharge>,
}

/// Cloning is a cheap, independent snapshot. Undo/redo history is deliberately
/// not copied into background work or saving snapshots.
impl Clone for Document {
    fn clone(&self) -> Self {
        let mut snapshot =
            Self::from_shared_content(self.content.clone(), self.revision, self.saved_revision);
        snapshot.history_byte_limit = self.history_byte_limit;
        snapshot.changes = self.changes.clone();
        snapshot.refresh_metadata_charge();
        snapshot
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
        Self::from_shared_content(Arc::new(content), revision, revision)
    }

    fn from_shared_content(content: Arc<Content>, revision: u64, saved_revision: u64) -> Self {
        let mut usage = HistoryUsage::default();
        usage.content(&content, false, true);
        let mut document = Self {
            content,
            undo: VecDeque::new(),
            redo: Vec::new(),
            revision,
            saved_revision,
            usage,
            history_byte_limit: HISTORY_BYTE_LIMIT,
            history_evictions: 0,
            changes: VecDeque::new(),
            metadata_memory: MemoryCharge::unique(0),
        };
        document.refresh_metadata_charge();
        document
    }

    fn refresh_metadata_charge(&mut self) {
        let bytes = std::mem::size_of::<Self>()
            + (self.undo.capacity() + self.redo.capacity()) * std::mem::size_of::<Edit>()
            + self.changes.capacity() * std::mem::size_of::<DocumentChange>()
            + self.usage.allocations.capacity()
                * (std::mem::size_of::<(usize, AllocationUse)>() + 16);
        self.metadata_memory = MemoryCharge::unique(bytes);
    }

    fn set_content(&mut self, content: Arc<Content>) {
        // Add before removing to preserve allocation identity across both states.
        self.usage.content(&content, false, true);
        self.usage.content(&self.content, false, false);
        self.content = content;
    }

    pub fn history_bytes(&self) -> usize {
        if self.undo.is_empty() && self.redo.is_empty() {
            return 0;
        }
        self.usage.retained
            + (self.undo.capacity() + self.redo.capacity()) * std::mem::size_of::<Edit>()
            + self.usage.allocations.capacity()
                * (std::mem::size_of::<(usize, AllocationUse)>() + 16)
    }

    /// Unique storage reachable from this document (history included). Unlike
    /// resident_document_bytes, summing this across snapshots double-counts sharing.
    pub fn retained_storage_bytes(&self) -> usize {
        self.usage
            .allocations
            .values()
            .map(|entry| entry.bytes)
            .sum::<usize>()
            + self.metadata_memory.bytes
    }

    pub fn history_evictions(&self) -> u64 {
        self.history_evictions
    }

    pub fn undo_group_revision(&self) -> Option<u64> {
        self.undo.back().map(|edit| edit.before_revision)
    }

    pub fn set_history_byte_limit(&mut self, limit: usize) {
        self.history_byte_limit = limit;
        self.trim_history();
    }

    pub fn last_change(&self) -> Option<DocumentChange> {
        self.changes.back().copied()
    }

    pub fn changes_since(&self, revision: u64) -> Option<Vec<DocumentChange>> {
        if revision == self.revision {
            return Some(Vec::new());
        }
        let start = self
            .changes
            .iter()
            .rposition(|change| change.before_revision == revision)?;
        let changes: Vec<_> = self.changes.iter().skip(start).copied().collect();
        let mut expected = revision;
        for change in &changes {
            if change.before_revision != expected {
                return None;
            }
            expected = change.after_revision;
        }
        (expected == self.revision).then_some(changes)
    }

    fn log_change(&mut self, before_revision: u64, range: Range<usize>, inserted_len: usize) {
        self.changes.push_back(DocumentChange {
            before_revision,
            after_revision: self.revision,
            changed_from: range.start,
            old_end: range.end,
            new_end: range.start + inserted_len,
        });
        if self.changes.len() > CHANGE_LOG_LIMIT {
            self.changes.pop_front();
        }
    }

    fn trim_history(&mut self) {
        loop {
            if self.history_bytes() > self.history_byte_limit {
                // Release old queue/accounting capacity before evicting useful
                // edits. This also makes single-edit admission match retention.
                self.undo.shrink_to_fit();
                self.redo.shrink_to_fit();
                self.usage.allocations.shrink_to_fit();
            }
            if self.undo.len() + self.redo.len() <= HISTORY_LIMIT
                && self.history_bytes() <= self.history_byte_limit
            {
                break;
            }
            // Remove oldest undo first, otherwise farthest redo, never a gap.
            let removed = self.undo.pop_front().or_else(|| {
                if self.redo.is_empty() {
                    None
                } else {
                    Some(self.redo.remove(0))
                }
            });
            let Some(edit) = removed else {
                break;
            };
            self.usage.edit(&edit, false);
            self.history_evictions += 1;
        }
        if self.undo.is_empty() {
            self.undo.shrink_to_fit();
        }
        if self.redo.is_empty() {
            self.redo.shrink_to_fit();
        }
        if self.usage.allocations.capacity()
            > self.usage.allocations.len().saturating_mul(4).max(64)
        {
            self.usage.allocations.shrink_to_fit();
        }
        self.refresh_metadata_charge();
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
        Self::from_chunks_cancellable_reserved(chunks, cancel, DocumentMemoryReservation(0))
    }

    /// Adopt a raw-load reservation without a release/reacquire gap or charging
    /// the same file twice. Keep this token alive throughout validation/indexing.
    pub fn from_chunks_cancellable_reserved(
        chunks: Vec<Arc<[u8]>>,
        cancel: &AtomicBool,
        mut reservation: DocumentMemoryReservation,
    ) -> Result<Self, String> {
        let mut input_len = 0usize;
        for chunk in &chunks {
            if cancel.load(Ordering::Relaxed) {
                return Err("Loading cancelled".to_owned());
            }
            input_len = checked_document_len(input_len, 0, chunk.len())?;
        }
        // Callers may supply tiny chunks rather than the loader's usual 256 KiB
        // chunks. Reserve their actual metadata count before allocating pieces.
        reservation.ensure(
            estimated_document_bytes(input_len).saturating_add(chunks.len().saturating_mul(2048)),
        )?;
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

    pub fn chunks(&self) -> impl DoubleEndedIterator<Item = &[u8]> {
        self.content.pieces.iter().map(Piece::as_slice)
    }

    /// Piece slices intersecting the range, in reverse order. A binary seek
    /// starts near the cursor rather than traversing an unrelated prefix.
    pub fn chunks_reverse(&self, range: Range<usize>) -> impl Iterator<Item = (usize, &[u8])> {
        let start = range.start.min(self.len());
        let end = range.end.min(self.len()).max(start);
        let first = self.content.piece_at(start);
        let last = if end > start {
            self.content.piece_at(end - 1) + 1
        } else {
            first
        };
        (first..last).rev().map(move |index| {
            let at = self.content.piece_start(index);
            let piece = &self.content.pieces[index];
            let from = start.saturating_sub(at);
            let to = piece.len().min(end - at);
            (at + from, &piece.as_slice()[from..to])
        })
    }

    /// Count Unicode scalar values, not UTF-8 bytes. Full pieces use cached counts.
    pub fn scalar_count(&self, range: Range<usize>) -> usize {
        let start = range.start.min(self.len());
        let end = range.end.min(self.len()).max(start);
        let mut at = start;
        let mut index = self.content.piece_at(start);
        let mut count = 0;
        while at < end {
            let piece = &self.content.pieces[index];
            let from = at - self.content.piece_start(index);
            let to = piece.len().min(end - self.content.piece_start(index));
            count += if from == 0 && to == piece.len() {
                piece.scalars
            } else {
                piece.scalar_count(from..to)
            };
            at += to - from;
            index += 1;
        }
        count
    }

    /// Advance by Unicode scalar values, clamped at EOF. The caller may clamp
    /// the result further to its desired line ending.
    pub fn offset_at_scalar(&self, start: usize, column: usize) -> usize {
        let mut at = start.min(self.len());
        while !self.char_boundary(at) {
            at += 1;
        }
        let mut remaining = column;
        let mut index = self.content.piece_at(at);
        while at < self.len() {
            let piece = &self.content.pieces[index];
            let from = at - self.content.piece_start(index);
            let prior = if from == 0 {
                0
            } else {
                piece.scalar_count(0..from)
            };
            let available = piece.scalars - prior;
            if remaining <= available {
                return self.content.piece_start(index) + piece.scalar_offset(prior + remaining);
            }
            remaining -= available;
            at = self.content.byte_ends[index];
            index += 1;
        }
        self.len()
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
        self.replace_grouped(range, text, false)
    }

    /// Join only adjacent insertion typing, and never cross a saved revision.
    /// Every actual edit still gets a revision and a syntax-change log entry.
    pub fn replace_grouped(
        &mut self,
        range: Range<usize>,
        text: &str,
        join_previous: bool,
    ) -> Result<(), String> {
        self.validate_range(&range)?;
        if self.range_equals(&range, text.as_bytes()) {
            return Ok(());
        }
        checked_document_len(self.len(), range.end - range.start, text.len())?;
        let _reservation = reserve_memory(
            estimated_document_bytes(text.len())
                .saturating_add(self.content.pieces.len().saturating_mul(2048)),
        )?;
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
        let after = Self::spliced_content(&self.content, range.clone(), &inserted);
        self.commit_change(
            after,
            Change::Splice {
                offset: range.start,
                removed,
                inserted,
            },
            range,
            text.len(),
            join_previous,
        )
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
        // Sparse output shares existing and repeated inserted bytes. Reserve
        // full output text only when the builder will actually copy that text.
        let _reservation = reserve_memory(bulk_allocation_bytes(new_len, text.len(), estimated))?;
        let changed_from = effective.first().unwrap().start;
        let old_end = effective.last().unwrap().end;
        let envelope_inserted = new_len - (self.len() - (old_end - changed_from));
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
        let before = self.content.clone();
        self.commit_change(
            after.clone(),
            Change::Snapshot { before, after },
            changed_from..old_end,
            envelope_inserted,
            false,
        )
    }

    fn spliced_content(content: &Content, range: Range<usize>, inserted: &[Piece]) -> Arc<Content> {
        let mut pieces = Vec::with_capacity(content.pieces.len() + inserted.len() + 2);
        content.append_range(0..range.start, &mut pieces);
        let edit_piece = pieces.len();
        for piece in inserted {
            push_piece(&mut pieces, piece.clone());
        }
        content.append_range(range.end..content.len, &mut pieces);
        coalesce_near(&mut pieces, edit_piece);
        Arc::new(Content::from_pieces(pieces))
    }

    fn splice(&mut self, range: Range<usize>, inserted: &[Piece]) {
        self.set_content(Self::spliced_content(&self.content, range, inserted));
    }

    fn single_edit_history_bytes(content: &Content, edit: &Edit) -> usize {
        let mut usage = HistoryUsage::default();
        usage.content(content, false, true);
        usage.edit(edit, true);
        usage.allocations.shrink_to_fit();
        usage.retained
            + 2 * std::mem::size_of::<Edit>()
            + usage.allocations.capacity() * (std::mem::size_of::<(usize, AllocationUse)>() + 16)
    }

    /// Prepare the exact new undo group, validate both replay directions, then
    /// publish once. Failed admission does not prune history, clear redo, change
    /// revisions, or alter live text, including for already-prepared snapshots.
    fn commit_change(
        &mut self,
        after: Arc<Content>,
        change: Change,
        range: Range<usize>,
        inserted_len: usize,
        join: bool,
    ) -> Result<(), String> {
        let before_revision = self.revision;
        let can_join = join
            && before_revision != self.saved_revision
            && range.is_empty()
            && self
                .undo
                .back()
                .is_some_and(|previous| match (&previous.change, &change) {
                    (
                        Change::Splice {
                            offset,
                            removed,
                            inserted,
                        },
                        Change::Splice {
                            removed: next_removed,
                            ..
                        },
                    ) => {
                        removed.is_empty()
                            && next_removed.is_empty()
                            && *offset + inserted.iter().map(Piece::len).sum::<usize>()
                                == range.start
                    }
                    _ => false,
                });
        let (change, group_before, envelope, group_inserted) = if can_join {
            let previous = self.undo.back().unwrap();
            let mut combined = previous.change.clone();
            if let (Change::Splice { inserted, .. }, Change::Splice { inserted: next, .. }) =
                (&mut combined, change)
            {
                for piece in next {
                    push_piece(inserted, piece);
                }
                let center = inserted.len().saturating_sub(1);
                coalesce_near(inserted, center);
            }
            (
                combined,
                previous.before_revision,
                previous.envelope.clone(),
                previous.inserted_len + inserted_len,
            )
        } else {
            (change, before_revision, range.clone(), inserted_len)
        };
        let memory = Edit::charge(&change);
        let edit = Edit {
            change,
            before_revision: group_before,
            after_revision: fresh_revision(),
            envelope,
            inserted_len: group_inserted,
            memory,
        };
        let forward_bytes = Self::single_edit_history_bytes(&after, &edit);
        let backward_content = match &edit.change {
            Change::Snapshot { before, .. } => before.clone(),
            Change::Splice {
                offset,
                removed,
                inserted,
            } => {
                let length: usize = inserted.iter().map(Piece::len).sum();
                Self::spliced_content(&after, *offset..*offset + length, removed)
            }
        };
        let backward_bytes = Self::single_edit_history_bytes(&backward_content, &edit);
        if forward_bytes.max(backward_bytes) > self.history_byte_limit {
            return Err(format!(
                "This operation needs more than the {} byte undo/redo budget. Make a smaller edit or replacement; the document and existing history were not changed.",
                self.history_byte_limit,
            ));
        }
        drop(backward_content);
        self.set_content(after);
        for old in &self.redo {
            self.usage.edit(old, false);
        }
        self.redo.clear();
        if can_join {
            let previous = self.undo.pop_back().unwrap();
            self.usage.edit(&previous, false);
        }
        self.revision = edit.after_revision;
        self.usage.edit(&edit, true);
        self.undo.push_back(edit);
        self.log_change(before_revision, range, inserted_len);
        self.trim_history();
        debug_assert_eq!(
            self.undo.back().map(|edit| edit.after_revision),
            Some(self.revision)
        );
        Ok(())
    }

    pub fn undo(&mut self) -> bool {
        let Some(edit) = self.undo.pop_back() else {
            return false;
        };
        let before_revision = self.revision;
        match &edit.change {
            Change::Splice {
                offset,
                removed,
                inserted,
            } => {
                let inserted_len: usize = inserted.iter().map(Piece::len).sum();
                self.splice(*offset..*offset + inserted_len, removed);
            }
            Change::Snapshot { before, .. } => self.set_content(before.clone()),
        }
        self.revision = edit.before_revision;
        self.log_change(
            before_revision,
            edit.envelope.start..edit.envelope.start + edit.inserted_len,
            edit.envelope.len(),
        );
        self.redo.push(edit);
        self.trim_history();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(edit) = self.redo.pop() else {
            return false;
        };
        let before_revision = self.revision;
        match &edit.change {
            Change::Splice {
                offset,
                removed,
                inserted,
            } => {
                let removed_len: usize = removed.iter().map(Piece::len).sum();
                self.splice(*offset..*offset + removed_len, inserted);
            }
            Change::Snapshot { after, .. } => self.set_content(after.clone()),
        }
        self.revision = edit.after_revision;
        self.log_change(before_revision, edit.envelope.clone(), edit.inserted_len);
        self.undo.push_back(edit);
        self.trim_history();
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
        self.try_apply_snapshot(snapshot).unwrap_or(false)
    }

    /// Fallible adoption is required for UI callers so a rejected newest undo
    /// transaction is reported rather than confused with an unchanged snapshot.
    pub fn try_apply_snapshot(&mut self, snapshot: &Document) -> Result<bool, String> {
        if Arc::ptr_eq(&self.content, &snapshot.content) {
            return Ok(false);
        }
        let _reservation = reserve_memory(
            (self.content.pieces.len() + snapshot.content.pieces.len()).saturating_mul(2048),
        )?;
        let (range, inserted_len) = match snapshot.changes_since(self.revision) {
            Some(changes) if changes.len() == 1 => {
                let change = changes[0];
                (
                    change.changed_from..change.old_end,
                    change.new_end - change.changed_from,
                )
            }
            _ => (0..self.len(), snapshot.len()),
        };
        let after = snapshot.content.clone();
        let before = self.content.clone();
        self.commit_change(
            after.clone(),
            Change::Snapshot { before, after },
            range,
            inserted_len,
            false,
        )?;
        Ok(true)
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
    #[test]
    fn history_budget_bounds_unique_retention_in_both_directions() {
        let size = 64 * 1024;
        let mut document = Document::from_bytes(vec![b'a'; size]).unwrap();
        document.set_history_byte_limit(3 * size);
        for index in 0..40 {
            let replacement = if index % 2 == 0 { "b" } else { "a" }.repeat(size);
            document.replace(0..size, &replacement).unwrap();
            assert!(document.history_bytes() <= 3 * size);
            assert_eq!(document.len(), size);
        }
        assert!(document.history_evictions() > 0);
        let mut undos = 0;
        while document.undo() {
            undos += 1;
            assert!(document.history_bytes() <= 3 * size);
        }
        assert_eq!(undos, 2, "two 64 KiB versions plus metadata fit the budget");
        for _ in 0..undos {
            assert!(document.redo());
            assert!(document.history_bytes() <= 3 * size);
        }
        assert!(!document.redo());
        assert_eq!(document.read(0..1), b"a");
    }

    #[test]
    fn oversized_newest_transaction_rejects_atomically_and_preserves_history() {
        let original = "a".repeat(CHUNK_SIZE);
        let mut document = Document::from_bytes(original.as_bytes().to_vec()).unwrap();
        document.set_history_byte_limit(16 * 1024);
        document
            .replace(document.len()..document.len(), " prior")
            .unwrap();
        let snapshot = document.snapshot();
        let history_bytes = document.history_bytes();
        let evictions = document.history_evictions();
        let error = document.replace(0..document.len(), "new text").unwrap_err();
        assert!(error.contains("undo/redo budget"));
        assert!(Arc::ptr_eq(&document.content, &snapshot.content));
        assert_eq!(document.revision(), snapshot.revision());
        assert_eq!(document.history_bytes(), history_bytes);
        assert_eq!(document.history_evictions(), evictions);
        assert!(document.undo());
        assert_eq!(text(&document), original);
        assert!(document.redo());
        assert_eq!(text(&document), format!("{original} prior"));
    }

    #[test]
    fn accounting_counts_whole_backing_and_deduplicates_shared_allocations() {
        let backing: Arc<[u8]> = Arc::from(vec![b'a'; CHUNK_SIZE]);
        let mut document = Document::from_chunks(vec![backing.clone(); 4]).unwrap();
        let same_backing = Document::from_chunks(vec![backing]).unwrap();
        let charge = &document.content.pieces[0].bytes_memory;
        assert!(Arc::ptr_eq(
            charge,
            &same_backing.content.pieces[0].bytes_memory
        ));
        assert!(document.retained_storage_bytes() < CHUNK_SIZE * 2);
        let snapshot = document.snapshot();
        let weak_content = Arc::downgrade(&snapshot.content);
        let weak_backing = Arc::downgrade(&snapshot.content.pieces[0].bytes_memory);
        document.replace(1..document.len(), "").unwrap();
        document.set_history_byte_limit(0);
        assert_eq!(document.len(), 1);
        assert!(document.retained_storage_bytes() >= CHUNK_SIZE);
        drop(document);
        drop(same_backing);
        assert!(weak_content.upgrade().is_some());
        assert!(
            weak_backing.upgrade().is_some(),
            "outstanding snapshot keeps its allocation charged"
        );
        drop(snapshot);
        assert!(weak_content.upgrade().is_none());
        assert!(weak_backing.upgrade().is_none());
    }

    #[test]
    fn shared_baseline_is_not_charged_once_per_history_action() {
        let mut document = Document::from_bytes(vec![b'a'; 4 * CHUNK_SIZE]).unwrap();
        document.set_history_byte_limit(256 * 1024);
        for _ in 0..100 {
            document.replace(500..500, "x").unwrap();
        }
        assert_eq!(document.history_evictions(), 0);
        assert!(document.history_bytes() < 256 * 1024);
        assert!(document.retained_storage_bytes() < 5 * CHUNK_SIZE);
        for _ in 0..100 {
            assert!(document.undo());
        }
    }

    #[test]
    fn snapshot_history_counts_metadata_and_shared_before_after_once() {
        let mut document = Document::from_bytes(vec![b'a'; 4 * CHUNK_SIZE]).unwrap();
        document.set_history_byte_limit(256 * 1024);
        for _ in 0..20 {
            document.replace_ranges(&[100..100, 200..200], "x").unwrap();
        }
        assert_eq!(document.history_evictions(), 0);
        assert!(
            document.history_bytes() > 10_000,
            "retained Content and piece metadata must count"
        );
        assert!(
            document.history_bytes() < 256 * 1024,
            "unchanged megabyte backing is shared"
        );
        for _ in 0..20 {
            assert!(document.undo());
        }
    }

    #[test]
    fn grouped_typing_keeps_one_undo_and_does_not_cross_saved_boundary() {
        let mut document = Document::new();
        let original = document.revision();
        document.replace_grouped(0..0, "é", false).unwrap();
        document.replace_grouped(2..2, "🙂", true).unwrap();
        assert_eq!(document.undo_group_revision(), Some(original));
        assert!(document.undo());
        assert!(document.is_empty());
        assert!(document.redo());
        assert_eq!(text(&document), "é🙂");
        document.mark_saved();
        document.replace_grouped(6..6, "x", true).unwrap();
        assert!(document.undo());
        assert_eq!(text(&document), "é🙂");
        assert!(!document.is_modified());
        assert!(document.undo());
        assert!(document.is_empty());
    }

    #[test]
    fn change_log_tracks_immediate_revisions_groups_bulk_undo_and_snapshot() {
        let mut document = Document::from_bytes(b"abcdef".to_vec()).unwrap();
        let original = document.revision();
        document.replace_grouped(2..2, "x", false).unwrap();
        let first = document.revision();
        document.replace_grouped(3..3, "é", true).unwrap();
        let changes = document.changes_since(original).unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[1].before_revision, first);
        assert_eq!(
            (
                changes[1].changed_from,
                changes[1].old_end,
                changes[1].new_end
            ),
            (3, 3, 5)
        );
        let typed = document.revision();
        assert!(document.undo());
        let undo = document.changes_since(typed).unwrap();
        assert_eq!(
            (undo[0].changed_from, undo[0].old_end, undo[0].new_end),
            (2, 5, 2)
        );
        let mut prepared = document.snapshot();
        prepared.replace_ranges(&[1..2, 4..5], "🙂").unwrap();
        assert!(document.apply_snapshot(&prepared));
        let change = document.last_change().unwrap();
        assert_eq!(
            (change.changed_from, change.old_end, change.new_end),
            (1, 5, 11)
        );
        for _ in 0..CHANGE_LOG_LIMIT + 1 {
            document.replace(0..0, "x").unwrap();
        }
        assert!(document.changes_since(original).is_none());
        assert_eq!(
            document.changes_since(document.revision()),
            Some(Vec::new())
        );
    }

    #[test]
    fn unicode_scalar_navigation_uses_counts_and_survives_slices() {
        let reference = "aé🙂日".repeat(CHUNK_SIZE / 2);
        let mut document = Document::from_bytes(reference.clone().into_bytes()).unwrap();
        for scalar in [0, 1, 2, 3, 255, 256, 100_000, reference.chars().count()] {
            let expected = reference
                .char_indices()
                .nth(scalar)
                .map_or(reference.len(), |(at, _)| at);
            assert_eq!(document.offset_at_scalar(0, scalar), expected);
            assert_eq!(document.scalar_count(0..expected), scalar);
        }
        document.replace(1..3, "").unwrap();
        let expected = format!("a{}", &reference[3..]);
        for scalar in [0, 1, 2, 256, 70_000] {
            let at = expected.char_indices().nth(scalar).unwrap().0;
            assert_eq!(document.offset_at_scalar(0, scalar), at);
            assert_eq!(document.scalar_count(0..at), scalar);
        }
        assert!(document.undo());
        assert_eq!(
            document.scalar_count(0..document.len()),
            reference.chars().count()
        );
    }

    #[test]
    fn tracked_memory_admission_rejects_without_reserving_or_mutating() {
        let document = Document::from_bytes(b"unchanged".to_vec()).unwrap();
        assert!(reserve_memory(DOCUMENT_MEMORY_LIMIT + 1).is_err());
        assert_eq!(text(&document), "unchanged");
        assert!(!document.is_modified());
    }

    #[test]
    fn sparse_large_document_edits_reserve_metadata_not_shared_text() {
        let gib = 1024 * 1024 * 1024;
        let pieces = gib / CHUNK_SIZE + 4;
        let sparse = bulk_allocation_bytes(gib + 2, 1, pieces);
        assert!(
            sparse < 16 * 1024 * 1024,
            "sparse Tab/replace on 1 GiB must not reserve another GiB"
        );
        let compact = bulk_allocation_bytes(gib, 1, BULK_COMPACT_PIECES + 1);
        assert!(compact >= gib);
    }

    #[test]
    fn raw_load_reservation_is_adopted_without_double_reserving() {
        let bytes: Arc<[u8]> = Arc::from(b"reserved load".as_slice());
        let reservation = reserve_load_memory(bytes.len()).unwrap();
        let document = Document::from_chunks_cancellable_reserved(
            vec![bytes],
            &AtomicBool::new(false),
            reservation,
        )
        .unwrap();
        assert_eq!(text(&document), "reserved load");
        assert_eq!(document.history_bytes(), 0);
    }

    #[test]
    fn large_insertion_and_bulk_expansion_must_fit_the_redo_endpoint() {
        let mut document = Document::from_bytes(b"a".to_vec()).unwrap();
        document.set_history_byte_limit(16 * 1024);
        let revision = document.revision();
        let insertion = "b".repeat(64 * 1024);
        assert!(document.replace(1..1, &insertion).is_err());
        assert!(document.replace_ranges(&[0..1], &insertion).is_err());
        assert_eq!(document.revision(), revision);
        assert_eq!(text(&document), "a");
        assert!(!document.undo());
        assert_eq!(document.history_evictions(), 0);
    }

    #[test]
    fn grouped_typing_refusal_preserves_previous_group_and_redo() {
        let mut document = Document::new();
        document.set_history_byte_limit(32 * 1024);
        let first = "a".repeat(8 * 1024);
        document.replace_grouped(0..0, &first, false).unwrap();
        let revision = document.revision();
        assert!(
            document
                .replace_grouped(first.len()..first.len(), &"b".repeat(32 * 1024), true)
                .is_err()
        );
        assert_eq!(document.revision(), revision);
        assert_eq!(text(&document), first);
        assert!(document.undo());
        assert!(document.is_empty());
        assert!(document.replace(0..0, &"c".repeat(64 * 1024)).is_err());
        assert!(
            document.redo(),
            "failed new edit must not clear existing redo"
        );
        assert_eq!(text(&document), first);
    }

    #[test]
    fn prepared_snapshot_respects_receivers_history_budget_atomically() {
        let mut document = Document::from_bytes(b"original".to_vec()).unwrap();
        let mut prepared = document.snapshot();
        prepared
            .replace(0..prepared.len(), &"x".repeat(64 * 1024))
            .unwrap();
        document.set_history_byte_limit(16 * 1024);
        let revision = document.revision();
        assert!(document.try_apply_snapshot(&prepared).is_err());
        assert_eq!(document.revision(), revision);
        assert_eq!(text(&document), "original");
        assert!(!document.undo());
        assert_eq!(document.history_evictions(), 0);
    }
}
