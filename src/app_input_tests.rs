//! Focused input checks; the independent safety suite lives separately.
use super::*;

fn editor(text: &str) -> (egui::Context, EditorApp) {
    let context = egui::Context::default();
    let mut app = EditorApp::create(&context, false);
    app.doc = Document::from_bytes(text.as_bytes().to_vec()).unwrap();
    app.reset_view();
    (context, app)
}
fn text(app: &EditorApp) -> String {
    String::from_utf8(app.doc.read(0..app.doc.len())).unwrap()
}
fn key(key: Key, shift: bool, word: bool) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers {
            shift,
            ctrl: word,
            command: word,
            ..Default::default()
        },
    }
}
fn ime(before_chars: usize, after_chars: usize) -> egui::Event {
    egui::Event::Ime(egui::ImeEvent::DeleteSurrounding {
        before_chars,
        after_chars,
    })
}

#[test]
fn block_outdent_keeps_unicode_crlf_selection_and_one_undo() {
    let original = "\u{feff}  one\r\n\t二\r\n  three";
    let (ctx, mut app) = editor(original);
    app.anchor = Some(3);
    app.cursor = app.doc.len();
    let before = app.cursor_state();
    app.event(key(Key::Tab, true, false), &ctx);
    assert_eq!(text(&app), "\u{feff}one\r\n二\r\nthree");
    assert_eq!(app.anchor, Some(3));
    assert_eq!(app.cursor, app.doc.len());
    let after = app.cursor_state();
    app.undo(false);
    assert_eq!(text(&app), original);
    assert_eq!(app.cursor_state(), before);
    assert!(!app.doc.is_modified());
    app.undo(true);
    assert_eq!(text(&app), "\u{feff}one\r\n二\r\nthree");
    assert_eq!(app.cursor_state(), after);
}

#[test]
fn block_tab_excludes_untouched_final_line_and_maps_backward_selection() {
    let (ctx, mut app) = editor("α\r\nβ\r\nγ");
    app.cursor = 0;
    app.anchor = Some("α\r\nβ\r\n".len());
    let before = app.cursor_state();
    app.event(key(Key::Tab, false, false), &ctx);
    assert_eq!(text(&app), "\tα\r\n\tβ\r\nγ");
    assert_eq!(app.cursor, 1);
    assert_eq!(app.anchor, Some("\tα\r\n\tβ\r\n".len()));
    app.undo(false);
    assert_eq!(text(&app), "α\r\nβ\r\nγ");
    assert_eq!(app.cursor_state(), before);
}

#[test]
fn outdent_only_removes_existing_prefix() {
    let (ctx, mut app) = editor("      α");
    app.cursor = app.doc.len();
    app.event(key(Key::Tab, true, false), &ctx);
    assert_eq!(text(&app), "  α");
    assert_eq!(app.cursor, app.doc.len());
    app.event(key(Key::Tab, true, false), &ctx);
    assert_eq!(text(&app), "α");
    let revision = app.doc.revision();
    app.event(key(Key::Tab, true, false), &ctx);
    assert_eq!(text(&app), "α");
    assert_eq!(app.doc.revision(), revision);
}

#[test]
fn ime_counts_raw_unicode_scalars_and_exact_crlf_halves() {
    for (cursor, before, after, expected) in [
        (3, 2, 0, "ab"),
        (3, 1, 0, "a\rb"),
        (1, 0, 1, "a\nb"),
        (1, 0, 2, "ab"),
    ] {
        let (ctx, mut app) = editor("a\r\nb");
        app.cursor = cursor;
        app.event(ime(before, after), &ctx);
        assert_eq!(text(&app), expected);
        app.undo(false);
        assert_eq!(text(&app), "a\r\nb");
        assert_eq!(app.cursor, cursor);
    }
    let (ctx, mut app) = editor("\u{feff}a💚\r\n猫b");
    app.cursor = "\u{feff}a💚\r\n".len();
    app.event(ime(3, 1), &ctx);
    assert_eq!(text(&app), "\u{feff}ab");
    app.undo(false);
    app.move_to(3, false);
    app.event(ime(100, 0), &ctx);
    assert_eq!(text(&app), "\u{feff}a💚\r\n猫b");
}

#[test]
fn ime_excessive_request_is_atomic_refusal() {
    let (ctx, mut app) = editor("keep this");
    app.cursor = app.doc.len();
    app.event(ime(usize::MAX, 0), &ctx);
    assert_eq!(text(&app), "keep this");
    assert!(!app.doc.is_modified());
    assert!(app.status.contains("nothing was changed"));
}

#[test]
fn vertical_scalar_column_survives_short_lines_and_resets_after_horizontal_motion() {
    let (ctx, mut app) = editor("éabc\nx\n猫狗鸡鸭\n12345");
    app.move_to("éab".len(), false);
    app.event(key(Key::ArrowDown, false, false), &ctx);
    assert_eq!(app.cursor, "éabc\nx".len());
    app.event(key(Key::ArrowDown, false, false), &ctx);
    assert_eq!(app.cursor, "éabc\nx\n猫狗鸡".len());
    app.event(key(Key::ArrowLeft, false, false), &ctx);
    app.event(key(Key::ArrowDown, false, false), &ctx);
    assert_eq!(app.cursor, "éabc\nx\n猫狗鸡鸭\n12".len());
}

#[test]
fn vertical_long_unicode_lines_use_index_without_losing_preferred_column() {
    let first = format!("{}ab", "é".repeat(100_000));
    let third = format!("{}ab", "🐱".repeat(100_000));
    let (ctx, mut app) = editor(&format!("{first}\nx\n{third}"));
    app.move_to(first.len() - 1, false);
    app.event(key(Key::ArrowDown, false, false), &ctx);
    assert_eq!(app.preferred_column, Some(100_001));
    app.event(key(Key::ArrowDown, false, false), &ctx);
    assert_eq!(app.cursor, app.doc.len() - 1);
    assert_eq!(app.preferred_column, Some(100_001));
}

#[test]
fn autoindent_is_one_edit_and_preserves_bom_crlf() {
    let (ctx, mut app) = editor("\u{feff}  α\r\n\tb\r\n");
    app.move_to("\u{feff}  α".len(), false);
    let before = app.cursor_state();
    app.event(key(Key::Enter, false, false), &ctx);
    assert_eq!(text(&app), "\u{feff}  α\r\n  \r\n\tb\r\n");
    assert_eq!(app.cursor, "\u{feff}  α\r\n  ".len());
    app.undo(false);
    assert_eq!(text(&app), "\u{feff}  α\r\n\tb\r\n");
    assert_eq!(app.cursor_state(), before);
}

#[test]
fn newline_inside_indentation_copies_only_prefix_before_caret() {
    let (ctx, mut app) = editor("    α\n");
    app.move_to(2, false);
    app.event(key(Key::Enter, false, false), &ctx);
    assert_eq!(text(&app), "  \n    α\n");
    assert_eq!(app.cursor, 5);
}

#[test]
fn word_movement_and_deletion_obey_unicode_runs() {
    let (ctx, mut app) = editor("αβ_猫  ++ zz");
    app.event(key(Key::ArrowRight, false, true), &ctx);
    assert_eq!(app.cursor, "αβ_猫  ".len());
    app.event(key(Key::ArrowRight, false, true), &ctx);
    assert_eq!(app.cursor, "αβ_猫  ++ ".len());
    app.event(key(Key::ArrowRight, false, true), &ctx);
    assert_eq!(app.cursor, app.doc.len());
    app.event(key(Key::Backspace, false, true), &ctx);
    assert_eq!(text(&app), "αβ_猫  ++ ");
    app.undo(false);
    assert_eq!(text(&app), "αβ_猫  ++ zz");
    assert_eq!(app.cursor, app.doc.len());
    app.move_to(0, false);
    app.event(key(Key::Delete, false, true), &ctx);
    assert_eq!(text(&app), "++ zz");
}

#[test]
fn overlong_word_motion_never_partially_deletes() {
    let original = "a".repeat(70_000);
    let (ctx, mut app) = editor(&original);
    app.event(key(Key::Delete, false, true), &ctx);
    assert_eq!(text(&app), original);
    assert_eq!(app.cursor, 0);
    assert!(!app.doc.is_modified());
    assert!(app.status.contains("scan limit"));
}

#[test]
fn typing_groups_restore_cursor_and_stop_at_movement_paste_and_ime() {
    let (ctx, mut app) = editor("");
    for character in ["a", "猫", "b"] {
        app.event(egui::Event::Text(character.into()), &ctx);
    }
    assert_eq!(text(&app), "a猫b");
    let after = app.cursor;
    app.undo(false);
    assert_eq!(text(&app), "");
    assert_eq!(app.cursor, 0);
    app.undo(true);
    assert_eq!(app.cursor, after);
    app.event(key(Key::ArrowLeft, false, false), &ctx);
    app.event(egui::Event::Text("X".into()), &ctx);
    app.event(egui::Event::Paste("YY".into()), &ctx);
    app.event(egui::Event::Text("Z".into()), &ctx);
    app.event(egui::Event::Ime(egui::ImeEvent::Commit("語".into())), &ctx);
    assert_eq!(text(&app), "a猫XYYZ語b");
    for expected in ["a猫XYYZb", "a猫XYYb", "a猫Xb", "a猫b", ""] {
        app.undo(false);
        assert_eq!(text(&app), expected);
    }
}

#[test]
fn replacement_undo_redo_restores_reverse_selection_and_caret() {
    let (ctx, mut app) = editor("ab猫cd");
    app.cursor = 2;
    app.anchor = Some(5);
    let before = app.cursor_state();
    app.event(egui::Event::Text("💚".into()), &ctx);
    assert_eq!(text(&app), "ab💚cd");
    let after = app.cursor_state();
    app.move_to(0, false);
    app.undo(false);
    assert_eq!(app.cursor_state(), before);
    assert_eq!(app.selection(), 2..5);
    app.undo(true);
    assert_eq!(app.cursor_state(), after);
    assert_eq!(text(&app), "ab💚cd");
}

#[test]
fn scalar_backspace_undo_restores_no_synthetic_selection() {
    let (ctx, mut app) = editor("a💚\r\nb");
    app.move_to("a💚\r\n".len(), false);
    let before = app.cursor_state();
    app.event(key(Key::Backspace, false, false), &ctx);
    assert_eq!(text(&app), "a💚b");
    app.undo(false);
    assert_eq!(app.cursor_state(), before);
    assert!(app.anchor.is_none());
}

#[test]
fn ime_preserves_reverse_selection_and_deletes_only_surrounding_scalars() {
    let (ctx, mut app) = editor("a猫SELECT💚z");
    app.cursor = "a猫".len();
    app.anchor = Some("a猫SELECT".len());
    let before = app.cursor_state();
    app.event(ime(1, 1), &ctx);
    assert_eq!(text(&app), "aSELECTz");
    assert_eq!(app.cursor, 1);
    assert_eq!(app.anchor, Some(7));
    app.undo(false);
    assert_eq!(text(&app), "a猫SELECT💚z");
    assert_eq!(app.cursor_state(), before);
}

#[test]
fn history_eviction_is_visible_and_new_document_drops_cursor_metadata() {
    let (_, mut app) = editor(&"a".repeat(32 * 1024));
    app.doc.set_history_byte_limit(128 * 1024);
    for turn in 0..8 {
        app.anchor = Some(0);
        app.cursor = app.doc.len();
        app.insert(&if turn % 2 == 0 { "b" } else { "a" }.repeat(32 * 1024));
        assert!(app.error.is_none());
    }
    assert!(app.doc.history_evictions() > 0);
    assert!(app.status.contains("Undo history was trimmed"));
    app.doc = Document::new();
    app.reset_view();
    app.insert("fresh");
    app.undo(false);
    assert_eq!(text(&app), "");
    assert_eq!(app.cursor, 0);
    assert!(app.anchor.is_none());
    assert!(!app.status.contains("trimmed"));
}

#[test]
fn typing_after_saved_revision_remains_separately_undoable() {
    let (ctx, mut app) = editor("");
    app.event(egui::Event::Text("a".into()), &ctx);
    app.doc.mark_saved();
    app.event(egui::Event::Text("b".into()), &ctx);
    app.undo(false);
    assert_eq!(text(&app), "a");
    assert_eq!(app.cursor, 1);
    assert!(!app.doc.is_modified());
    app.undo(false);
    assert_eq!(text(&app), "");
}

#[test]
fn refused_vim_visual_edits_restore_selection_mode_and_register() {
    use simptxt::vim::{Mode, Register};
    for command in ["d", "c", "x"] {
        let (ctx, mut app) = editor("a💚b\r\nc");
        app.doc.set_history_byte_limit(0);
        app.vim_enabled = true;
        app.vim.set_mode(Mode::Visual);
        app.cursor = 1;
        app.anchor = Some(0);
        app.preferred_column = Some(7);
        app.vim_register = Register {
            text: "keep register".into(),
            linewise: true,
        };
        let before = app.cursor_state();
        let selection = app.selection();
        let revision = app.doc.revision();
        let register = app.vim_register.clone();
        app.event(egui::Event::Text(command.into()), &ctx);
        assert_eq!(text(&app), "a💚b\r\nc", "{command}");
        assert_eq!(app.doc.revision(), revision);
        assert!(!app.doc.is_modified());
        assert_eq!(app.cursor_state(), before);
        assert_eq!(app.selection(), selection);
        assert_eq!(app.preferred_column, Some(7));
        assert_eq!(app.vim.mode(), Mode::Visual);
        assert_eq!(app.vim_register, register);
        assert!(app.error.is_none());
        assert!(app.status.contains("undo"));
    }
}

#[test]
fn refused_vim_open_and_paste_never_enter_insert_or_execute_trailing_text() {
    use simptxt::vim::{Mode, Register};
    for command in ["oTRAIL", "OTRAIL", "p", "P"] {
        let (ctx, mut app) = editor("abc\r\nxyz");
        app.doc.set_history_byte_limit(0);
        app.vim_enabled = true;
        app.vim.set_mode(Mode::Normal);
        app.cursor = 1;
        app.vim_register = Register {
            text: "猫".into(),
            linewise: false,
        };
        let before = app.cursor_state();
        let revision = app.doc.revision();
        let register = app.vim_register.clone();
        app.event(egui::Event::Text(command.into()), &ctx);
        assert_eq!(text(&app), "abc\r\nxyz", "{command}");
        assert_eq!(app.doc.revision(), revision);
        assert_eq!(app.cursor_state(), before);
        assert_eq!(app.vim.mode(), Mode::Normal);
        assert_eq!(app.vim_register, register);
        assert!(app.error.is_none());
        assert!(app.status.contains("undo"));
    }
}

#[test]
fn refused_vim_normal_operators_preserve_caret_and_do_not_enter_insert() {
    use simptxt::vim::{Mode, Register};
    for command in ["cwTRAIL", "dd", "x"] {
        let (ctx, mut app) = editor("abc\nxyz");
        app.doc.set_history_byte_limit(0);
        app.vim_enabled = true;
        app.vim.set_mode(Mode::Normal);
        app.cursor = 1;
        app.vim_register = Register {
            text: "keep".into(),
            linewise: false,
        };
        let before = app.cursor_state();
        let revision = app.doc.revision();
        let register = app.vim_register.clone();
        app.event(egui::Event::Text(command.into()), &ctx);
        assert_eq!(text(&app), "abc\nxyz", "{command}");
        assert_eq!(app.doc.revision(), revision);
        assert_eq!(app.cursor_state(), before);
        assert_eq!(app.vim.mode(), Mode::Normal);
        assert_eq!(app.vim_register, register);
        assert!(app.error.is_none());
        assert!(app.status.contains("undo"));
    }
}
