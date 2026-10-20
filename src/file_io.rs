//! Bounded UTF-8 loading and failure-safe, byte-preserving file replacement.
//!
//! Saving does not modify the original until a complete temporary file has
//! been flushed. A save with no expected stamp is **create-new only**. To
//! overwrite a Save As destination, obtain the user's confirmation and pass
//! that destination's current stamp, not the open document's stamp.
//!
//! Metadata stamps detect ordinary external edits, replacement, and deletion;
//! they are not content hashes or an interprocess lock. There remains a narrow
//! check/rename race with an uncooperative writer. Local filesystem rename and
//! durability guarantees vary. Unix parent-directory fsync is attempted;
//! Windows relies on the atomic replacement supplied by `tempfile`. Network
//! filesystems are outside the supported durability assumptions.
//!
//! The named file must not be a symbolic link. Ancestor directories may be
//! symbolic links (including system paths such as macOS's /var); concurrent
//! changes to path components cannot be ruled out without platform-specific
//! directory-handle APIs. This module is not a security boundary against an
//! adversary modifying the filesystem.

use crate::document::Document;
use std::fs::{self, File, Metadata, Permissions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;
use tempfile::{Builder, NamedTempFile};

/// Maximum uncancellable read/write work between cancellation checks.
pub const IO_CHUNK_BYTES: usize = 256 * 1024;
/// A preview never reads more than this amount, even for a larger request.
pub const MAX_PREVIEW_BYTES: usize = 1024 * 1024;
const CANCELLED: &str = "Operation cancelled; the original file was not changed.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStamp {
    pub len: u64,
    pub modified: Option<SystemTime>,
    /// Unix device/inode identity. Other platforms still compare timestamps,
    /// size, creation time, and read-only state.
    pub identity: Option<(u64, u64)>,
    created: Option<SystemTime>,
    readonly: bool,
    #[cfg(unix)]
    changed: (i64, i64),
    #[cfg(unix)]
    mode: u32,
}

impl FileStamp {
    /// Inspect an existing regular file without accepting a symbolic link.
    pub fn read(path: &Path) -> Result<Self, String> {
        regular_metadata(path).map(|metadata| Self::from_metadata(&metadata))
    }

    pub fn matches_current(&self, path: &Path) -> Result<bool, String> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                require_regular(path, &metadata)?;
                Ok(*self == Self::from_metadata(&metadata))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!("Cannot inspect {}: {error}", path.display())),
        }
    }

    fn from_metadata(metadata: &Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            identity: Some((metadata.dev(), metadata.ino())),
            #[cfg(not(unix))]
            identity: None,
            created: metadata.created().ok(),
            readonly: metadata.permissions().readonly(),
            #[cfg(unix)]
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            #[cfg(unix)]
            mode: metadata.mode(),
        }
    }
}

pub struct LoadedFile {
    pub document: Document,
    pub path: PathBuf,
    pub stamp: FileStamp,
}

fn require_regular(path: &Path, metadata: &Metadata) -> Result<(), String> {
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "{} is a symbolic link. Open the intended target explicitly instead.",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!(
            "{} is not a regular file; devices, directories, and pipes are unsupported.",
            path.display()
        ));
    }
    Ok(())
}

fn regular_metadata(path: &Path) -> Result<Metadata, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Cannot inspect {}: {error}", path.display()))?;
    require_regular(path, &metadata)?;
    Ok(metadata)
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err(CANCELLED.to_owned())
    } else {
        Ok(())
    }
}

fn open_checked(path: &Path) -> Result<(File, FileStamp), String> {
    let stamp = FileStamp::read(path)?;
    let file =
        File::open(path).map_err(|error| format!("Cannot open {}: {error}", path.display()))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("Cannot inspect opened file {}: {error}", path.display()))?;
    if FileStamp::from_metadata(&opened) != stamp || !stamp.matches_current(path)? {
        return Err(format!(
            "{} changed while it was being opened. Try opening it again.",
            path.display()
        ));
    }
    Ok((file, stamp))
}

/// Read a small, explicitly read-only, potentially lossy preview.
///
/// `max_bytes` bounds input bytes, not UTF-8 replacement expansion or the
/// explanatory label. This preview must never become the editable document.
/// It is deliberately independent of complete-file validation and indexing.
pub fn preview(path: &Path, max_bytes: usize) -> Result<String, String> {
    let (file, stamp) = open_checked(path)?;
    let limit = max_bytes.min(MAX_PREVIEW_BYTES);
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(limit)
        .map_err(|error| format!("Not enough memory for a preview: {error}"))?;
    file.take(limit as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Cannot preview {}: {error}", path.display()))?;
    let truncated = stamp.len > bytes.len() as u64;
    // Drop a trailing partial character only if this is a bounded prefix.
    // Real malformed input remains visible as replacement characters.
    if truncated
        && let Err(error) = std::str::from_utf8(&bytes)
        && error.error_len().is_none()
    {
        bytes.truncate(error.valid_up_to());
    }
    let mut text = String::from(
        "[Read-only preview: not yet validated or editable; unsupported bytes may appear as \u{fffd}.]\n\n",
    );
    // NUL is made visible instead of passing binary control data to the UI.
    text.push_str(&String::from_utf8_lossy(&bytes).replace('\0', "\u{2400}"));
    if truncated {
        text.push_str("\n\n[Preview ends here; the file continues.]");
    }
    Ok(text)
}

/// Load without normalizing BOMs, newlines, or final-newline presence.
///
/// Invalid UTF-8 and NUL-containing files are rejected for editing. The caller
/// may retain the separately obtained read-only preview on that error.
pub fn load(path: &Path, cancel: &AtomicBool, progress: &AtomicU64) -> Result<LoadedFile, String> {
    progress.store(0, Ordering::Relaxed);
    check_cancel(cancel)?;
    let (mut file, stamp) = open_checked(path)?;
    if stamp.len > crate::document::MAX_DOCUMENT_BYTES as u64 {
        return Err(format!(
            "The file exceeds the 2 GiB editable-document safety limit: {}. It remains a read-only preview.",
            path.display()
        ));
    }
    let mut chunks = Vec::new();
    let mut carry = [0_u8; 3];
    let mut carry_len = 0;
    loop {
        check_cancel(cancel)?;
        let mut buffer = vec![0_u8; IO_CHUNK_BYTES];
        buffer[..carry_len].copy_from_slice(&carry[..carry_len]);
        let count = file
            .read(&mut buffer[carry_len..])
            .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
        if count == 0 {
            if carry_len != 0 {
                return Err(unsupported_encoding(
                    path,
                    "incomplete UTF-8 at end of file",
                ));
            }
            break;
        }
        progress.fetch_add(count as u64, Ordering::Relaxed);
        check_cancel(cancel)?;
        if progress.load(Ordering::Relaxed) > stamp.len {
            return Err(format!(
                "{} grew during loading. Reopen it after the other writer has finished.",
                path.display()
            ));
        }
        buffer.truncate(carry_len + count);
        if memchr::memchr(0, &buffer).is_some() {
            return Err(unsupported_encoding(
                path,
                "NUL bytes (binary or unsupported encoding)",
            ));
        }
        let valid_len = match std::str::from_utf8(&buffer) {
            Ok(_) => buffer.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => return Err(unsupported_encoding(path, "invalid UTF-8")),
        };
        carry_len = buffer.len() - valid_len;
        debug_assert!(carry_len <= carry.len());
        carry[..carry_len].copy_from_slice(&buffer[valid_len..]);
        buffer.truncate(valid_len);
        if !buffer.is_empty() {
            chunks
                .try_reserve(1)
                .map_err(|error| format!("Not enough memory to load the file: {error}"))?;
            chunks.push(Arc::<[u8]>::from(buffer));
        }
    }
    let end_metadata = file
        .metadata()
        .map_err(|error| format!("Cannot verify {} after loading: {error}", path.display()))?;
    if FileStamp::from_metadata(&end_metadata) != stamp || !stamp.matches_current(path)? {
        return Err(format!(
            "{} changed during loading. The partially loaded document was discarded; reopen the file.",
            path.display()
        ));
    }
    check_cancel(cancel)?;
    let document = Document::from_chunks_cancellable(chunks, cancel)?;
    check_cancel(cancel)?;
    Ok(LoadedFile {
        document,
        path: path.to_path_buf(),
        stamp,
    })
}

fn unsupported_encoding(path: &Path, reason: &str) -> String {
    format!(
        "{} cannot be edited safely: {reason}. The preview is read-only; no bytes were converted or changed.",
        path.display()
    )
}

/// Atomically save a stable document snapshot.
///
/// `Some(stamp)` means the caller authorizes replacing exactly that observed
/// file. `None` means create a new file and refuse any existing destination.
/// The caller is responsible for the user's overwrite confirmation for Save
/// As. The short rename/directory-flush commit cannot safely be cancelled.
pub fn save(
    doc: &Document,
    path: &Path,
    expected: Option<&FileStamp>,
    cancel: &AtomicBool,
    progress: &AtomicU64,
) -> Result<FileStamp, String> {
    save_with_writer(doc, path, expected, cancel, progress, write_document)
}

fn save_with_writer(
    doc: &Document,
    path: &Path,
    expected: Option<&FileStamp>,
    cancel: &AtomicBool,
    progress: &AtomicU64,
    write: impl FnOnce(&Document, &mut File, &AtomicBool, &AtomicU64) -> Result<(), String>,
) -> Result<FileStamp, String> {
    progress.store(0, Ordering::Relaxed);
    check_cancel(cancel)?;
    let permissions = validate_destination(path, expected)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = Builder::new()
        .prefix(".simptxt-save-")
        .tempfile_in(parent)
        .map_err(|error| {
            format!(
                "Cannot create a temporary file beside {}: {error}. The original was not changed.",
                path.display()
            )
        })?;
    write(doc, temporary.as_file_mut(), cancel, progress)?;
    check_cancel(cancel)?;
    let written_len = temporary
        .as_file()
        .metadata()
        .map_err(|error| format!("Cannot verify the temporary file length: {error}"))?
        .len();
    if written_len != doc.len() as u64 {
        return Err(format!(
            "Incomplete temporary file: wrote {written_len} bytes, expected {}. \
             The original was not changed.",
            doc.len()
        ));
    }
    if let Some(permissions) = permissions {
        temporary
            .as_file()
            .set_permissions(permissions)
            .map_err(|error| {
                format!("Cannot preserve file permissions: {error}. The original was not changed.")
            })?;
    }
    temporary.as_file().sync_all().map_err(|error| {
        format!("Cannot flush the complete temporary file: {error}. The original was not changed.")
    })?;
    check_cancel(cancel)?;
    // Check immediately before commit, after all writing and flushing. A
    // cooperating editor should still avoid concurrent saves of the same file.
    validate_destination(path, expected)?;
    // From here onward cancellation is intentionally ignored. The original
    // remains intact until the platform's atomic replace succeeds. tempfile's
    // persist operation replaces existing files on Windows as well as Unix.
    let committed = commit(temporary, path, expected.is_some())?;
    #[cfg(unix)]
    {
        let result = File::open(parent).and_then(|directory| directory.sync_all());
        if let Err(error) = result {
            return Err(format!(
                "The file was replaced, but its directory could not be synced: {error}. \
                 Crash durability is uncertain. Keep the document open and verify {} before retrying.",
                path.display()
            ));
        }
    }
    let metadata = committed.metadata().map_err(|error| {
        format!(
            "The file was replaced, but its saved metadata could not be read: {error}. \
             Keep the document open and verify {}.",
            path.display()
        )
    })?;
    let result = FileStamp::from_metadata(&metadata);
    if !result.matches_current(path)? {
        return Err(format!(
            "{} changed again immediately after saving. Keep the document open and compare the disk version.",
            path.display()
        ));
    }
    Ok(result)
}

fn validate_destination(
    path: &Path,
    expected: Option<&FileStamp>,
) -> Result<Option<Permissions>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            require_regular(path, &metadata)?;
            match expected {
                Some(expected) if *expected == FileStamp::from_metadata(&metadata) => {
                    Ok(Some(metadata.permissions()))
                }
                Some(_) => Err(format!(
                    "{} changed outside SimpTxt. The disk file was not overwritten. \
                     Reload it, save elsewhere, or explicitly confirm replacing the current version.",
                    path.display()
                )),
                None => Err(format!(
                    "{} already exists. Confirm overwriting this file before saving.",
                    path.display()
                )),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if expected.is_some() {
                Err(format!(
                    "{} was removed or moved outside SimpTxt. Save elsewhere or reload; the file was not recreated.",
                    path.display()
                ))
            } else {
                Ok(None)
            }
        }
        Err(error) => Err(format!(
            "Cannot inspect save destination {}: {error}. The original was not changed.",
            path.display()
        )),
    }
}

fn write_document(
    doc: &Document,
    file: &mut File,
    cancel: &AtomicBool,
    progress: &AtomicU64,
) -> Result<(), String> {
    for piece in doc.chunks() {
        for bytes in piece.chunks(IO_CHUNK_BYTES) {
            check_cancel(cancel)?;
            file.write_all(bytes).map_err(|error| {
                format!("Cannot write the temporary file: {error}. The original was not changed.")
            })?;
            progress.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
    }
    Ok(())
}

fn commit(temporary: NamedTempFile, path: &Path, overwrite: bool) -> Result<File, String> {
    let result = if overwrite {
        temporary.persist(path)
    } else {
        // Unlike a check followed by rename, this does not overwrite a file
        // created by another process after our final destination check.
        temporary.persist_noclobber(path)
    };
    result.map_err(|error| {
        format!(
            "Cannot replace {}: {}. The original was not changed.",
            path.display(),
            error.error
        )
        // PersistError owns the tempfile; dropping it cleans up failed writes.
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(bytes: &[u8]) -> Document {
        Document::from_chunks(vec![Arc::from(bytes)]).expect("valid test document")
    }

    fn flags() -> (AtomicBool, AtomicU64) {
        (AtomicBool::new(false), AtomicU64::new(0))
    }

    fn assert_no_temporary_files(directory: &Path) {
        for entry in fs::read_dir(directory).unwrap() {
            assert!(
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".simptxt-save-"),
                "failed save leaked a temporary file"
            );
        }
    }

    #[test]
    fn no_op_save_preserves_bom_and_mixed_line_endings_exactly() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mixed.txt");
        let bytes = b"\xef\xbb\xbfalpha\r\nbeta\ngamma\rdelta\r\nlast";
        fs::write(&path, bytes).unwrap();
        let (cancel, progress) = flags();
        let loaded = load(&path, &cancel, &progress).unwrap();
        let stamp = save(
            &loaded.document,
            &path,
            Some(&loaded.stamp),
            &cancel,
            &progress,
        )
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(progress.load(Ordering::Relaxed), bytes.len() as u64);
        assert_eq!(stamp, FileStamp::read(&path).unwrap());
    }

    #[test]
    fn utf8_across_every_chunk_boundary_is_preserved() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unicode.txt");
        for offset in 1..=3 {
            let mut bytes = vec![b'x'; IO_CHUNK_BYTES - offset];
            bytes.extend_from_slice("🦀é\r\n끝".as_bytes());
            fs::write(&path, &bytes).unwrap();
            let (cancel, progress) = flags();
            let loaded = load(&path, &cancel, &progress).unwrap();
            assert_eq!(
                loaded
                    .document
                    .chunks()
                    .flatten()
                    .copied()
                    .collect::<Vec<_>>(),
                bytes
            );
            for chunk in loaded.document.chunks() {
                assert!(std::str::from_utf8(chunk).is_ok());
            }
        }
    }

    #[test]
    fn invalid_utf8_nul_and_incomplete_utf8_are_read_only_errors() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unsupported.txt");
        for bytes in [&b"abc\xff"[..], &b"ab\0cd"[..], &b"abc\xf0\x9f"[..]] {
            fs::write(&path, bytes).unwrap();
            let (cancel, progress) = flags();
            let error = load(&path, &cancel, &progress).err().unwrap();
            assert!(error.contains("read-only"));
            assert_eq!(fs::read(&path).unwrap(), bytes);
            let text = preview(&path, 64).unwrap();
            assert!(text.starts_with("[Read-only preview"));
            assert!(!text.contains('\0'));
        }
    }

    #[test]
    fn preview_is_bounded_and_omits_trailing_partial_character() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("preview.txt");
        fs::write(&path, "abc🦀tail").unwrap();
        let text = preview(&path, 5).unwrap();
        assert!(text.contains("\n\nabc\n\n[Preview ends"));
        assert!(!text.contains("tail"));
        assert!(!text.contains("abc\u{fffd}"));
    }

    #[test]
    fn empty_file_loads_and_saves() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("empty.txt");
        fs::write(&path, []).unwrap();
        let (cancel, progress) = flags();
        let loaded = load(&path, &cancel, &progress).unwrap();
        assert_eq!(loaded.document.len(), 0);
        save(
            &loaded.document,
            &path,
            Some(&loaded.stamp),
            &cancel,
            &progress,
        )
        .unwrap();
        assert!(fs::read(path).unwrap().is_empty());
    }

    #[test]
    fn cancellation_before_or_during_save_retains_original() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cancel.txt");
        fs::write(&path, b"original").unwrap();
        let expected = FileStamp::read(&path).unwrap();
        let (cancel, progress) = flags();
        cancel.store(true, Ordering::Relaxed);
        assert!(
            save(
                &document(b"replacement"),
                &path,
                Some(&expected),
                &cancel,
                &progress
            )
            .is_err()
        );
        cancel.store(false, Ordering::Relaxed);
        let result = save_with_writer(
            &document(b"replacement"),
            &path,
            Some(&expected),
            &cancel,
            &progress,
            |_, file, cancel, progress| {
                file.write_all(b"partial replacement").unwrap();
                progress.store(19, Ordering::Relaxed);
                cancel.store(true, Ordering::Relaxed);
                Ok(())
            },
        );
        assert!(result.unwrap_err().contains("cancelled"));
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_no_temporary_files(directory.path());
    }

    #[test]
    fn simulated_disk_exhaustion_after_partial_write_retains_original() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("full-disk.txt");
        fs::write(&path, b"original").unwrap();
        let expected = FileStamp::read(&path).unwrap();
        let (cancel, progress) = flags();
        let result = save_with_writer(
            &document(b"replacement"),
            &path,
            Some(&expected),
            &cancel,
            &progress,
            |_, file, _, _| {
                file.write_all(b"partial").unwrap();
                Err("Injected write failure: no space left on device".into())
            },
        );
        assert!(result.unwrap_err().contains("no space left"));
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_no_temporary_files(directory.path());
    }

    #[test]
    fn incomplete_write_cannot_be_reported_as_a_successful_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("short-write.txt");
        fs::write(&path, b"original").unwrap();
        let expected = FileStamp::read(&path).unwrap();
        let (cancel, progress) = flags();
        let result = save_with_writer(
            &document(b"complete replacement"),
            &path,
            Some(&expected),
            &cancel,
            &progress,
            |_, file, _, _| {
                file.write_all(b"partial").unwrap();
                Ok(())
            },
        );
        assert!(result.unwrap_err().contains("Incomplete temporary file"));
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_no_temporary_files(directory.path());
    }

    #[test]
    fn interrupted_save_process_keeps_original() {
        const CHILD_PATH: &str = "SIMPTXT_INTERRUPTED_SAVE_TEST_PATH";
        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let path = PathBuf::from(path);
            let expected = FileStamp::read(&path).unwrap();
            let (cancel, progress) = flags();
            let _ = save_with_writer(
                &document(b"replacement"),
                &path,
                Some(&expected),
                &cancel,
                &progress,
                |_, file, _, _| {
                    file.write_all(b"partial replacement").unwrap();
                    file.sync_all().unwrap();
                    // Simulate process interruption with no stack unwinding
                    // or tempfile destructor. A crash can leave a sidecar,
                    // but must not damage the original.
                    std::process::exit(77);
                },
            );
            panic!("the simulated interrupted process must not return");
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("interrupted.txt");
        fs::write(&path, b"original").unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("file_io::tests::interrupted_save_process_keeps_original")
            .arg("--nocapture")
            .env(CHILD_PATH, &path)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(77));
        assert_eq!(fs::read(path).unwrap(), b"original");
    }

    #[test]
    fn external_change_before_or_during_save_is_not_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("conflict.txt");
        fs::write(&path, b"original").unwrap();
        let expected = FileStamp::read(&path).unwrap();
        let (cancel, progress) = flags();
        fs::write(&path, b"external change").unwrap();
        let result = save(
            &document(b"replacement"),
            &path,
            Some(&expected),
            &cancel,
            &progress,
        );
        assert!(result.unwrap_err().contains("changed outside"));
        let expected = FileStamp::read(&path).unwrap();
        let result = save_with_writer(
            &document(b"replacement"),
            &path,
            Some(&expected),
            &cancel,
            &progress,
            |doc, file, cancel, progress| {
                write_document(doc, file, cancel, progress)?;
                fs::write(&path, b"new external version").unwrap();
                Ok(())
            },
        );
        assert!(result.unwrap_err().contains("changed outside"));
        assert_eq!(fs::read(&path).unwrap(), b"new external version");
        assert_no_temporary_files(directory.path());
    }

    #[test]
    fn create_new_never_overwrites_an_existing_destination() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("new.txt");
        let (cancel, progress) = flags();
        save(&document(b"first"), &path, None, &cancel, &progress).unwrap();
        assert!(save(&document(b"second"), &path, None, &cancel, &progress).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"first");
        assert_no_temporary_files(directory.path());
    }

    #[test]
    fn create_new_detects_a_destination_created_during_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("race.txt");
        let (cancel, progress) = flags();
        let result = save_with_writer(
            &document(b"replacement"),
            &path,
            None,
            &cancel,
            &progress,
            |doc, file, cancel, progress| {
                write_document(doc, file, cancel, progress)?;
                fs::write(&path, b"other process").unwrap();
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"other process");
        assert_no_temporary_files(directory.path());
    }

    #[test]
    fn removed_destination_is_not_silently_recreated() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("removed.txt");
        fs::write(&path, b"original").unwrap();
        let expected = FileStamp::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        let (cancel, progress) = flags();
        assert!(
            save(
                &document(b"replacement"),
                &path,
                Some(&expected),
                &cancel,
                &progress
            )
            .is_err()
        );
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn file_permissions_survive_replacement() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("executable.txt");
        fs::write(&path, b"original").unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o751)).unwrap();
        let expected = FileStamp::read(&path).unwrap();
        let (cancel, progress) = flags();
        save(
            &document(b"replacement"),
            &path,
            Some(&expected),
            &cancel,
            &progress,
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o751
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_load_preview_and_save_are_rejected() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.txt");
        let link = directory.path().join("link.txt");
        fs::write(&target, b"original").unwrap();
        symlink(&target, &link).unwrap();
        let (cancel, progress) = flags();
        assert!(
            load(&link, &cancel, &progress)
                .err()
                .unwrap()
                .contains("symbolic link")
        );
        assert!(preview(&link, 64).unwrap_err().contains("symbolic link"));
        assert!(
            save(&document(b"changed"), &link, None, &cancel, &progress)
                .unwrap_err()
                .contains("symbolic link")
        );
        let expected = FileStamp::read(&target).unwrap();
        assert!(
            save(
                &document(b"changed"),
                &link,
                Some(&expected),
                &cancel,
                &progress
            )
            .is_err()
        );
        assert_eq!(fs::read(target).unwrap(), b"original");
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn directory_permission_failure_preserves_original_when_enforced() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("protected.txt");
        fs::write(&path, b"original").unwrap();
        let expected = FileStamp::read(&path).unwrap();
        fs::set_permissions(directory.path(), Permissions::from_mode(0o500)).unwrap();
        // Privileged test runners can bypass permission checks. Detect that
        // rather than claiming a permission-denied test passed under root.
        let probe = Builder::new()
            .prefix(".permission-probe-")
            .tempfile_in(directory.path());
        if probe.is_ok() {
            drop(probe);
            fs::set_permissions(directory.path(), Permissions::from_mode(0o700)).unwrap();
            eprintln!("permission test skipped: runner can write to a non-writable directory");
            return;
        }
        let (cancel, progress) = flags();
        let result = save(
            &document(b"replacement"),
            &path,
            Some(&expected),
            &cancel,
            &progress,
        );
        fs::set_permissions(directory.path(), Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        assert_eq!(fs::read(path).unwrap(), b"original");
        assert_no_temporary_files(directory.path());
    }
}

#[cfg(test)]
mod size_limit_tests {
    use super::*;
    #[test]
    fn oversized_file_is_rejected_before_reading_or_allocating_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized.txt");
        let file = File::create(&path).unwrap();
        file.set_len(crate::document::MAX_DOCUMENT_BYTES as u64 + 1)
            .unwrap();
        let progress = AtomicU64::new(0);
        let result = load(&path, &AtomicBool::new(false), &progress);
        assert!(result.err().unwrap().contains("2 GiB"));
        assert_eq!(progress.load(Ordering::Relaxed), 0);
    }
}
