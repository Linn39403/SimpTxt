//! Deterministic UI-state regression tests. Native dialogs and worker threads
//! are replaced by direct message injection so races can be tested reliably.

use super::*;

fn editor_with_text(text: &str) -> (egui::Context, EditorApp) {
    let ctx = egui::Context::default();
    let mut app = EditorApp::create(&ctx, false);
    app.doc = Document::from_bytes(text.as_bytes().to_vec()).unwrap();
    app.cursor = app.doc.len();
    (ctx, app)
}

fn pressed(key: Key) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::default(),
    }
}

fn search_job(id: u64) -> SearchWork {
    SearchWork {
        id,
        cancel: Arc::new(AtomicBool::new(false)),
        progress: Arc::new(AtomicUsize::new(0)),
    }
}

fn save_job(total: usize) -> Work {
    Work {
        label: "Saving",
        cancel: Arc::new(AtomicBool::new(false)),
        progress: Arc::new(AtomicU64::new(0)),
        total,
    }
}

fn contents(app: &EditorApp) -> Vec<u8> {
    app.doc.read(0..app.doc.len())
}

#[test]
fn undo_to_saved_revision_updates_visible_status() {
    let (_, mut app) = editor_with_text("saved text");
    app.insert(" modified");
    assert_eq!(app.status, "Modified");
    app.undo(false);
    assert!(!app.doc.is_modified());
    assert_eq!(app.status, "No unsaved changes");
    app.undo(true);
    assert!(app.doc.is_modified());
    assert_eq!(app.status, "Modified");
}

#[test]
fn modal_windows_and_native_dialogs_block_editor_mutations() {
    let modals: [fn(&mut EditorApp); 6] = [
        |app| app.pending = Some(Pending::New),
        |app| app.error = Some("Test error".into()),
        |app| app.overwrite = Some(PathBuf::from("existing.txt")),
        |app| app.conflict = Some(PathBuf::from("changed.txt")),
        |app| app.goto_open = true,
        |app| app.dialog_pending = true,
    ];

    for (index, open_modal) in modals.into_iter().enumerate() {
        let (ctx, mut app) = editor_with_text("keep 💚 intact");
        app.anchor = Some(0);
        open_modal(&mut app);
        assert!(app.modal_open(), "Modal {index} must block input");
        let original = contents(&app);
        let selection = app.selection();
        let revision = app.doc.revision();
        for event in [
            egui::Event::Text("lost".into()),
            egui::Event::Paste("lost".into()),
            egui::Event::Cut,
            pressed(Key::Backspace),
            pressed(Key::Delete),
            pressed(Key::Enter),
            egui::Event::Ime(egui::ImeEvent::Commit("lost".into())),
        ] {
            app.event(event, &ctx);
        }
        assert_eq!(contents(&app), original, "Modal {index} lost document data");
        assert_eq!(app.selection(), selection);
        assert_eq!(app.doc.revision(), revision);
        assert!(!app.doc.is_modified());
    }
}

#[test]
fn obsolete_search_results_never_clear_or_apply_over_newer_job() {
    let (ctx, mut app) = editor_with_text("alpha beta");
    app.generation = 7;
    app.search_work = Some(search_job(2));
    let active_cancel = app.search_work.as_ref().unwrap().cancel.clone();
    let revision = app.doc.revision();
    let original_selection = app.selection();
    let replacement = Document::from_bytes(b"WRONG DOCUMENT".to_vec()).unwrap();

    // A different generation can have the same local job number.
    app.tx
        .send(Message::Found(6, 2, revision, Ok(Some(0..5))))
        .unwrap();
    app.tx
        .send(Message::Replaced(
            6,
            2,
            revision,
            Ok((replacement.snapshot(), 1)),
        ))
        .unwrap();
    // A cancelled job from the current generation can finish after its successor.
    app.tx
        .send(Message::Found(7, 1, revision, Ok(Some(0..5))))
        .unwrap();
    app.tx
        .send(Message::Replaced(7, 1, revision, Ok((replacement, 1))))
        .unwrap();
    app.poll(&ctx);

    let active = app.search_work.as_ref().expect("Current job was cleared");
    assert_eq!(active.id, 2);
    assert!(Arc::ptr_eq(&active.cancel, &active_cancel));
    assert!(!active_cancel.load(Ordering::Relaxed));
    assert_eq!(contents(&app), b"alpha beta");
    assert_eq!(app.selection(), original_selection);
    assert!(!app.doc.is_modified());

    app.tx
        .send(Message::Found(7, 2, revision, Ok(Some(6..10))))
        .unwrap();
    app.poll(&ctx);
    assert!(app.search_work.is_none());
    assert_eq!(app.selection(), 6..10);
}

#[test]
fn search_result_for_old_revision_keeps_new_edits() {
    let (ctx, mut app) = editor_with_text("before");
    let searched_revision = app.doc.revision();
    app.search_work = Some(search_job(1));
    app.insert(" after");
    let revision = app.doc.revision();
    app.tx
        .send(Message::Replaced(
            app.generation,
            1,
            searched_revision,
            Ok((Document::from_bytes(b"obsolete".to_vec()).unwrap(), 1)),
        ))
        .unwrap();
    app.poll(&ctx);
    assert!(app.search_work.is_none());
    assert_eq!(contents(&app), b"before after");
    assert_eq!(app.doc.revision(), revision);
    assert!(app.doc.is_modified());
}

#[test]
fn obsolete_highlight_messages_do_not_clear_current_worker() {
    let (ctx, mut app) = editor_with_text("fn main() {}");
    app.generation = 7;
    app.language = Language::Rust;
    app.highlight_id = 4;
    app.highlight_revision = Some(app.doc.revision());
    let active_cancel = Arc::new(AtomicBool::new(false));
    app.highlight_cancel = Some(active_cancel.clone());
    let revision = app.doc.revision();

    for (generation, result_revision, id) in [(6, revision, 4), (7, 0, 4), (7, revision, 3)] {
        let index =
            HighlightIndex::build(&app.doc, Language::Rust, &AtomicBool::new(false)).unwrap();
        app.tx
            .send(Message::Highlighted(
                generation,
                result_revision,
                id,
                true,
                Ok(index),
            ))
            .unwrap();
    }
    app.poll(&ctx);
    assert!(app.highlight.is_none());
    assert!(Arc::ptr_eq(
        app.highlight_cancel
            .as_ref()
            .expect("Current worker was cleared"),
        &active_cancel
    ));

    let prefix = HighlightIndex::build(&app.doc, Language::Rust, &AtomicBool::new(false)).unwrap();
    app.tx
        .send(Message::Highlighted(7, revision, 4, false, Ok(prefix)))
        .unwrap();
    app.poll(&ctx);
    assert!(app.highlight.is_some());
    assert!(
        app.highlight_cancel.is_some(),
        "Prefix is not worker completion"
    );

    let complete =
        HighlightIndex::build(&app.doc, Language::Rust, &AtomicBool::new(false)).unwrap();
    app.tx
        .send(Message::Highlighted(7, revision, 4, true, Ok(complete)))
        .unwrap();
    app.poll(&ctx);
    assert!(app.highlight.is_some());
    assert!(app.highlight_cancel.is_none());
}

#[test]
fn ime_composition_owns_enter_and_navigation_until_commit() {
    let (ctx, mut app) = editor_with_text("prefix ");
    let original = contents(&app);
    let position = app.cursor;
    app.event(
        egui::Event::Ime(egui::ImeEvent::Preedit {
            text: "かな".into(),
            active_range_chars: None,
        }),
        &ctx,
    );
    for key in [Key::Enter, Key::Backspace, Key::Delete, Key::ArrowLeft] {
        app.event(pressed(key), &ctx);
    }
    assert_eq!(contents(&app), original);
    assert_eq!(app.cursor, position);
    assert!(!app.doc.is_modified());
    app.event(
        egui::Event::Ime(egui::ImeEvent::Commit("仮名".into())),
        &ctx,
    );
    assert_eq!(contents(&app), "prefix 仮名".as_bytes());
    assert!(app.composition.is_empty());
    app.undo(false);
    assert_eq!(contents(&app), original);
    assert!(
        !app.doc.is_modified(),
        "One composition should be one undo action"
    );
}

#[test]
fn empty_native_input_does_not_delete_the_selection() {
    for event in [
        egui::Event::Text(String::new()),
        egui::Event::Paste(String::new()),
        egui::Event::Ime(egui::ImeEvent::Commit(String::new())),
        egui::Event::Ime(egui::ImeEvent::Preedit {
            text: String::new(),
            active_range_chars: None,
        }),
    ] {
        let (ctx, mut app) = editor_with_text("selected 💚 text");
        app.anchor = Some(0);
        let selection = app.selection();
        app.event(event, &ctx);
        assert_eq!(contents(&app), "selected 💚 text".as_bytes());
        assert_eq!(app.selection(), selection);
        assert!(!app.doc.is_modified());
    }
}

#[test]
fn text_newline_events_do_not_duplicate_enter() {
    let (ctx, mut app) = editor_with_text("line");
    app.event(pressed(Key::Enter), &ctx);
    let after_enter = contents(&app);
    app.event(egui::Event::Text("\n".into()), &ctx);
    app.event(egui::Event::Text("\r".into()), &ctx);
    assert_eq!(contents(&app), after_enter);
    app.event(egui::Event::Paste("pasted\nlines".into()), &ctx);
    assert!(contents(&app).ends_with(b"pasted\nlines"));
}

#[test]
fn save_as_language_change_invalidates_queued_old_syntax() {
    let (ctx, mut app) = editor_with_text("{\"key\": 1}");
    app.language = Language::Rust;
    app.highlight_id = 9;
    app.highlight_revision = Some(app.doc.revision());
    app.highlight =
        Some(HighlightIndex::build(&app.doc, Language::Rust, &AtomicBool::new(false)).unwrap());
    let cancel = Arc::new(AtomicBool::new(false));
    app.highlight_cancel = Some(cancel.clone());
    let old_index =
        HighlightIndex::build(&app.doc, Language::Rust, &AtomicBool::new(false)).unwrap();
    let revision = app.doc.revision();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("saved.json");
    std::fs::write(&path, contents(&app)).unwrap();
    let stamp = FileStamp::read(&path).unwrap();
    app.work = Some(save_job(app.doc.len()));
    app.tx
        .send(Message::Saved(
            app.generation,
            revision,
            path.clone(),
            Ok(stamp),
        ))
        .unwrap();
    app.tx
        .send(Message::Highlighted(
            app.generation,
            revision,
            9,
            true,
            Ok(old_index),
        ))
        .unwrap();
    app.poll(&ctx);

    assert_eq!(app.path.as_ref(), Some(&path));
    assert_eq!(app.language, Language::Json);
    assert!(cancel.load(Ordering::Relaxed));
    assert!(app.highlight.is_none());
    assert!(app.highlight_revision.is_none());
    assert_ne!(app.highlight_id, 9);
    assert!(app.work.is_none());
    assert!(!app.doc.is_modified());
}

#[test]
fn saved_snapshot_does_not_mark_newer_edits_clean_or_discard_them() {
    let (ctx, mut app) = editor_with_text("initial");
    app.insert(" saved");
    let saved_revision = app.doc.revision();
    let snapshot = contents(&app);
    app.work = Some(save_job(snapshot.len()));
    // Editing remains supported while the immutable snapshot is being saved.
    app.insert(" newer");
    let current_revision = app.doc.revision();
    app.pending = Some(Pending::New);
    app.continue_after_save = true;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("saved.txt");
    std::fs::write(&path, &snapshot).unwrap();
    let stamp = FileStamp::read(&path).unwrap();
    app.tx
        .send(Message::Saved(
            app.generation,
            saved_revision,
            path,
            Ok(stamp),
        ))
        .unwrap();
    app.poll(&ctx);

    assert_eq!(contents(&app), b"initial saved newer");
    assert_eq!(app.doc.revision(), current_revision);
    assert!(app.doc.is_modified());
    assert!(
        app.pending.is_some(),
        "Save must not discard edits made afterward"
    );
    assert!(!app.continue_after_save);
    assert!(app.work.is_none());
    assert!(app.status.contains("newer edits"));

    // Undoing only the newer edit returns exactly to the saved snapshot.
    app.undo(false);
    assert_eq!(contents(&app), snapshot);
    assert_eq!(app.doc.revision(), saved_revision);
    assert!(!app.doc.is_modified());
}

#[test]
fn failed_save_retains_unsaved_document_and_pending_choice() {
    let (ctx, mut app) = editor_with_text("initial");
    app.insert(" unsaved");
    app.work = Some(save_job(app.doc.len()));
    app.pending = Some(Pending::Exit);
    app.continue_after_save = true;
    let revision = app.doc.revision();
    app.tx
        .send(Message::Saved(
            app.generation,
            revision,
            PathBuf::from("unwritten.txt"),
            Err("Synthetic disk failure".into()),
        ))
        .unwrap();
    app.poll(&ctx);

    assert_eq!(contents(&app), b"initial unsaved");
    assert_eq!(app.doc.revision(), revision);
    assert!(app.doc.is_modified());
    assert!(app.pending.is_some());
    assert!(!app.continue_after_save);
    assert!(!app.allow_close);
    assert!(app.work.is_none());
    assert_eq!(app.error.as_deref(), Some("Synthetic disk failure"));
}

#[test]
fn destructive_request_discards_queued_replace_all_before_confirmation() {
    let (ctx, mut app) = editor_with_text("original");
    app.insert(" unsaved");
    let revision = app.doc.revision();
    app.search_work = Some(search_job(1));
    let cancel = app.search_work.as_ref().unwrap().cancel.clone();
    // Simulate a worker that finished immediately before the destructive request.
    app.tx
        .send(Message::Replaced(
            app.generation,
            1,
            revision,
            Ok((
                Document::from_bytes(b"late replacement".to_vec()).unwrap(),
                1,
            )),
        ))
        .unwrap();

    // Dirty New triggers the same destructive-request guard as Open, without
    // launching an OS picker or discarding the document in this test.
    app.request(Pending::New, &ctx);
    assert!(app.pending.is_some());
    assert!(app.search_work.is_none());
    assert!(cancel.load(Ordering::Relaxed));
    app.poll(&ctx);

    assert_eq!(contents(&app), b"original unsaved");
    assert_eq!(app.doc.revision(), revision);
    assert!(app.doc.is_modified());
    assert!(app.pending.is_some());
}

#[test]
fn undo_cursor_does_not_split_crlf_before_delete() {
    let (ctx, mut app) = editor_with_text("a\r\nb");
    app.move_to(1, false);
    app.insert("x");
    assert_eq!(contents(&app), b"ax\r\nb");
    assert_eq!(app.cursor, 2);

    app.undo(false);
    assert_eq!(contents(&app), b"a\r\nb");
    assert_eq!(app.cursor, 1, "Undo left the cursor between CR and LF");
    app.event(pressed(Key::Delete), &ctx);
    assert_eq!(contents(&app), b"ab", "Delete must remove the CRLF unit");
    app.undo(false);
    assert_eq!(contents(&app), b"a\r\nb");
    assert!(!app.doc.is_modified());
}

#[test]
fn review_shift_tab_outdents_selected_lines_without_replacing_them() {
    for reverse_selection in [false, true] {
        let original = "  abc\r\n  def";
        let (ctx, mut app) = editor_with_text(original);
        let (anchor, cursor) = if reverse_selection {
            (app.doc.len(), 0)
        } else {
            (0, app.doc.len())
        };
        app.anchor = Some(anchor);
        app.cursor = cursor;
        let mut event = pressed(Key::Tab);
        if let egui::Event::Key { modifiers, .. } = &mut event {
            modifiers.shift = true;
        }
        app.event(event, &ctx);
        assert_eq!(
            contents(&app),
            b"abc\r\ndef",
            "Shift-Tab must outdent, not replace selected lines with a tab"
        );
        assert!(!app.selection().is_empty());
        app.undo(false);
        assert_eq!(contents(&app), original.as_bytes());
        assert_eq!(app.anchor, Some(anchor));
        assert_eq!(app.cursor, cursor);
        assert!(!app.doc.is_modified());
        app.undo(true);
        assert_eq!(contents(&app), b"abc\r\ndef");
        assert!(!app.selection().is_empty());
    }
}

#[test]
fn review_ime_surrounding_counts_crlf_as_two_unicode_scalars() {
    for (text, cursor, before_chars, after_chars, expected) in [
        ("a\r\nb", 3, 2, 0, "ab"),
        ("a\r\nb", 1, 0, 2, "ab"),
        ("é\r\n💚", 4, 2, 1, "é"),
        ("é\r\n💚", 2, 1, 2, "💚"),
        ("\u{feff}é\r\n💚", 7, 3, 0, "\u{feff}💚"),
    ] {
        let (ctx, mut app) = editor_with_text(text);
        app.cursor = cursor;
        app.event(
            egui::Event::Ime(egui::ImeEvent::DeleteSurrounding {
                before_chars,
                after_chars,
            }),
            &ctx,
        );
        assert_eq!(
            contents(&app),
            expected.as_bytes(),
            "IME deletion must count individual Unicode scalars in {text:?}"
        );
        assert!(app.doc.char_boundary(app.cursor));
        app.undo(false);
        assert_eq!(contents(&app), text.as_bytes());
        assert_eq!(app.cursor, cursor);
        assert!(!app.doc.is_modified());
    }
}

#[test]
fn review_vertical_navigation_uses_unicode_not_byte_columns() {
    for (text, start, down) in [
        ("éab\nxyz", 2, 6),
        ("💚ab\nαβγ", 4, 9),
        ("\u{feff}éab\r\nxyz", 5, 10),
    ] {
        let (ctx, mut app) = editor_with_text(text);
        app.cursor = start;
        app.event(pressed(Key::ArrowDown), &ctx);
        assert_eq!(app.cursor, down, "wrong character column in {text:?}");
        app.event(pressed(Key::ArrowUp), &ctx);
        assert_eq!(app.cursor, start);
    }
}

#[test]
fn review_vertical_navigation_retains_preferred_column_through_short_lines() {
    let (ctx, mut app) = editor_with_text("éabc\nx\n💚xyz");
    app.cursor = "éab".len();
    app.event(pressed(Key::ArrowDown), &ctx);
    assert_eq!(app.cursor, "éabc\nx".len());
    app.event(pressed(Key::ArrowDown), &ctx);
    assert_eq!(
        app.cursor,
        "éabc\nx\n💚xy".len(),
        "A short line must not erase the preferred Unicode column"
    );
    app.event(pressed(Key::ArrowUp), &ctx);
    app.event(pressed(Key::ArrowUp), &ctx);
    assert_eq!(app.cursor, "éab".len());
    app.event(pressed(Key::ArrowRight), &ctx);
    app.event(pressed(Key::ArrowDown), &ctx);
    app.event(pressed(Key::ArrowDown), &ctx);
    assert_eq!(
        app.cursor,
        app.doc.len(),
        "Horizontal movement resets the goal"
    );
}

#[test]
fn review_indent_excludes_unselected_trailing_line_and_preserves_bom_crlf() {
    let original = "\u{feff}one\r\ntwo\r\nthree";
    let (ctx, mut app) = editor_with_text(original);
    let start = view::content_start(&app.doc);
    let end = "\u{feff}one\r\ntwo\r\n".len();
    app.anchor = Some(start);
    app.cursor = end;
    app.event(pressed(Key::Tab), &ctx);
    assert_eq!(contents(&app), "\u{feff}\tone\r\n\ttwo\r\nthree".as_bytes());
    assert_eq!(app.selection(), start + 1..end + 2);
    app.undo(false);
    assert_eq!(contents(&app), original.as_bytes());
    assert_eq!(app.anchor, Some(start));
    assert_eq!(app.cursor, end);
    assert!(!app.doc.is_modified());
}

#[test]
fn review_enter_copies_leading_indent_without_normalizing_line_endings() {
    let (ctx, mut app) = editor_with_text("\u{feff} \talpha\r\nbeta\n");
    app.cursor = "\u{feff} \talpha".len();
    app.event(pressed(Key::Enter), &ctx);
    assert_eq!(
        contents(&app),
        "\u{feff} \talpha\r\n \t\r\nbeta\n".as_bytes()
    );
    assert_eq!(app.cursor, "\u{feff} \talpha\r\n \t".len());
    app.undo(false);
    assert_eq!(contents(&app), "\u{feff} \talpha\r\nbeta\n".as_bytes());
    assert_eq!(app.cursor, "\u{feff} \talpha".len());
    assert!(!app.doc.is_modified());
}

#[test]
fn review_undo_restores_original_selection_direction_and_redo_caret() {
    for reversed in [false, true] {
        let original = "aé💚z";
        let (ctx, mut app) = editor_with_text(original);
        let (anchor, cursor) = if reversed { (7, 1) } else { (1, 7) };
        app.anchor = Some(anchor);
        app.cursor = cursor;
        app.event(egui::Event::Text("x".into()), &ctx);
        assert_eq!(contents(&app), b"axz");
        assert_eq!(app.cursor, 2);
        assert!(app.anchor.is_none());
        app.undo(false);
        assert_eq!(contents(&app), original.as_bytes());
        assert_eq!(app.cursor, cursor);
        assert_eq!(app.anchor, Some(anchor));
        assert!(!app.doc.is_modified());
        app.undo(true);
        assert_eq!(contents(&app), b"axz");
        assert_eq!(app.cursor, 2);
        assert!(app.anchor.is_none());
    }
}

#[test]
fn review_typing_groups_stop_at_motion_paste_and_undo() {
    let (ctx, mut app) = editor_with_text("");
    for text in ["a", "é", "💚"] {
        app.event(egui::Event::Text(text.into()), &ctx);
    }
    app.event(pressed(Key::ArrowLeft), &ctx);
    app.event(egui::Event::Text("x".into()), &ctx);
    app.event(egui::Event::Paste("yz".into()), &ctx);
    assert_eq!(contents(&app), "aéxyz💚".as_bytes());
    app.undo(false);
    assert_eq!(
        contents(&app),
        "aéx💚".as_bytes(),
        "Paste is its own undo action"
    );
    app.undo(false);
    assert_eq!(
        contents(&app),
        "aé💚".as_bytes(),
        "Motion breaks a typing group"
    );
    assert_eq!(app.cursor, "aé".len());
    app.undo(false);
    assert!(
        app.doc.is_empty(),
        "Adjacent native text events form one action"
    );
    assert_eq!(app.cursor, 0);
    assert!(!app.doc.is_modified());
    app.event(egui::Event::Text("new".into()), &ctx);
    app.undo(true);
    assert_eq!(contents(&app), b"new", "A branch edit must discard redo");
}

#[test]
fn review_word_navigation_and_deletion_respect_unicode_bom_and_crlf() {
    let original = "\u{feff}éclair  beta\r\nγ";
    let (ctx, mut app) = editor_with_text(original);
    app.cursor = view::content_start(&app.doc);
    let word_key = |key| {
        let mut event = pressed(key);
        if let egui::Event::Key { modifiers, .. } = &mut event {
            modifiers.ctrl = true;
        }
        event
    };
    app.event(word_key(Key::ArrowRight), &ctx);
    assert_eq!(app.cursor, "\u{feff}éclair  ".len());
    app.event(word_key(Key::Delete), &ctx);
    assert_eq!(contents(&app), "\u{feff}éclair  γ".as_bytes());
    app.undo(false);
    assert_eq!(contents(&app), original.as_bytes());
    assert_eq!(app.cursor, "\u{feff}éclair  ".len());
    app.event(word_key(Key::Backspace), &ctx);
    assert_eq!(contents(&app), "\u{feff}beta\r\nγ".as_bytes());
    assert_eq!(app.cursor, 3);
    app.undo(false);
    assert_eq!(contents(&app), original.as_bytes());
    app.event(word_key(Key::ArrowLeft), &ctx);
    assert_eq!(app.cursor, 3);
    app.event(word_key(Key::Backspace), &ctx);
    assert_eq!(
        contents(&app),
        original.as_bytes(),
        "Word deletion cannot remove BOM"
    );
}

#[test]
fn review_oversized_edit_refuses_atomically_and_preserves_selection_and_prior_undo() {
    let original = "a".repeat(512 * 1024);
    let (ctx, mut app) = editor_with_text(&original);
    app.doc.set_history_byte_limit(64 * 1024);
    app.insert(" prior");
    assert!(app.error.is_none());
    let before = contents(&app);
    let revision = app.doc.revision();
    let history_bytes = app.doc.history_bytes();
    let evictions = app.doc.history_evictions();
    let change = app.doc.last_change();
    app.anchor = Some(0);
    app.cursor = app.doc.len();
    let cursor = app.cursor;
    app.event(pressed(Key::Backspace), &ctx);
    assert!(
        app.error.is_some(),
        "Refused undo capacity must be explained"
    );
    assert_eq!(contents(&app), before);
    assert_eq!(app.doc.revision(), revision);
    assert_eq!(app.doc.history_bytes(), history_bytes);
    assert_eq!(app.doc.history_evictions(), evictions);
    assert_eq!(app.doc.last_change(), change);
    assert_eq!(app.cursor, cursor);
    assert_eq!(app.anchor, Some(0));
    assert!(app.doc.is_modified());
    app.error = None;
    app.undo(false);
    assert_eq!(contents(&app), original.as_bytes());
    assert_eq!(app.cursor, original.len());
    assert!(!app.doc.is_modified());
    app.undo(true);
    assert_eq!(contents(&app), before);
    assert_eq!(app.doc.revision(), revision);
}

#[test]
fn review_oversized_replace_all_preserves_document_selection_revision_and_history() {
    let original = "a".repeat(512 * 1024);
    let (ctx, mut app) = editor_with_text(&original);
    app.doc.set_history_byte_limit(64 * 1024);
    app.insert(" prior");
    let before = contents(&app);
    let revision = app.doc.revision();
    let history_bytes = app.doc.history_bytes();
    let evictions = app.doc.history_evictions();
    let change = app.doc.last_change();
    app.cursor = 1;
    app.anchor = Some(9);
    app.search_work = Some(search_job(1));
    app.tx
        .send(Message::Replaced(
            app.generation,
            1,
            revision,
            Ok((Document::from_bytes(b"replacement".to_vec()).unwrap(), 1)),
        ))
        .unwrap();
    app.poll(&ctx);
    assert!(app.search_work.is_none());
    assert!(
        app.error.is_some(),
        "Replace-all admission refusal must be explained"
    );
    assert_eq!(contents(&app), before);
    assert_eq!(app.doc.revision(), revision);
    assert_eq!(app.doc.history_bytes(), history_bytes);
    assert_eq!(app.doc.history_evictions(), evictions);
    assert_eq!(app.doc.last_change(), change);
    assert_eq!(app.cursor, 1);
    assert_eq!(app.anchor, Some(9));
    assert!(app.doc.is_modified());
    assert!(!app.status.contains("one undo action"));
    app.error = None;
    app.undo(false);
    assert_eq!(contents(&app), original.as_bytes());
    assert!(!app.doc.is_modified());
    app.undo(true);
    assert_eq!(contents(&app), before);
    assert_eq!(app.doc.revision(), revision);
}

#[test]
fn review_rejected_edit_does_not_discard_an_existing_redo_branch() {
    let original = "a".repeat(512 * 1024);
    let (ctx, mut app) = editor_with_text(&original);
    app.doc.set_history_byte_limit(64 * 1024);
    app.insert(" first");
    app.insert(" second");
    let expected_redo = contents(&app);
    let expected_revision = app.doc.revision();
    app.undo(false);
    let before = contents(&app);
    let before_revision = app.doc.revision();
    app.anchor = Some(0);
    app.cursor = app.doc.len();
    app.event(pressed(Key::Delete), &ctx);
    assert!(app.error.is_some());
    assert_eq!(contents(&app), before);
    assert_eq!(app.doc.revision(), before_revision);
    assert_eq!(app.selection(), 0..before.len());
    app.error = None;
    app.undo(true);
    assert_eq!(contents(&app), expected_redo);
    assert_eq!(app.doc.revision(), expected_revision);
    assert_eq!(app.cursor, expected_redo.len());
    assert!(app.anchor.is_none());
}

#[test]
fn review_replace_all_worker_refusal_preserves_state_and_finishes_status() {
    let (ctx, mut app) = editor_with_text("original");
    app.insert(" prior");
    let before = contents(&app);
    let revision = app.doc.revision();
    let history_bytes = app.doc.history_bytes();
    app.cursor = 2;
    app.anchor = Some(7);
    app.status = "Preparing replace all".into();
    app.search_work = Some(search_job(1));
    app.tx
        .send(Message::Replaced(
            app.generation,
            1,
            revision,
            Err("The replacement exceeds the undo/redo budget".into()),
        ))
        .unwrap();
    app.poll(&ctx);
    assert!(app.search_work.is_none());
    assert_eq!(contents(&app), before);
    assert_eq!(app.doc.revision(), revision);
    assert_eq!(app.doc.history_bytes(), history_bytes);
    assert_eq!(app.cursor, 2);
    assert_eq!(app.anchor, Some(7));
    assert!(app.error.as_ref().unwrap().contains("undo/redo budget"));
    assert!(app.status.contains("Replace all was not applied"));
    assert!(app.status.contains("unchanged"));
    app.error = None;
    app.undo(false);
    assert_eq!(contents(&app), b"original");
    assert!(!app.doc.is_modified());
}

#[test]
fn review_vim_visual_refused_delete_and_change_preserve_mode_selection_and_register() {
    use simptxt::vim::{Mode, Register};
    for command in ["d", "c"] {
        let original = "a".repeat(512 * 1024);
        let (ctx, mut app) = editor_with_text(&original);
        app.doc.set_history_byte_limit(64 * 1024);
        app.insert(" prior");
        let before = contents(&app);
        let revision = app.doc.revision();
        let history_bytes = app.doc.history_bytes();
        let change = app.doc.last_change();
        app.vim_enabled = true;
        app.vim.set_mode(Mode::Visual);
        app.anchor = Some(0);
        app.cursor = app.doc.len() - 1;
        let cursor = app.cursor;
        app.vim_register = Register {
            text: "previous register".into(),
            linewise: true,
        };
        app.event(egui::Event::Text(command.into()), &ctx);
        assert_eq!(contents(&app), before, "Vim command {command}");
        assert_eq!(app.doc.revision(), revision);
        assert_eq!(app.doc.history_bytes(), history_bytes);
        assert_eq!(app.doc.last_change(), change);
        assert_eq!(app.cursor, cursor);
        assert_eq!(app.anchor, Some(0));
        assert_eq!(app.vim.mode(), Mode::Visual);
        assert_eq!(app.vim_register.text, "previous register");
        assert!(app.vim_register.linewise);
        assert!(
            app.error
                .as_deref()
                .unwrap_or(&app.status)
                .contains("undo/redo budget")
        );
        app.error = None;
        app.undo(false);
        assert_eq!(contents(&app), original.as_bytes());
        assert!(!app.doc.is_modified());
    }
}
