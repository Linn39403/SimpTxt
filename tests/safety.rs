use rust_notepad_prototype::Document;
use std::fs;

#[test]
fn failed_save_as_preserves_path_dirty_and_history() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.txt");
    fs::write(&source, "before").unwrap();
    let mut doc = Document::open(&source).unwrap();
    // Opening resolves symlinks and platform-specific canonical path prefixes.
    let original_path = doc.path().unwrap().to_path_buf();
    doc.set_text("after".into());
    assert!(doc.save_as(dir.path().join("missing/failure.txt")).is_err());
    assert_eq!(doc.path(), Some(original_path.as_path()));
    assert_eq!(doc.text(), "after");
    assert!(doc.is_dirty());
    assert_eq!(fs::read_to_string(&source).unwrap(), "before");
    assert!(doc.undo());
    assert!(!doc.is_dirty());
}

#[test]
fn deleted_original_is_not_silently_recreated() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.txt");
    fs::write(&source, "before").unwrap();
    let mut doc = Document::open(&source).unwrap();
    doc.set_text("after".into());
    fs::remove_file(&source).unwrap();
    assert!(doc.save().is_err());
    assert!(!source.exists());
    assert!(doc.is_dirty());
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        0,
        "Temporary files must be cleaned up after failure"
    );
}

#[test]
fn edited_bom_crlf_roundtrips_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.txt");
    fs::write(&source, "\u{feff}é\r\n中文\r\n").unwrap();
    let mut doc = Document::open(&source).unwrap();
    assert_eq!(doc.line_ending_label(), "Windows (CRLF)");
    doc.set_text(format!("{}next\r\n", doc.text()));
    doc.save().unwrap();
    assert_eq!(
        fs::read(&source).unwrap(),
        "\u{feff}é\r\n中文\r\nnext\r\n".as_bytes()
    );
}

#[test]
fn replacement_is_nonrecursive_and_atomic_in_undo() {
    let mut doc = Document::new();
    doc.set_text("🦀🦀🦀".into());
    assert_eq!(doc.replace_all("🦀", "🦀🦀", true), 3);
    assert_eq!(doc.char_count(), 6);
    assert!(doc.undo());
    assert_eq!(doc.char_count(), 3);
    assert_eq!(doc.replace_all("", "x", true), 0);
}

#[cfg(unix)]
#[test]
fn save_preserves_symlink_and_target_permissions() {
    use std::os::unix::{fs::PermissionsExt, fs::symlink};
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.txt");
    let link = dir.path().join("alias.txt");
    fs::write(&source, "before").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
    symlink(&source, &link).unwrap();
    let mut doc = Document::open(&link).unwrap();
    doc.set_text("after".into());
    doc.save().unwrap();
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(&link).unwrap(), "after");
    assert_eq!(
        fs::metadata(&source).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[cfg(unix)]
#[test]
fn read_only_original_is_never_replaced() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.txt");
    fs::write(&source, "before").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o444)).unwrap();
    let mut doc = Document::open(&source).unwrap();
    doc.set_text("after".into());
    assert!(doc.save().is_err());
    assert!(doc.is_dirty());
    assert_eq!(fs::read_to_string(&source).unwrap(), "before");
}
