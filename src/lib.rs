//! Plain-text document model for the standalone notepad prototype.
//! Text positions are UTF-8 byte offsets; displayed line/column positions are 1-based.
pub mod preferences;

use std::{
    collections::VecDeque,
    fs::{self, File},
    io::{self, Read, Write},
    ops::Range,
    path::{Path, PathBuf},
};

pub const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
const HISTORY_BYTES: usize = 32 * 1024 * 1024;
const HISTORY_ENTRIES: usize = 100;
const BOM: &[u8] = b"\xef\xbb\xbf";

#[derive(Debug, Default)]
pub struct Document {
    text: String,
    saved_text: String,
    path: Option<PathBuf>,
    disk_snapshot: Option<Vec<u8>>,
    bom: bool,
    undo: VecDeque<String>,
    redo: VecDeque<String>,
}

impl Document {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        // Resolve symlinks so saving edits the target, not the link itself.
        let path = fs::canonicalize(path)?;
        let bytes = read_limited(&path)?;
        let bom = bytes.starts_with(BOM);
        let payload = if bom { &bytes[BOM.len()..] } else { &bytes };
        let text = std::str::from_utf8(payload).map_err(|_| io::Error::new(io::ErrorKind::InvalidData,
            "This prototype opens UTF-8 text only. Convert this file to UTF-8 in another editor first."))?.to_owned();
        Ok(Self {
            saved_text: text.clone(),
            text,
            path: Some(path),
            disk_snapshot: Some(bytes),
            bom,
            ..Self::default()
        })
    }
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub fn title(&self) -> String {
        self.path
            .as_deref()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into())
    }
    /// Describes actual contents without silently normalizing mixed newlines.
    pub fn line_ending_label(&self) -> &'static str {
        let bytes = self.text.as_bytes();
        let mut crlf = false;
        let mut lf = false;
        let mut cr = false;
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'\r' {
                if bytes.get(i + 1) == Some(&b'\n') {
                    crlf = true;
                } else {
                    cr = true;
                }
            } else if b == b'\n' && (i == 0 || bytes[i - 1] != b'\r') {
                lf = true;
            }
        }
        match (crlf, lf, cr) {
            (true, false, false) => "Windows (CRLF)",
            (false, true, false) => "Unix (LF)",
            (false, false, true) => "Mac (CR)",
            (false, false, false) => "Unix (LF)",
            _ => "Mixed",
        }
    }
    pub fn is_dirty(&self) -> bool {
        self.text != self.saved_text
    }
    pub fn has_bom(&self) -> bool {
        self.bom
    }
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn set_text(&mut self, text: String) {
        if text == self.text {
            return;
        }
        self.undo.push_back(std::mem::replace(&mut self.text, text));
        self.redo.clear();
        self.trim_history();
    }
    pub fn undo(&mut self) -> bool {
        if let Some(previous) = self.undo.pop_back() {
            self.redo
                .push_back(std::mem::replace(&mut self.text, previous));
            self.trim_history();
            true
        } else {
            false
        }
    }
    pub fn redo(&mut self) -> bool {
        if let Some(next) = self.redo.pop_back() {
            self.undo.push_back(std::mem::replace(&mut self.text, next));
            self.trim_history();
            true
        } else {
            false
        }
    }
    fn trim_history(&mut self) {
        let mut size: usize = self
            .undo
            .iter()
            .chain(self.redo.iter())
            .map(String::len)
            .sum();
        while size > HISTORY_BYTES || self.undo.len() + self.redo.len() > HISTORY_ENTRIES {
            let removed = if !self.undo.is_empty() {
                self.undo.pop_front()
            } else {
                self.redo.pop_front()
            };
            if let Some(s) = removed {
                size -= s.len();
            } else {
                break;
            }
        }
    }
    pub fn save(&mut self) -> io::Result<()> {
        let path = self.path.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Choose a filename with Save As first.",
            )
        })?;
        self.save_to(&path)
    }
    /// The caller must confirm overwriting an existing, different destination.
    pub fn save_as(&mut self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        let path = if path.exists() {
            fs::canonicalize(path)?
        } else if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        self.save_to(&path)
    }
    fn save_to(&mut self, path: &Path) -> io::Result<()> {
        let length = self.text.len() + if self.bom { BOM.len() } else { 0 };
        if length > MAX_FILE_BYTES {
            return Err(size_error());
        }
        let mut bytes = Vec::with_capacity(length);
        if self.bom {
            bytes.extend_from_slice(BOM);
        }
        bytes.extend_from_slice(self.text.as_bytes());
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        if let Ok(meta) = fs::metadata(path) {
            if meta.permissions().readonly() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "The file is read-only. Choose a different filename with Save As.",
                ));
            }
            temp.as_file().set_permissions(meta.permissions())?;
        }
        temp.write_all(&bytes)?;
        temp.as_file().sync_all()?;
        // Best-effort conflict detection immediately before atomic replacement.
        // This is not a cross-process locking protocol.
        if self.path.as_deref() == Some(path) {
            let current = read_limited(path).map_err(|err| io::Error::new(err.kind(), format!("Could not verify the original file before saving: {err}. Use Save As to preserve your edits.")))?;
            if self.disk_snapshot.as_deref() != Some(current.as_slice()) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "This file changed outside the editor. Your edits have not been saved. Use Save As to preserve them, or reopen the file.",
                ));
            }
        }
        temp.persist(path).map_err(|error| error.error)?;
        self.path = Some(path.to_owned());
        self.saved_text = self.text.clone();
        self.disk_snapshot = Some(bytes);
        Ok(())
    }
    /// Finds the next occurrence at/after start, wrapping once. Empty queries never match.
    /// Case-insensitive matching uses Unicode lowercase, not locale-specific collation.
    pub fn find(&self, query: &str, start_byte: usize, match_case: bool) -> Option<Range<usize>> {
        let ranges = matches(&self.text, query, match_case);
        ranges
            .iter()
            .find(|r| r.start >= start_byte)
            .or_else(|| ranges.first())
            .cloned()
    }
    pub fn replace_one(
        &mut self,
        query: &str,
        replacement: &str,
        start_byte: usize,
        match_case: bool,
    ) -> Option<Range<usize>> {
        let range = self.find(query, start_byte, match_case)?;
        let mut text = self.text.clone();
        text.replace_range(range.clone(), replacement);
        let result = range.start..range.start + replacement.len();
        self.set_text(text);
        Some(result)
    }
    pub fn replace_all(&mut self, query: &str, replacement: &str, match_case: bool) -> usize {
        let ranges = matches(&self.text, query, match_case);
        if ranges.is_empty() {
            return 0;
        }
        let mut text = String::new();
        let mut last = 0;
        for range in &ranges {
            text.push_str(&self.text[last..range.start]);
            text.push_str(replacement);
            last = range.end;
        }
        text.push_str(&self.text[last..]);
        self.set_text(text);
        ranges.len()
    }
    /// Counts Unicode scalar values, not grapheme clusters.
    pub fn char_count(&self) -> usize {
        self.text.chars().count()
    }
    /// CRLF is one newline. Lone CR and LF are also newlines. Columns count scalars.
    pub fn line_col(&self, byte: usize) -> (usize, usize) {
        let mut end = byte.min(self.text.len());
        while !self.text.is_char_boundary(end) {
            end -= 1;
        }
        let mut line = 1;
        let mut col = 1;
        let mut previous_cr = false;
        for c in self.text[..end].chars() {
            if c == '\r' {
                line += 1;
                col = 1;
            } else if c == '\n' {
                if !previous_cr {
                    line += 1;
                }
                col = 1;
            } else {
                col += 1;
            }
            previous_cr = c == '\r';
        }
        (line, col)
    }
}

fn size_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "This prototype supports text files up to 16 MiB. Use another editor for larger files.",
    )
}
fn read_limited(path: &Path) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    if file.metadata()?.len() > MAX_FILE_BYTES as u64 {
        return Err(size_error());
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(size_error());
    }
    Ok(bytes)
}
fn matches(text: &str, query: &str, match_case: bool) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    if match_case {
        return text
            .match_indices(query)
            .map(|(i, s)| i..i + s.len())
            .collect();
    }
    let mut folded = String::new();
    let mut boundaries = Vec::new();
    for (index, c) in text.char_indices() {
        boundaries.push((folded.len(), index));
        folded.extend(c.to_lowercase());
    }
    boundaries.push((folded.len(), text.len()));
    let query: String = query.chars().flat_map(char::to_lowercase).collect();
    folded
        .match_indices(&query)
        .filter_map(|(start, matched)| {
            let a = boundaries
                .binary_search_by_key(&start, |&(fold, _)| fold)
                .ok()?;
            let b = boundaries
                .binary_search_by_key(&(start + matched.len()), |&(fold, _)| fold)
                .ok()?;
            Some(boundaries[a].1..boundaries[b].1)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn labels_reflect_actual_line_endings() {
        let mut d = Document::new();
        assert_eq!(d.title(), "Untitled");
        for (text, label) in [
            ("a\r\nb", "Windows (CRLF)"),
            ("a\nb", "Unix (LF)"),
            ("a\rb", "Mac (CR)"),
            ("a\r\nb\nc", "Mixed"),
        ] {
            d.set_text(text.into());
            assert_eq!(d.line_ending_label(), label);
        }
    }
    #[test]
    fn history_and_dirty_follow_saved_snapshot() {
        let mut d = Document::new();
        assert!(!d.is_dirty());
        d.set_text("hello".into());
        assert!(d.is_dirty());
        assert!(d.undo());
        assert!(!d.is_dirty());
        assert!(d.redo());
        d.set_text("other".into());
        assert!(!d.can_redo());
        assert!(d.undo());
        assert_eq!(d.text(), "hello");
    }
    #[test]
    fn bom_and_mixed_line_endings_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("note.txt");
        let bytes = b"\xef\xbb\xbfhello\r\nworld\nlast\rend";
        fs::write(&p, bytes).unwrap();
        let mut d = Document::open(&p).unwrap();
        assert!(d.has_bom());
        assert!(!d.text().starts_with('\u{feff}'));
        d.save().unwrap();
        assert_eq!(fs::read(&p).unwrap(), bytes);
        d.set_text(format!("{}!", d.text()));
        d.save().unwrap();
        assert!(!d.is_dirty());
        assert!(d.undo());
        assert!(d.is_dirty());
    }
    #[test]
    fn external_change_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("note.txt");
        fs::write(&p, "original").unwrap();
        let mut d = Document::open(&p).unwrap();
        d.set_text("my edits".into());
        fs::write(&p, "outside edit").unwrap();
        assert!(d.save().is_err());
        assert_eq!(fs::read_to_string(&p).unwrap(), "outside edit");
        assert!(d.is_dirty());
        let copy = dir.path().join("copy.txt");
        d.save_as(&copy).unwrap();
        assert_eq!(fs::read_to_string(copy).unwrap(), "my edits");
    }
    #[test]
    fn bad_encoding_and_large_files_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bad.txt");
        fs::write(&p, [0xff, 0xfe]).unwrap();
        assert_eq!(
            Document::open(&p).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        File::create(&p)
            .unwrap()
            .set_len(MAX_FILE_BYTES as u64 + 1)
            .unwrap();
        assert!(Document::open(&p).is_err());
    }
    #[test]
    fn unicode_find_wrap_and_replace() {
        let mut d = Document::new();
        d.set_text("Élan 🦀 éLAN İstanbul".into());
        assert_eq!(d.find("élan", 0, false), Some(0..5));
        assert_eq!(d.find("élan", 5, false), Some(11..16));
        assert_eq!(d.find("élan", 17, false), Some(0..5));
        assert_eq!(d.find("élan", 0, true), None);
        assert_eq!(d.find("", 0, false), None);
        assert_eq!(d.replace_all("élan", "fox", false), 2);
        assert_eq!(d.text(), "fox 🦀 fox İstanbul");
        assert!(d.undo());
        assert_eq!(d.replace_one("🦀", "x", 2, true), Some(6..7));
        assert_eq!(d.text(), "Élan x éLAN İstanbul");
    }
    #[test]
    fn expansion_matches_only_whole_characters() {
        let mut d = Document::new();
        d.set_text("İ i".into());
        assert_eq!(d.find("i", 0, false), Some(3..4));
        assert_eq!(d.find("i\u{307}", 0, false), Some(0..2));
    }
    #[test]
    fn cursor_counts_unicode_and_crlf() {
        let mut d = Document::new();
        d.set_text("é🦀\r\nx\ry\nz".into());
        assert_eq!(d.char_count(), 9);
        assert_eq!(d.line_col(1), (1, 1));
        assert_eq!(d.line_col(6), (1, 3));
        assert_eq!(d.line_col(8), (2, 1));
        assert_eq!(d.line_col(usize::MAX), (4, 2));
    }
    #[test]
    fn history_is_bounded_and_replace_all_is_one_undo() {
        let mut d = Document::new();
        for n in 0..200 {
            d.set_text(n.to_string());
        }
        let mut count = 0;
        while d.undo() {
            count += 1;
        }
        assert_eq!(count, HISTORY_ENTRIES);
        d.set_text("a a a".into());
        assert_eq!(d.replace_all("a", "aa", true), 3);
        d.undo();
        assert_eq!(d.text(), "a a a");
    }
    #[test]
    fn new_document_save_requires_filename() {
        let mut d = Document::new();
        assert_eq!(d.save().unwrap_err().kind(), io::ErrorKind::InvalidInput);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("new.txt");
        d.set_text("hello".into());
        d.save_as(&p).unwrap();
        assert!(!d.is_dirty());
        assert_eq!(fs::read(&p).unwrap(), b"hello");
    }
}
