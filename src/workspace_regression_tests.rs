//! Independent workspace regressions. Workers are completed through their real
//! per-tab channels so timing-sensitive ownership is tested deterministically.
use super::*;
use crate::preferences::{DEFAULT_FONT_SIZE, Theme};

fn setup() -> (egui::Context, WorkspaceApp) {
    let ctx = egui::Context::default();
    let app = WorkspaceApp::create(&ctx, false);
    (ctx, app)
}

fn editor(app: &mut WorkspaceApp, id: TabId) -> &mut EditorApp {
    let index = app.index(id).expect("test tab still exists");
    &mut app.tabs[index].editor
}

fn seed(app: &mut WorkspaceApp, id: TabId, text: &str) {
    let editor = editor(app, id);
    editor.doc = Document::from_bytes(text.as_bytes().to_vec()).unwrap();
    editor.cursor = view::content_start(&editor.doc);
}

fn bytes(app: &mut WorkspaceApp, id: TabId) -> Vec<u8> {
    let editor = editor(app, id);
    editor.doc.read(0..editor.doc.len())
}

fn work(label: &'static str) -> Work {
    Work {
        label,
        cancel: Arc::new(AtomicBool::new(false)),
        progress: Arc::new(AtomicU64::new(0)),
        total: 100,
    }
}

fn search_work(id: u64) -> SearchWork {
    SearchWork {
        id,
        cancel: Arc::new(AtomicBool::new(false)),
        progress: Arc::new(AtomicUsize::new(0)),
    }
}

fn key(key: Key, command: bool) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers {
            ctrl: command,
            command,
            ..Default::default()
        },
    }
}

fn render_frame(
    ctx: &egui::Context,
    app: &mut WorkspaceApp,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1100.0, 760.0))),
            events,
            ..Default::default()
        },
        |ui| app.render(ui),
    );
    output.textures_delta.clear();
    output
}

fn painted_text(output: &egui::FullOutput) -> Vec<(String, Rect)> {
    fn collect(shape: &egui::Shape, text: &mut Vec<(String, Rect)>) {
        match shape {
            egui::Shape::Text(shape) => {
                text.push((shape.galley.text().to_owned(), shape.visual_bounding_rect()));
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect(shape, text);
                }
            }
            _ => {}
        }
    }
    let mut text = Vec::new();
    for shape in &output.shapes {
        collect(&shape.shape, &mut text);
    }
    text
}

fn cancel_pending(app: &mut WorkspaceApp, id: TabId, ctx: &egui::Context) {
    let editor = editor(app, id);
    editor.pending = None;
    editor.continue_after_save = false;
    editor.workspace_actions.push(WorkspaceAction::CancelClose);
    app.process_actions(ctx);
}

fn await_loads(app: &mut WorkspaceApp, ctx: &egui::Context) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        app.poll(ctx);
        if app.tabs.iter().all(|tab| tab.editor.work.is_none()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "small fixture load did not finish"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(app.tabs.iter().all(|tab| tab.editor.ready));
}

#[test]
fn tab_switch_preserves_independent_cursor_selection_view_and_undo() {
    let (ctx, mut app) = setup();
    let first = app.active;
    seed(&mut app, first, "first α\nsecond\nthird");
    editor(&mut app, first).cursor = 5;
    editor(&mut app, first).insert("!");
    editor(&mut app, first).anchor = Some(1);
    editor(&mut app, first).top_line = 2;
    editor(&mut app, first).horizontal = 3;
    editor(&mut app, first).find = "first query".into();
    let cursor = editor(&mut app, first).cursor;
    let selection = editor(&mut app, first).selection();
    let second = app.new_tab(&ctx);
    seed(&mut app, second, "other 💚 document");
    editor(&mut app, second).cursor = 0;
    editor(&mut app, second).insert("second ");
    let second_bytes = bytes(&mut app, second);
    let second_cursor = editor(&mut app, second).cursor;
    app.switch_to(first);
    assert_eq!(editor(&mut app, first).cursor, cursor);
    assert_eq!(editor(&mut app, first).selection(), selection);
    assert_eq!(editor(&mut app, first).top_line, 2);
    assert_eq!(editor(&mut app, first).horizontal, 3);
    assert_eq!(editor(&mut app, first).find, "first query");
    editor(&mut app, first).undo(false);
    assert_eq!(bytes(&mut app, first), "first α\nsecond\nthird".as_bytes());
    assert!(!editor(&mut app, first).doc.is_modified());
    assert_eq!(bytes(&mut app, second), second_bytes);
    assert_eq!(editor(&mut app, second).cursor, second_cursor);
    assert!(editor(&mut app, second).doc.is_modified());
    app.switch_to(second);
    editor(&mut app, second).undo(false);
    assert_eq!(bytes(&mut app, second), "other 💚 document".as_bytes());
    editor(&mut app, first).undo(true);
    assert_eq!(bytes(&mut app, first), "first! α\nsecond\nthird".as_bytes());
}

#[test]
fn inactive_search_and_replacement_finish_only_in_their_owner() {
    let (ctx, mut app) = setup();
    let first = app.active;
    seed(&mut app, first, "alpha beta");
    let second = app.new_tab(&ctx);
    seed(&mut app, second, "other text");
    let first_editor = editor(&mut app, first);
    first_editor.search_work = Some(search_work(4));
    first_editor
        .tx
        .send(Message::Found(
            first_editor.generation,
            4,
            first_editor.doc.revision(),
            Ok(Some(6..10)),
        ))
        .unwrap();
    app.poll(&ctx);
    assert_eq!(app.active, second);
    assert_eq!(editor(&mut app, first).selection(), 6..10);
    assert_eq!(editor(&mut app, second).selection(), 0..0);
    let first_editor = editor(&mut app, first);
    first_editor.search_work = Some(search_work(5));
    first_editor
        .tx
        .send(Message::Replaced(
            first_editor.generation,
            5,
            first_editor.doc.revision(),
            Ok((Document::from_bytes(b"changed".to_vec()).unwrap(), 2)),
        ))
        .unwrap();
    app.poll(&ctx);
    assert_eq!(bytes(&mut app, first), b"changed");
    assert_eq!(bytes(&mut app, second), b"other text");
    assert!(!editor(&mut app, second).doc.is_modified());
    editor(&mut app, first).undo(false);
    assert_eq!(bytes(&mut app, first), b"alpha beta");
}

#[test]
fn inactive_highlight_completion_does_not_touch_active_worker() {
    let (ctx, mut app) = setup();
    let first = app.active;
    seed(&mut app, first, "fn alpha() {}");
    let second = app.new_tab(&ctx);
    seed(&mut app, second, "fn beta() {}");
    for id in [first, second] {
        let editor = editor(&mut app, id);
        editor.language = Language::Rust;
        editor.highlight_id = 9;
        editor.highlight_revision = Some(editor.doc.revision());
        editor.highlight_cancel = Some(Arc::new(AtomicBool::new(false)));
    }
    let second_cancel = editor(&mut app, second).highlight_cancel.clone().unwrap();
    let first_editor = editor(&mut app, first);
    let result =
        HighlightIndex::build(&first_editor.doc, Language::Rust, &AtomicBool::new(false)).unwrap();
    first_editor
        .tx
        .send(Message::Highlighted(
            first_editor.generation,
            first_editor.doc.revision(),
            9,
            true,
            Ok(result),
        ))
        .unwrap();
    app.poll(&ctx);
    assert!(editor(&mut app, first).highlight.is_some());
    assert!(editor(&mut app, first).highlight_cancel.is_none());
    assert!(editor(&mut app, second).highlight.is_none());
    assert!(Arc::ptr_eq(
        editor(&mut app, second).highlight_cancel.as_ref().unwrap(),
        &second_cancel
    ));
    assert!(!second_cancel.load(Ordering::Relaxed));
}

#[test]
fn inactive_loaded_result_keeps_both_documents_and_selection_separate() {
    let (ctx, mut app) = setup();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("loaded.txt");
    std::fs::write(&path, "loaded α\r\nfile").unwrap();
    let loaded = file_io::load(&path, &AtomicBool::new(false), &AtomicU64::new(0)).unwrap();
    let first = app.active;
    seed(&mut app, first, "preview");
    editor(&mut app, first).ready = false;
    editor(&mut app, first).work = Some(work("Opening"));
    let second = app.new_tab(&ctx);
    seed(&mut app, second, "active text");
    editor(&mut app, second).cursor = 3;
    let first_editor = editor(&mut app, first);
    first_editor
        .tx
        .send(Message::Loaded(first_editor.generation, Ok(loaded)))
        .unwrap();
    app.poll(&ctx);
    assert_eq!(app.active, second);
    assert_eq!(bytes(&mut app, first), "loaded α\r\nfile".as_bytes());
    assert!(editor(&mut app, first).ready);
    assert_eq!(bytes(&mut app, second), b"active text");
    assert_eq!(editor(&mut app, second).cursor, 3);
}

#[test]
fn inactive_save_completion_marks_only_origin_revision_clean() {
    let (ctx, mut app) = setup();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("saved.json");
    let first = app.active;
    seed(&mut app, first, "{}");
    editor(&mut app, first).insert(" ");
    let revision = editor(&mut app, first).doc.revision();
    std::fs::write(&path, bytes(&mut app, first)).unwrap();
    let stamp = FileStamp::read(&path).unwrap();
    editor(&mut app, first).work = Some(work("Saving"));
    editor(&mut app, first).save_destination = Some(path.clone());
    let second = app.new_tab(&ctx);
    seed(&mut app, second, "untouched");
    editor(&mut app, second).insert("new ");
    let first_editor = editor(&mut app, first);
    first_editor
        .tx
        .send(Message::Saved(
            first_editor.generation,
            revision,
            path.clone(),
            Ok(stamp),
        ))
        .unwrap();
    app.poll(&ctx);
    assert_eq!(app.active, second);
    assert_eq!(editor(&mut app, first).path.as_ref(), Some(&path));
    assert!(!editor(&mut app, first).doc.is_modified());
    assert!(editor(&mut app, first).save_destination.is_none());
    assert_eq!(editor(&mut app, first).language, Language::Json);
    assert_eq!(bytes(&mut app, second), b"new untouched");
    assert!(editor(&mut app, second).doc.is_modified());
    assert!(editor(&mut app, second).path.is_none());
}

#[test]
fn cancel_dirty_close_keeps_all_tabs_and_undo_histories() {
    let (ctx, mut app) = setup();
    let first = app.active;
    seed(&mut app, first, "one");
    editor(&mut app, first).insert("dirty ");
    let second = app.new_tab(&ctx);
    seed(&mut app, second, "two");
    editor(&mut app, second).insert("dirty ");
    app.request_close(first, &ctx);
    assert!(matches!(
        editor(&mut app, first).pending,
        Some(Pending::CloseTab)
    ));
    cancel_pending(&mut app, first, &ctx);
    assert_eq!(app.tabs.len(), 2);
    assert_eq!(bytes(&mut app, first), b"dirty one");
    assert_eq!(bytes(&mut app, second), b"dirty two");
    for id in [first, second] {
        editor(&mut app, id).undo(false);
        assert!(!editor(&mut app, id).doc.is_modified());
    }
}

#[test]
fn cancel_quit_after_first_discard_keeps_every_document() {
    let (ctx, mut app) = setup();
    let first = app.active;
    seed(&mut app, first, "one");
    editor(&mut app, first).insert("dirty ");
    let second = app.new_tab(&ctx);
    seed(&mut app, second, "two");
    editor(&mut app, second).insert("dirty ");
    app.request_quit();
    app.advance_quit(&ctx);
    assert_eq!(app.active, first);
    editor(&mut app, first).perform(Pending::QuitWorkspace, &ctx);
    app.process_actions(&ctx);
    app.advance_quit(&ctx);
    assert_eq!(app.active, second);
    assert!(matches!(
        editor(&mut app, second).pending,
        Some(Pending::QuitWorkspace)
    ));
    cancel_pending(&mut app, second, &ctx);
    assert!(app.quitting.is_none());
    assert!(!app.allow_close);
    assert_eq!(app.tabs.len(), 2);
    assert_eq!(bytes(&mut app, first), b"dirty one");
    assert_eq!(bytes(&mut app, second), b"dirty two");
    assert!(editor(&mut app, first).doc.is_modified());
}

#[test]
fn quit_approval_is_specific_to_the_approved_revision() {
    let (ctx, mut app) = setup();
    let first = app.active;
    editor(&mut app, first).insert("first edit");
    let second = app.new_tab(&ctx);
    editor(&mut app, second).insert("second edit");
    app.request_quit();
    app.advance_quit(&ctx);
    editor(&mut app, first).perform(Pending::QuitWorkspace, &ctx);
    app.process_actions(&ctx);
    // Any new revision invalidates the old discard decision, even if it was
    // produced by a queued operation rather than ordinary visible typing.
    editor(&mut app, first).insert(" later");
    app.advance_quit(&ctx);
    assert_eq!(app.active, first);
    assert!(matches!(
        editor(&mut app, first).pending,
        Some(Pending::QuitWorkspace)
    ));
    assert!(!app.allow_close);
}

#[test]
fn failed_save_during_quit_keeps_prompt_and_every_tab() {
    let (ctx, mut app) = setup();
    let first = app.active;
    editor(&mut app, first).insert("must survive");
    let second = app.new_tab(&ctx);
    seed(&mut app, second, "other");
    app.request_quit();
    app.advance_quit(&ctx);
    let first_editor = editor(&mut app, first);
    first_editor.work = Some(work("Saving"));
    first_editor.continue_after_save = true;
    first_editor
        .tx
        .send(Message::Saved(
            first_editor.generation,
            first_editor.doc.revision(),
            PathBuf::from("not-written.txt"),
            Err("injected disk full".into()),
        ))
        .unwrap();
    app.poll(&ctx);
    assert!(!app.allow_close);
    assert_eq!(app.tabs.len(), 2);
    assert_eq!(bytes(&mut app, first), b"must survive");
    assert!(editor(&mut app, first).doc.is_modified());
    assert!(matches!(
        editor(&mut app, first).pending,
        Some(Pending::QuitWorkspace)
    ));
    assert_eq!(app.quitting.as_ref().unwrap().prompted, Some(first));
    assert_eq!(
        editor(&mut app, first).error.as_deref(),
        Some("injected disk full")
    );
}

#[test]
fn saving_old_snapshot_during_quit_does_not_approve_newer_edits() {
    let (ctx, mut app) = setup();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("snapshot.txt");
    let first = app.active;
    editor(&mut app, first).insert("snapshot");
    let revision = editor(&mut app, first).doc.revision();
    std::fs::write(&path, bytes(&mut app, first)).unwrap();
    let stamp = FileStamp::read(&path).unwrap();
    app.request_quit();
    app.advance_quit(&ctx);
    let first_editor = editor(&mut app, first);
    first_editor.work = Some(work("Saving"));
    first_editor.continue_after_save = true;
    first_editor.insert(" newer");
    first_editor
        .tx
        .send(Message::Saved(
            first_editor.generation,
            revision,
            path,
            Ok(stamp),
        ))
        .unwrap();
    app.poll(&ctx);
    assert!(!app.allow_close);
    assert_eq!(bytes(&mut app, first), b"snapshot newer");
    assert!(editor(&mut app, first).doc.is_modified());
    assert!(matches!(
        editor(&mut app, first).pending,
        Some(Pending::QuitWorkspace)
    ));
    assert!(!app.quitting.as_ref().unwrap().approved.contains_key(&first));
    assert!(!editor(&mut app, first).continue_after_save);
}

#[test]
fn quit_waits_for_inactive_save_and_failure_cannot_close_window() {
    let (ctx, mut app) = setup();
    let first = app.active;
    editor(&mut app, first).insert("unsaved");
    editor(&mut app, first).work = Some(work("Saving"));
    let second = app.new_tab(&ctx);
    seed(&mut app, second, "clean");
    app.request_quit();
    app.advance_quit(&ctx);
    assert!(!app.allow_close);
    assert!(app.quitting.as_ref().unwrap().prompted.is_none());
    let first_editor = editor(&mut app, first);
    first_editor
        .tx
        .send(Message::Saved(
            first_editor.generation,
            first_editor.doc.revision(),
            PathBuf::from("not-written.txt"),
            Err("save failed".into()),
        ))
        .unwrap();
    app.poll(&ctx);
    assert!(!app.allow_close);
    assert_eq!(bytes(&mut app, first), b"unsaved");
    assert_eq!(app.tabs.len(), 2);
}

#[test]
fn close_loading_tab_cancels_jobs_and_late_completion_cannot_reach_replacement() {
    let (ctx, mut app) = setup();
    let first = app.active;
    let first_editor = editor(&mut app, first);
    first_editor.ready = false;
    first_editor.work = Some(work("Opening"));
    first_editor.search_work = Some(search_work(1));
    first_editor.highlight_cancel = Some(Arc::new(AtomicBool::new(false)));
    let load_cancel = first_editor.work.as_ref().unwrap().cancel.clone();
    let search_cancel = first_editor.search_work.as_ref().unwrap().cancel.clone();
    let highlight_cancel = first_editor.highlight_cancel.clone().unwrap();
    let late_sender = first_editor.tx.clone();
    app.request_close(first, &ctx);
    assert_eq!(app.tabs.len(), 1);
    assert_ne!(app.active, first, "replacement must have a fresh identity");
    assert!(load_cancel.load(Ordering::Relaxed));
    assert!(search_cancel.load(Ordering::Relaxed));
    assert!(highlight_cancel.load(Ordering::Relaxed));
    assert!(
        late_sender
            .send(Message::Loaded(0, Err("late error".into())))
            .is_err()
    );
    app.poll(&ctx);
    assert!(app.active_editor().doc.is_empty());
    assert!(app.active_editor().error.is_none());
}

#[test]
fn close_saving_tab_and_native_dialog_owner_are_not_removed() {
    let (ctx, mut app) = setup();
    let first = app.active;
    editor(&mut app, first).work = Some(work("Saving"));
    app.request_close(first, &ctx);
    assert!(app.index(first).is_some());
    assert!(editor(&mut app, first).status.contains("save"));
    editor(&mut app, first).work = None;
    editor(&mut app, first).dialog_pending = true;
    app.request_close(first, &ctx);
    assert!(app.index(first).is_some());
    assert!(editor(&mut app, first).dialog_pending);
}

#[test]
fn save_as_dialog_reply_keeps_original_owner_after_tab_switch() {
    let (ctx, mut app) = setup();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("existing.txt");
    std::fs::write(&path, b"disk bytes").unwrap();
    let first = app.active;
    editor(&mut app, first).insert("first content");
    editor(&mut app, first).dialog_pending = true;
    let second = app.new_tab(&ctx);
    editor(&mut app, second).insert("second content");
    editor(&mut app, first)
        .tx
        .send(Message::SaveDialog(Some(path.clone())))
        .unwrap();
    app.poll(&ctx);
    assert_eq!(editor(&mut app, first).overwrite.as_ref(), Some(&path));
    assert!(editor(&mut app, second).overwrite.is_none());
    assert_eq!(bytes(&mut app, first), b"first content");
    assert_eq!(bytes(&mut app, second), b"second content");
    assert_eq!(std::fs::read(path).unwrap(), b"disk bytes");
}

#[test]
fn save_as_rejects_open_loading_saving_and_overwrite_destinations() {
    for reservation in 0..3 {
        let (ctx, mut app) = setup();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reserved.txt");
        std::fs::write(&path, b"disk original").unwrap();
        let first = app.active;
        match reservation {
            0 => {
                editor(&mut app, first).path = Some(path.clone());
                editor(&mut app, first).ready = false;
                editor(&mut app, first).work = Some(work("Opening"));
            }
            1 => {
                editor(&mut app, first).save_destination = Some(path.clone());
                editor(&mut app, first).work = Some(work("Saving"));
            }
            _ => editor(&mut app, first).overwrite = Some(path.clone()),
        }
        let second = app.new_tab(&ctx);
        editor(&mut app, second).insert("do not overwrite");
        editor(&mut app, second).continue_after_save = true;
        app.save_path(
            second,
            directory.path().join(".").join("reserved.txt"),
            &ctx,
        );
        assert!(
            editor(&mut app, second)
                .error
                .as_ref()
                .unwrap()
                .contains("another tab")
        );
        assert!(editor(&mut app, second).work.is_none());
        assert!(editor(&mut app, second).overwrite.is_none());
        assert!(!editor(&mut app, second).continue_after_save);
        assert_eq!(bytes(&mut app, second), b"do not overwrite");
        assert_eq!(std::fs::read(path).unwrap(), b"disk original");
    }
}

#[test]
fn real_files_open_in_separate_tabs_and_duplicate_path_focuses_owner() {
    let (ctx, mut app) = setup();
    let directory = tempfile::tempdir().unwrap();
    let first_path = directory.path().join("first.txt");
    let second_path = directory.path().join("second.txt");
    std::fs::write(&first_path, "\u{feff}first α\r\n").unwrap();
    std::fs::write(&second_path, "second 💚\n").unwrap();
    app.open_path(first_path.clone(), &ctx);
    let first = app.active;
    app.open_path(second_path, &ctx);
    let second = app.active;
    await_loads(&mut app, &ctx);
    assert_eq!(app.tabs.len(), 2);
    assert_eq!(bytes(&mut app, first), "\u{feff}first α\r\n".as_bytes());
    assert_eq!(bytes(&mut app, second), "second 💚\n".as_bytes());
    editor(&mut app, first).insert("edited ");
    let edited = bytes(&mut app, first);
    app.open_path(directory.path().join(".").join("first.txt"), &ctx);
    assert_eq!(app.tabs.len(), 2);
    assert_eq!(app.active, first);
    assert_eq!(bytes(&mut app, first), edited);
    assert!(editor(&mut app, first).doc.is_modified());
    assert_eq!(
        std::fs::read(first_path).unwrap(),
        "\u{feff}first α\r\n".as_bytes()
    );
}

#[cfg(unix)]
#[test]
fn existing_hard_link_focuses_original_but_symlink_is_not_silently_followed() {
    use std::os::unix::fs::symlink;
    let (ctx, mut app) = setup();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("file.txt");
    let hard_link = directory.path().join("hard-link.txt");
    let symbolic_link = directory.path().join("symbolic-link.txt");
    std::fs::write(&path, b"original").unwrap();
    std::fs::hard_link(&path, &hard_link).unwrap();
    symlink(&path, &symbolic_link).unwrap();
    app.open_path(path, &ctx);
    let first = app.active;
    await_loads(&mut app, &ctx);
    let second = app.new_tab(&ctx);
    app.open_path(hard_link, &ctx);
    assert_eq!(app.active, first);
    assert_eq!(app.tabs.len(), 2);
    app.switch_to(second);
    app.open_path(symbolic_link, &ctx);
    assert_eq!(app.active, second);
    assert_eq!(app.tabs.len(), 2);
    assert!(editor(&mut app, second).error.is_some());
}

#[test]
fn zoom_events_do_not_type_scroll_select_dirty_or_join_undo_history() {
    let (ctx, mut app) = setup();
    let first = app.active;
    seed(&mut app, first, &"line α\n".repeat(200));
    editor(&mut app, first).cursor = 10;
    editor(&mut app, first).anchor = Some(3);
    editor(&mut app, first).top_line = 1;
    editor(&mut app, first).horizontal = 1;
    let original = bytes(&mut app, first);
    let revision = editor(&mut app, first).doc.revision();
    let selection = editor(&mut app, first).selection();
    let top_line = editor(&mut app, first).top_line;
    let horizontal = editor(&mut app, first).horizontal;
    for (zoom_key, text) in [(Key::Plus, "+"), (Key::Minus, "-"), (Key::Num0, "0")] {
        render_frame(
            &ctx,
            &mut app,
            vec![key(zoom_key, true), egui::Event::Text(text.into())],
        );
    }
    assert_eq!(app.preferences.font_size, DEFAULT_FONT_SIZE);
    render_frame(
        &ctx,
        &mut app,
        vec![egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: Vec2::new(0.0, 1.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::CTRL,
        }],
    );
    assert_eq!(app.preferences.font_size, DEFAULT_FONT_SIZE + 1.0);
    assert_eq!(bytes(&mut app, first), original);
    assert_eq!(editor(&mut app, first).doc.revision(), revision);
    assert_eq!(editor(&mut app, first).selection(), selection);
    assert_eq!(editor(&mut app, first).top_line, top_line);
    assert_eq!(editor(&mut app, first).horizontal, horizontal);
    assert!(!editor(&mut app, first).doc.is_modified());
    assert!(!editor(&mut app, first).doc.undo());
    assert_eq!(ctx.zoom_factor(), 1.0);
}

#[test]
fn theme_change_and_new_tab_keep_existing_bytes_revisions_and_zoom() {
    let (ctx, mut app) = setup();
    let first = app.active;
    seed(&mut app, first, "\u{feff}one\r\ntwo 💚\n");
    let original = bytes(&mut app, first);
    let revision = editor(&mut app, first).doc.revision();
    app.preferences.font_size = 21.0;
    app.preferences.theme = Theme::Light;
    app.preferences.apply_theme(&ctx);
    let second = app.new_tab(&ctx);
    assert_eq!(ctx.theme(), egui::Theme::Light);
    let preferences = app.preferences;
    assert_eq!(editor(&mut app, second).preferences, preferences);
    app.switch_to(first);
    render_frame(&ctx, &mut app, Vec::new());
    assert_eq!(editor(&mut app, first).preferences, preferences);
    assert_eq!(bytes(&mut app, first), original);
    assert_eq!(editor(&mut app, first).doc.revision(), revision);
    assert!(!editor(&mut app, first).doc.is_modified());
    assert!(!editor(&mut app, first).doc.undo());
}

fn enable_vim(app: &mut WorkspaceApp) {
    app.vim_enabled = true;
    for tab in &mut app.tabs {
        tab.editor.vim_enabled = true;
        tab.editor.cancel_vim_input();
    }
}

fn text(app: &mut WorkspaceApp, id: TabId, command: &str, ctx: &egui::Context) {
    editor(app, id).event(egui::Event::Text(command.into()), ctx);
}

#[test]
fn vim_key_and_text_pairs_execute_printable_commands_exactly_once() {
    let (ctx, mut app) = setup();
    let first = app.active;
    seed(&mut app, first, "a💚b\r\nnext");
    enable_vim(&mut app);
    editor(&mut app, first).event(key(Key::X, false), &ctx);
    assert_eq!(bytes(&mut app, first), "a💚b\r\nnext".as_bytes());
    text(&mut app, first, "x", &ctx);
    assert_eq!(bytes(&mut app, first), "💚b\r\nnext".as_bytes());
    editor(&mut app, first).event(key(Key::I, false), &ctx);
    text(&mut app, first, "i", &ctx);
    assert_eq!(
        editor(&mut app, first).vim.mode(),
        simptxt::vim::Mode::Insert
    );
    editor(&mut app, first).event(key(Key::A, false), &ctx);
    text(&mut app, first, "α", &ctx);
    assert_eq!(bytes(&mut app, first), "α💚b\r\nnext".as_bytes());
    editor(&mut app, first).event(key(Key::Escape, false), &ctx);
    assert_eq!(
        editor(&mut app, first).vim.mode(),
        simptxt::vim::Mode::Normal
    );
    let first_editor = editor(&mut app, first);
    assert!(first_editor.doc.char_boundary(first_editor.cursor));
}

#[test]
fn vim_normal_ime_paste_cut_and_unfocused_text_never_edit() {
    let (ctx, mut app) = setup();
    let first = app.active;
    seed(&mut app, first, "keep 💚\r\nnext");
    enable_vim(&mut app);
    let original = bytes(&mut app, first);
    for event in [
        egui::Event::Ime(egui::ImeEvent::Preedit {
            text: "dd".into(),
            active_range_chars: None,
        }),
        egui::Event::Ime(egui::ImeEvent::Commit("dd".into())),
        egui::Event::Ime(egui::ImeEvent::DeleteSurrounding {
            before_chars: 100,
            after_chars: 100,
        }),
        egui::Event::Paste("dd".into()),
        egui::Event::Cut,
    ] {
        editor(&mut app, first).event(event, &ctx);
        assert_eq!(bytes(&mut app, first), original);
        assert!(editor(&mut app, first).composition.is_empty());
    }
    editor(&mut app, first).editor_focus = false;
    text(&mut app, first, "dd", &ctx);
    assert_eq!(bytes(&mut app, first), original);
    assert!(!editor(&mut app, first).doc.is_modified());
}

#[test]
fn vim_insert_ime_is_single_undo_and_does_not_duplicate_enter() {
    let (ctx, mut app) = setup();
    let first = app.active;
    seed(&mut app, first, "prefix \r\n");
    enable_vim(&mut app);
    text(&mut app, first, "A", &ctx);
    editor(&mut app, first).event(
        egui::Event::Ime(egui::ImeEvent::Preedit {
            text: "かな".into(),
            active_range_chars: None,
        }),
        &ctx,
    );
    editor(&mut app, first).event(key(Key::Enter, false), &ctx);
    editor(&mut app, first).event(key(Key::ArrowLeft, false), &ctx);
    assert_eq!(bytes(&mut app, first), b"prefix \r\n");
    editor(&mut app, first).event(
        egui::Event::Ime(egui::ImeEvent::Commit("仮名".into())),
        &ctx,
    );
    assert_eq!(bytes(&mut app, first), "prefix 仮名\r\n".as_bytes());
    editor(&mut app, first).event(key(Key::Escape, false), &ctx);
    text(&mut app, first, "u", &ctx);
    assert_eq!(bytes(&mut app, first), b"prefix \r\n");
    assert!(!editor(&mut app, first).doc.is_modified());
}

#[test]
fn vim_x_dd_and_p_preserve_utf8_bom_crlf_and_final_newline_choice() {
    let original = "\u{feff}α💚\r\nsecond\r\nlast";
    let (ctx, mut app) = setup();
    let first = app.active;
    seed(&mut app, first, original);
    enable_vim(&mut app);
    text(&mut app, first, "x", &ctx);
    assert_eq!(
        bytes(&mut app, first),
        "\u{feff}💚\r\nsecond\r\nlast".as_bytes()
    );
    assert_eq!(editor(&mut app, first).vim_register.text, "α");
    text(&mut app, first, "u", &ctx);
    text(&mut app, first, "0dd", &ctx);
    assert_eq!(bytes(&mut app, first), "\u{feff}second\r\nlast".as_bytes());
    assert_eq!(editor(&mut app, first).vim_register.text, "α💚\r\n");
    assert!(editor(&mut app, first).vim_register.linewise);
    text(&mut app, first, "P", &ctx);
    assert_eq!(bytes(&mut app, first), original.as_bytes());
    text(&mut app, first, "Gdd", &ctx);
    assert_eq!(bytes(&mut app, first), "\u{feff}α💚\r\nsecond".as_bytes());
    assert_eq!(editor(&mut app, first).vim_register.text, "last");
    text(&mut app, first, "p", &ctx);
    assert_eq!(bytes(&mut app, first), original.as_bytes());
    let first_editor = editor(&mut app, first);
    assert!(first_editor.doc.char_boundary(first_editor.cursor));
    assert!(editor(&mut app, first).cursor >= 3);
}

#[test]
fn vim_x_at_last_character_or_empty_line_never_eats_crlf() {
    let (ctx, mut app) = setup();
    let first = app.active;
    seed(&mut app, first, "α\r\n\r\nz");
    enable_vim(&mut app);
    text(&mut app, first, "1000x", &ctx);
    assert_eq!(bytes(&mut app, first), b"\r\n\r\nz");
    let revision = editor(&mut app, first).doc.revision();
    text(&mut app, first, "x", &ctx);
    assert_eq!(bytes(&mut app, first), b"\r\n\r\nz");
    assert_eq!(editor(&mut app, first).doc.revision(), revision);
    assert_eq!(editor(&mut app, first).vim_register.text, "α");
}

#[test]
fn vim_tab_switch_cancels_operator_count_composition_and_insert_mode() {
    let (ctx, mut app) = setup();
    let first = app.active;
    seed(&mut app, first, "one\ntwo\nthree");
    let second = app.new_tab(&ctx);
    seed(&mut app, second, "other\nfile");
    enable_vim(&mut app);
    app.switch_to(first);
    text(&mut app, first, "2d", &ctx);
    app.switch_to(second);
    text(&mut app, second, "d", &ctx);
    app.switch_to(first);
    text(&mut app, first, "d", &ctx);
    assert_eq!(bytes(&mut app, first), b"one\ntwo\nthree");
    text(&mut app, first, "d", &ctx);
    assert_eq!(
        bytes(&mut app, first),
        b"two\nthree",
        "old count must not survive a tab switch"
    );
    app.switch_to(second);
    text(&mut app, second, "d", &ctx);
    assert_eq!(
        bytes(&mut app, second),
        b"other\nfile",
        "other tab's operator must also reset"
    );
    editor(&mut app, second).event(key(Key::Escape, false), &ctx);
    text(&mut app, second, "i", &ctx);
    editor(&mut app, second).event(
        egui::Event::Ime(egui::ImeEvent::Preedit {
            text: "かな".into(),
            active_range_chars: None,
        }),
        &ctx,
    );
    app.switch_to(first);
    assert_eq!(
        editor(&mut app, second).vim.mode(),
        simptxt::vim::Mode::Normal
    );
    assert!(editor(&mut app, second).composition.is_empty());
}

#[test]
fn vim_register_and_visual_selection_belong_to_their_tab() {
    let (ctx, mut app) = setup();
    let first = app.active;
    seed(&mut app, first, "α💚z");
    let second = app.new_tab(&ctx);
    seed(&mut app, second, "other");
    enable_vim(&mut app);
    app.switch_to(first);
    text(&mut app, first, "vld", &ctx);
    assert_eq!(bytes(&mut app, first), b"z");
    assert_eq!(editor(&mut app, first).vim_register.text, "α💚");
    app.switch_to(second);
    text(&mut app, second, "p", &ctx);
    assert_eq!(bytes(&mut app, second), b"other");
    app.switch_to(first);
    text(&mut app, first, "P", &ctx);
    assert_eq!(bytes(&mut app, first), "α💚z".as_bytes());
    text(&mut app, first, "vl", &ctx);
    app.switch_to(second);
    assert!(editor(&mut app, first).anchor.is_none());
    assert_eq!(
        editor(&mut app, first).vim.mode(),
        simptxt::vim::Mode::Normal
    );
}

#[test]
fn vim_readonly_and_modal_states_block_all_command_mutations() {
    let (ctx, mut app) = setup();
    let first = app.active;
    seed(&mut app, first, "preview\r\nbytes");
    enable_vim(&mut app);
    editor(&mut app, first).ready = false;
    for command in ["dd", "x", "iBAD", "p", ":q!"] {
        text(&mut app, first, command, &ctx);
        editor(&mut app, first).event(key(Key::Enter, false), &ctx);
    }
    assert_eq!(bytes(&mut app, first), b"preview\r\nbytes");
    assert!(editor(&mut app, first).workspace_actions.is_empty());
    editor(&mut app, first).ready = true;
    editor(&mut app, first).pending = Some(Pending::CloseTab);
    text(&mut app, first, "dd", &ctx);
    assert_eq!(bytes(&mut app, first), b"preview\r\nbytes");
    assert!(!editor(&mut app, first).doc.is_modified());
}

#[test]
fn vim_quit_requests_confirmation_and_force_quit_only_closes_owner() {
    let (ctx, mut app) = setup();
    let first = app.active;
    editor(&mut app, first).insert("unsaved first");
    let second = app.new_tab(&ctx);
    editor(&mut app, second).insert("unsaved second");
    enable_vim(&mut app);
    app.switch_to(first);
    text(&mut app, first, ":q", &ctx);
    editor(&mut app, first).event(key(Key::Enter, false), &ctx);
    app.process_actions(&ctx);
    assert!(matches!(
        editor(&mut app, first).pending,
        Some(Pending::CloseTab)
    ));
    assert_eq!(app.tabs.len(), 2);
    cancel_pending(&mut app, first, &ctx);
    text(&mut app, first, ":q!", &ctx);
    editor(&mut app, first).event(key(Key::Enter, false), &ctx);
    app.process_actions(&ctx);
    assert!(app.index(first).is_none());
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(bytes(&mut app, second), b"unsaved second");
    assert!(editor(&mut app, second).doc.is_modified());
    assert!(!app.allow_close);
}

#[test]
fn vim_write_quit_failure_retains_tab_and_current_document() {
    let (ctx, mut app) = setup();
    let directory = tempfile::tempdir().unwrap();
    let first = app.active;
    editor(&mut app, first).insert("must survive");
    // A directory cannot be an editable regular-file save destination.
    editor(&mut app, first).path = Some(directory.path().to_path_buf());
    enable_vim(&mut app);
    text(&mut app, first, ":wq", &ctx);
    editor(&mut app, first).event(key(Key::Enter, false), &ctx);
    app.process_actions(&ctx);
    let deadline = Instant::now() + Duration::from_secs(5);
    while editor(&mut app, first).work.is_some() {
        assert!(Instant::now() < deadline, "failed save did not finish");
        app.poll(&ctx);
        std::thread::sleep(Duration::from_millis(1));
    }
    app.poll(&ctx);
    assert!(app.index(first).is_some());
    assert_eq!(bytes(&mut app, first), b"must survive");
    assert!(editor(&mut app, first).doc.is_modified());
    assert!(editor(&mut app, first).error.is_some());
    assert!(matches!(
        editor(&mut app, first).pending,
        Some(Pending::CloseTab)
    ));
    assert!(!editor(&mut app, first).continue_after_save);
}

#[test]
fn reload_instead_of_quit_save_cancels_quit_without_stranding_other_tabs() {
    for discard_reload in [false, true] {
        let (ctx, mut app) = setup();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("externally-changed.txt");
        std::fs::write(&path, b"old disk").unwrap();
        app.open_path(path.clone(), &ctx);
        await_loads(&mut app, &ctx);
        let first = app.active;
        editor(&mut app, first).insert("unsaved ");
        let second = app.new_tab(&ctx);
        editor(&mut app, second).insert("other unsaved");
        std::fs::write(&path, b"new disk content with distinct length").unwrap();
        app.request_quit();
        app.advance_quit(&ctx);
        assert_eq!(app.quitting.as_ref().unwrap().prompted, Some(first));
        editor(&mut app, first).continue_after_save = true;
        editor(&mut app, first).save(&ctx);
        app.process_actions(&ctx);
        assert!(editor(&mut app, first).conflict.is_some());
        // This is the action taken by the conflict modal's Reload disk version.
        editor(&mut app, first).conflict = None;
        editor(&mut app, first).request(Pending::Reload, &ctx);
        app.process_actions(&ctx);
        assert!(matches!(
            editor(&mut app, first).pending,
            Some(Pending::Reload)
        ));
        if discard_reload {
            editor(&mut app, first).perform(Pending::Reload, &ctx);
            await_loads(&mut app, &ctx);
            assert_eq!(
                bytes(&mut app, first),
                b"new disk content with distinct length"
            );
        } else {
            // Reload's own Cancel is not a tab-close/quit cancellation button.
            editor(&mut app, first).pending = None;
            editor(&mut app, first).continue_after_save = false;
            assert_eq!(bytes(&mut app, first), b"unsaved old disk");
        }
        app.poll(&ctx);
        assert!(
            app.quitting.is_none(),
            "Reload replaced the prompt; quit must not retain its old owner"
        );
        assert!(!app.allow_close);
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(bytes(&mut app, second), b"other unsaved");
        assert!(editor(&mut app, second).doc.is_modified());
    }
}

fn bracket_pair(app: &mut WorkspaceApp, id: TabId) -> simptxt::brackets::PairResult {
    let editor = editor(app, id);
    editor.bracket_cache.resolve(
        &editor.doc,
        editor.cursor,
        editor.language,
        editor.highlight.as_ref(),
    )
}

#[test]
fn bracket_pair_caches_are_tab_local_and_never_change_bytes_or_undo() {
    use simptxt::brackets::PairResult;
    let (ctx, mut app) = setup();
    let first = app.active;
    seed(&mut app, first, "\u{feff}(α💚)\r\n");
    let first_original = bytes(&mut app, first);
    let first_close = first_original
        .iter()
        .position(|byte| *byte == b')')
        .unwrap();
    assert_eq!(
        bracket_pair(&mut app, first),
        PairResult::Match {
            open: 3,
            close: first_close
        }
    );
    let second = app.new_tab(&ctx);
    seed(&mut app, second, "[x)");
    assert_eq!(bracket_pair(&mut app, second), PairResult::NoPair);
    app.switch_to(first);
    assert_eq!(
        bracket_pair(&mut app, first),
        PairResult::Match {
            open: 3,
            close: first_close
        }
    );
    render_frame(&ctx, &mut app, Vec::new());
    assert_eq!(bytes(&mut app, first), first_original);
    assert_eq!(bytes(&mut app, second), b"[x)");
    for id in [first, second] {
        assert!(!editor(&mut app, id).doc.is_modified());
        assert!(!editor(&mut app, id).doc.undo());
    }
}

#[test]
fn bracket_cache_follows_syntax_completion_and_edit_invalidation() {
    use simptxt::brackets::PairResult;
    let (ctx, mut app) = setup();
    let first = app.active;
    let original = "( /* ) */ [💚] )";
    seed(&mut app, first, original);
    editor(&mut app, first).language = Language::Rust;
    assert_eq!(bracket_pair(&mut app, first), PairResult::UnknownSyntax);
    let first_editor = editor(&mut app, first);
    first_editor.highlight_id = 1;
    first_editor.highlight_revision = Some(first_editor.doc.revision());
    let index =
        HighlightIndex::build(&first_editor.doc, Language::Rust, &AtomicBool::new(false)).unwrap();
    first_editor
        .tx
        .send(Message::Highlighted(
            first_editor.generation,
            first_editor.doc.revision(),
            1,
            true,
            Ok(index),
        ))
        .unwrap();
    app.poll(&ctx);
    assert_eq!(
        bracket_pair(&mut app, first),
        PairResult::Match {
            open: 0,
            close: original.len() - 1
        }
    );
    editor(&mut app, first).insert("(");
    editor(&mut app, first).cursor = 0;
    assert_eq!(bracket_pair(&mut app, first), PairResult::UnknownSyntax);
    editor(&mut app, first).undo(false);
    assert_eq!(bytes(&mut app, first), original.as_bytes());
    assert_eq!(bracket_pair(&mut app, first), PairResult::UnknownSyntax);
    assert!(!editor(&mut app, first).doc.is_modified());
}

#[test]
fn external_widget_focus_blocks_editor_text_and_delete_but_not_global_zoom() {
    let (ctx, mut app) = setup();
    let first = app.active;
    seed(&mut app, first, "keep editor text");
    let original = bytes(&mut app, first);
    let widget = egui::Id::new("independent_sidebar_focus_target");
    // Reproduce a focusable sidebar/control widget owning egui keyboard focus
    // while the editor's own last-click flag remains true.
    ctx.memory_mut(|memory| memory.request_focus(widget));
    render_frame(
        &ctx,
        &mut app,
        vec![egui::Event::Text("lost".into()), key(Key::Delete, false)],
    );
    assert_eq!(bytes(&mut app, first), original);
    assert!(!editor(&mut app, first).doc.is_modified());
    editor(&mut app, first).editor_focus = true;
    ctx.memory_mut(|memory| memory.request_focus(widget));
    render_frame(
        &ctx,
        &mut app,
        vec![key(Key::Plus, true), egui::Event::Text("+".into())],
    );
    assert_eq!(app.preferences.font_size, DEFAULT_FONT_SIZE + 1.0);
    assert_eq!(bytes(&mut app, first), original);
}

#[test]
fn vim_visual_copy_includes_one_or_two_unicode_characters_without_mutation() {
    for (command, expected) in [("v", "α"), ("vl", "α💚")] {
        let (ctx, mut app) = setup();
        let first = app.active;
        seed(&mut app, first, "\u{feff}α💚z\r\n");
        enable_vim(&mut app);
        editor(&mut app, first).vim_register = simptxt::vim::Register {
            text: "retained register".into(),
            linewise: false,
        };
        text(&mut app, first, command, &ctx);
        let selection = editor(&mut app, first).selection();
        editor(&mut app, first).event(egui::Event::Copy, &ctx);
        let copied: Vec<_> = ctx.output(|output| {
            output
                .commands
                .iter()
                .filter_map(|command| {
                    if let egui::OutputCommand::CopyText(text) = command {
                        Some(text.clone())
                    } else {
                        None
                    }
                })
                .collect()
        });
        assert_eq!(copied, vec![expected]);
        assert_eq!(bytes(&mut app, first), "\u{feff}α💚z\r\n".as_bytes());
        assert_eq!(
            editor(&mut app, first).vim.mode(),
            simptxt::vim::Mode::Visual
        );
        assert_eq!(editor(&mut app, first).selection(), selection);
        assert_eq!(
            editor(&mut app, first).vim_register.text,
            "retained register"
        );
        assert!(!editor(&mut app, first).doc.is_modified());
    }
}

#[test]
fn control_tab_and_close_shortcuts_cannot_insert_printable_text_or_tabs() {
    let (ctx, mut app) = setup();
    let first = app.active;
    seed(&mut app, first, "first");
    let second = app.new_tab(&ctx);
    seed(&mut app, second, "second");
    app.switch_to(first);
    for (shortcut, text, expected, count) in [
        (Key::Tab, "\t", second, 2),
        (Key::W, "w", first, 1),
        (Key::Tab, "\t", first, 1),
    ] {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1100.0, 760.0))),
                events: vec![
                    egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                    key(shortcut, true),
                    egui::Event::Text(text.into()),
                ],
                ..Default::default()
            },
            |ui| app.render(ui),
        );
        output.textures_delta.clear();
        assert_eq!(app.active, expected);
        assert_eq!(app.tabs.len(), count);
        assert_eq!(bytes(&mut app, first), b"first");
        assert!(!editor(&mut app, first).doc.is_modified());
        if app.index(second).is_some() {
            assert_eq!(bytes(&mut app, second), b"second");
            assert!(!editor(&mut app, second).doc.is_modified());
        }
    }
}

#[test]
fn quitting_cancels_every_tab_search_and_rejects_already_queued_replace_all() {
    let (ctx, mut app) = setup();
    let first = app.active;
    seed(&mut app, first, "first");
    editor(&mut app, first).insert("dirty ");
    let second = app.new_tab(&ctx);
    seed(&mut app, second, "second");
    editor(&mut app, second).insert("dirty ");
    let mut cancellation = Vec::new();
    for id in [first, second] {
        let editor = editor(&mut app, id);
        editor.search_work = Some(search_work(3));
        cancellation.push(editor.search_work.as_ref().unwrap().cancel.clone());
        editor
            .tx
            .send(Message::Replaced(
                editor.generation,
                3,
                editor.doc.revision(),
                Ok((
                    Document::from_bytes(b"late replacement".to_vec()).unwrap(),
                    1,
                )),
            ))
            .unwrap();
    }
    app.request_quit();
    app.poll(&ctx);
    assert_eq!(bytes(&mut app, first), b"dirty first");
    assert_eq!(bytes(&mut app, second), b"dirty second");
    assert!(
        cancellation
            .iter()
            .all(|cancel| cancel.load(Ordering::Relaxed))
    );
    assert!(editor(&mut app, first).search_work.is_none());
    assert!(editor(&mut app, second).search_work.is_none());
    assert!(!app.allow_close);
}

#[test]
fn window_focus_loss_resets_vim_command_prefix_and_composition() {
    let (ctx, mut app) = setup();
    let first = app.active;
    seed(&mut app, first, "one\ntwo\nthree");
    enable_vim(&mut app);
    text(&mut app, first, "2d", &ctx);
    editor(&mut app, first).event(egui::Event::WindowFocused(false), &ctx);
    text(&mut app, first, "d", &ctx);
    assert_eq!(bytes(&mut app, first), b"one\ntwo\nthree");
    text(&mut app, first, "d", &ctx);
    assert_eq!(bytes(&mut app, first), b"two\nthree");
    text(&mut app, first, "i", &ctx);
    editor(&mut app, first).event(
        egui::Event::Ime(egui::ImeEvent::Preedit {
            text: "かな".into(),
            active_range_chars: None,
        }),
        &ctx,
    );
    editor(&mut app, first).event(egui::Event::WindowFocused(false), &ctx);
    assert!(editor(&mut app, first).composition.is_empty());
    assert_eq!(
        editor(&mut app, first).vim.mode(),
        simptxt::vim::Mode::Normal
    );
}

#[test]
fn tab_limit_rejects_new_and_open_without_replacing_clean_or_dirty_tabs() {
    let (ctx, mut app) = setup();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("must-not-replace-active.txt");
    std::fs::write(&path, b"new file bytes").unwrap();
    for index in 0..MAX_TABS {
        let id = if index == 0 {
            app.active
        } else {
            app.new_tab(&ctx)
        };
        seed(&mut app, id, &format!("tab {index}"));
        if index.is_multiple_of(2) {
            editor(&mut app, id).insert("dirty ");
        }
    }
    let active = app.active;
    let before: Vec<_> = app
        .tabs
        .iter()
        .map(|tab| {
            (
                tab.id,
                tab.editor.doc.revision(),
                tab.editor.doc.read(0..tab.editor.doc.len()),
                tab.editor.doc.is_modified(),
            )
        })
        .collect();
    assert_eq!(app.new_tab(&ctx), active);
    assert_eq!(app.tabs.len(), MAX_TABS);
    assert!(
        editor(&mut app, active)
            .error
            .as_ref()
            .unwrap()
            .contains("128")
    );
    editor(&mut app, active).error = None;
    app.open_path(path, &ctx);
    assert_eq!(app.active, active);
    assert_eq!(app.tabs.len(), MAX_TABS);
    assert!(
        editor(&mut app, active)
            .error
            .as_ref()
            .unwrap()
            .contains("128")
    );
    for (id, revision, original, modified) in before {
        assert_eq!(bytes(&mut app, id), original);
        assert_eq!(editor(&mut app, id).doc.revision(), revision);
        assert_eq!(editor(&mut app, id).doc.is_modified(), modified);
        assert!(editor(&mut app, id).work.is_none());
        assert!(editor(&mut app, id).path.is_none());
    }
}

#[test]
fn reload_resource_refusals_preserve_dirty_bytes_revision_and_undo() {
    for exhaust_bytes in [true, false] {
        let (ctx, mut app) = setup();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reload.txt");
        std::fs::write(&path, b"replacement file is larger than five bytes").unwrap();
        let first = app.active;
        seed(&mut app, first, "original buffer");
        editor(&mut app, first).path = Some(path.clone());
        editor(&mut app, first).insert("dirty ");
        let revision = editor(&mut app, first).doc.revision();
        let second = app.new_tab(&ctx);
        let mut reservation = work("Opening");
        reservation.total = if exhaust_bytes {
            MAX_OPEN_DOCUMENT_BYTES - 5
        } else {
            100
        };
        editor(&mut app, second).work = Some(reservation);
        editor(&mut app, second).ready = false;
        if !exhaust_bytes {
            let third = app.new_tab(&ctx);
            editor(&mut app, third).work = Some(work("Saving"));
        }
        editor(&mut app, first).request(Pending::Reload, &ctx);
        assert!(matches!(
            editor(&mut app, first).pending,
            Some(Pending::Reload)
        ));
        // Explicitly approve discarding for Reload. Admission must still happen
        // before replacing this buffer, not after the preview has taken over.
        editor(&mut app, first).perform(Pending::Reload, &ctx);
        app.process_actions(&ctx);
        assert_eq!(bytes(&mut app, first), b"dirty original buffer");
        assert_eq!(editor(&mut app, first).doc.revision(), revision);
        assert!(editor(&mut app, first).doc.is_modified());
        assert!(editor(&mut app, first).work.is_none());
        assert!(editor(&mut app, first).ready);
        assert_eq!(editor(&mut app, first).path.as_ref(), Some(&path));
        let error = editor(&mut app, first).error.as_ref().unwrap();
        assert!(error.contains(if exhaust_bytes {
            "2 GiB"
        } else {
            "Two file operations"
        }));
        editor(&mut app, first).undo(false);
        assert_eq!(bytes(&mut app, first), b"original buffer");
        assert!(!editor(&mut app, first).doc.is_modified());
        assert_eq!(
            std::fs::read(path).unwrap(),
            b"replacement file is larger than five bytes"
        );
    }
}

#[test]
fn queued_sidebar_selection_waits_for_modal_to_close_then_opens_only_once() {
    for modal in 0..3 {
        let (ctx, mut app) = setup();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sidebar.txt");
        std::fs::write(&path, b"sidebar bytes").unwrap();
        let first = app.active;
        seed(&mut app, first, "keep owner");
        match modal {
            0 => editor(&mut app, first).error = Some("test modal".into()),
            1 => editor(&mut app, first).pending = Some(Pending::CloseTab),
            _ => app.folder_dialog_pending = true,
        }
        app.deferred_project_path = Some(path.clone());
        for _ in 0..2 {
            render_frame(&ctx, &mut app, Vec::new());
            assert_eq!(app.tabs.len(), 1);
            assert_eq!(app.active, first);
            assert_eq!(app.deferred_project_path.as_ref(), Some(&path));
            assert_eq!(bytes(&mut app, first), b"keep owner");
        }
        editor(&mut app, first).error = None;
        editor(&mut app, first).pending = None;
        app.folder_dialog_pending = false;
        render_frame(&ctx, &mut app, Vec::new());
        await_loads(&mut app, &ctx);
        assert_eq!(app.tabs.len(), 2);
        assert!(app.deferred_project_path.is_none());
        let opened = app.active;
        assert_ne!(opened, first);
        assert_eq!(bytes(&mut app, opened), b"sidebar bytes");
        assert_eq!(bytes(&mut app, first), b"keep owner");
        render_frame(&ctx, &mut app, Vec::new());
        assert_eq!(app.tabs.len(), 2);
    }
}

#[test]
fn queued_sidebar_selection_waits_during_quit_even_without_a_modal() {
    let (ctx, mut app) = setup();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sidebar.txt");
    std::fs::write(&path, b"sidebar bytes").unwrap();
    let first = app.active;
    seed(&mut app, first, "keep owner");
    editor(&mut app, first).insert("dirty ");
    editor(&mut app, first).work = Some(work("Saving"));
    app.deferred_project_path = Some(path.clone());
    app.request_quit();
    assert!(!app.modal_open());
    render_frame(&ctx, &mut app, Vec::new());
    assert_eq!(app.tabs.len(), 1);
    assert!(app.quitting.is_some());
    assert!(app.quitting.as_ref().unwrap().prompted.is_none());
    assert_eq!(app.deferred_project_path.as_ref(), Some(&path));
    assert_eq!(bytes(&mut app, first), b"dirty keep owner");
    assert!(!app.allow_close);
    editor(&mut app, first).work = None;
    cancel_pending(&mut app, first, &ctx);
    render_frame(&ctx, &mut app, Vec::new());
    await_loads(&mut app, &ctx);
    assert_eq!(app.tabs.len(), 2);
    assert!(app.deferred_project_path.is_none());
    assert_eq!(bytes(&mut app, first), b"dirty keep owner");
    assert!(editor(&mut app, first).doc.is_modified());
}

#[test]
fn folder_change_discards_queued_old_root_sidebar_selection() {
    let (ctx, mut app) = setup();
    let old_root = tempfile::tempdir().unwrap();
    let new_root = tempfile::tempdir().unwrap();
    let old_path = old_root.path().join("stale.txt");
    std::fs::write(&old_path, b"must not open").unwrap();
    let first = app.active;
    seed(&mut app, first, "keep owner");
    app.project.set_root(old_root.path().to_path_buf());
    app.deferred_project_path = Some(old_path);
    app.folder_dialog_pending = true;
    app.folder_dialog_request = Some(app.folder_dialog_generation);
    app.tx
        .send(WorkspaceMessage::Folder {
            generation: app.folder_dialog_generation,
            path: Some(new_root.path().to_path_buf()),
        })
        .unwrap();
    render_frame(&ctx, &mut app, Vec::new());
    assert_eq!(app.project.root(), Some(new_root.path()));
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.active, first);
    assert!(app.deferred_project_path.is_none());
    assert_eq!(bytes(&mut app, first), b"keep owner");
    assert!(editor(&mut app, first).path.is_none());
}

#[test]
fn visible_close_folder_button_preserves_all_open_tabs_edits_paths_cursors_and_undo() {
    let (ctx, mut app) = setup();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("keep-open.txt");
    std::fs::write(&path, b"original file\nsecond line").unwrap();
    app.open_path(path.clone(), &ctx);
    await_loads(&mut app, &ctx);
    let first = app.active;
    editor(&mut app, first).insert("first edits ");
    editor(&mut app, first).cursor = 4;
    editor(&mut app, first).anchor = Some(1);
    let second = app.new_tab(&ctx);
    editor(&mut app, second).insert("second edits");
    editor(&mut app, second).cursor = 3;
    editor(&mut app, second).anchor = Some(1);
    app.project.set_root(directory.path().to_path_buf());
    render_frame(&ctx, &mut app, Vec::new());
    let output = render_frame(&ctx, &mut app, Vec::new());
    let text = painted_text(&output);
    assert!(text.iter().any(|(text, _)| text == "Project"));
    let close_button = text
        .iter()
        .find(|(text, _)| text == "Close Folder")
        .expect("Close Folder is visibly discoverable")
        .1
        .center();
    let before: Vec<_> = app
        .tabs
        .iter()
        .map(|tab| {
            let editor = &tab.editor;
            (
                tab.id,
                editor.doc.read(0..editor.doc.len()),
                editor.doc.revision(),
                editor.cursor,
                editor.anchor,
                editor.path.clone(),
                editor.stamp.clone(),
                editor.doc.is_modified(),
            )
        })
        .collect();
    for pressed in [true, false] {
        render_frame(
            &ctx,
            &mut app,
            vec![
                egui::Event::PointerMoved(close_button),
                egui::Event::PointerButton {
                    pos: close_button,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: Default::default(),
                },
            ],
        );
    }
    assert!(app.project.root().is_none());
    assert_eq!(app.active, second);
    assert_eq!(app.tabs.len(), 2);
    assert!(!app.modal_open());
    for (id, original, revision, cursor, anchor, path, stamp, modified) in before {
        assert_eq!(bytes(&mut app, id), original);
        let editor = editor(&mut app, id);
        assert_eq!(editor.doc.revision(), revision);
        assert_eq!(editor.cursor, cursor);
        assert_eq!(editor.anchor, anchor);
        assert_eq!(editor.path, path);
        assert_eq!(editor.stamp, stamp);
        assert_eq!(editor.doc.is_modified(), modified);
        assert!(editor.pending.is_none());
        assert!(editor.error.is_none());
    }
    let output = render_frame(&ctx, &mut app, Vec::new());
    assert!(
        !painted_text(&output)
            .iter()
            .any(|(text, _)| text == "Project")
    );
    editor(&mut app, first).undo(false);
    assert_eq!(bytes(&mut app, first), b"original file\nsecond line");
    assert!(!editor(&mut app, first).doc.is_modified());
    editor(&mut app, second).undo(false);
    assert!(editor(&mut app, second).doc.is_empty());
    assert!(!editor(&mut app, second).doc.is_modified());
    assert!(directory.path().is_dir());
    assert_eq!(std::fs::read(&path).unwrap(), b"original file\nsecond line");
}

#[test]
fn close_folder_discards_queued_selection_and_supports_reopening_the_same_root() {
    let (ctx, mut app) = setup();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("must-not-open.txt");
    std::fs::write(&path, b"untouched disk bytes").unwrap();
    let first = app.active;
    editor(&mut app, first).insert("keep dirty editor");
    let revision = editor(&mut app, first).doc.revision();
    app.project.set_root(directory.path().to_path_buf());
    app.deferred_project_path = Some(path.clone());
    app.close_folder();
    assert!(app.project.root().is_none());
    assert!(app.deferred_project_path.is_none());
    for _ in 0..3 {
        render_frame(&ctx, &mut app, Vec::new());
        assert!(app.project.root().is_none());
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active, first);
        assert_eq!(bytes(&mut app, first), b"keep dirty editor");
        assert_eq!(editor(&mut app, first).doc.revision(), revision);
    }
    app.folder_dialog_generation += 1;
    app.folder_dialog_pending = true;
    app.folder_dialog_request = Some(app.folder_dialog_generation);
    app.tx
        .send(WorkspaceMessage::Folder {
            generation: app.folder_dialog_generation,
            path: Some(directory.path().to_path_buf()),
        })
        .unwrap();
    let output = render_frame(&ctx, &mut app, Vec::new());
    assert_eq!(app.project.root(), Some(directory.path()));
    assert!(!app.folder_dialog_pending);
    assert!(
        painted_text(&output)
            .iter()
            .any(|(text, _)| text == "Project")
    );
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(bytes(&mut app, first), b"keep dirty editor");
    assert_eq!(editor(&mut app, first).doc.revision(), revision);
    assert!(editor(&mut app, first).path.is_none());
    assert_eq!(std::fs::read(path).unwrap(), b"untouched disk bytes");
}

#[test]
fn closing_folder_ignores_late_picker_results_without_canceling_a_newer_picker() {
    let (ctx, mut app) = setup();
    let old_root = tempfile::tempdir().unwrap();
    let new_root = tempfile::tempdir().unwrap();
    let first = app.active;
    editor(&mut app, first).insert("keep editor");
    app.project.set_root(old_root.path().to_path_buf());
    app.folder_dialog_generation = 5;
    app.folder_dialog_pending = true;
    app.folder_dialog_request = Some(5);
    render_frame(&ctx, &mut app, Vec::new());
    let output = render_frame(&ctx, &mut app, Vec::new());
    let close_button = painted_text(&output)
        .into_iter()
        .find(|(text, _)| text == "Close Folder")
        .expect("Close Folder stays visible while the folder picker is pending")
        .1
        .center();
    for pressed in [true, false] {
        render_frame(
            &ctx,
            &mut app,
            vec![
                egui::Event::PointerMoved(close_button),
                egui::Event::PointerButton {
                    pos: close_button,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: Default::default(),
                },
            ],
        );
    }
    assert!(app.folder_dialog_pending);
    assert_eq!(app.folder_dialog_request, Some(5));
    assert!(app.project.root().is_none());
    let closed_generation = app.folder_dialog_generation;
    for _ in 0..3 {
        app.begin_folder_dialog(&ctx);
        assert!(app.folder_dialog_pending);
        assert_eq!(app.folder_dialog_request, Some(5));
        assert_eq!(app.folder_dialog_generation, closed_generation);
    }
    app.tx
        .send(WorkspaceMessage::Folder {
            generation: 5,
            path: Some(old_root.path().to_path_buf()),
        })
        .unwrap();
    app.poll(&ctx);
    assert!(app.project.root().is_none());
    assert!(!app.folder_dialog_pending);
    assert!(app.folder_dialog_request.is_none());

    app.folder_dialog_generation += 1;
    let current_generation = app.folder_dialog_generation;
    app.folder_dialog_pending = true;
    app.folder_dialog_request = Some(current_generation);
    for path in [Some(old_root.path().to_path_buf()), None] {
        app.tx
            .send(WorkspaceMessage::Folder {
                generation: 5,
                path,
            })
            .unwrap();
        app.poll(&ctx);
        assert!(app.project.root().is_none());
        assert!(app.folder_dialog_pending);
        assert_eq!(app.folder_dialog_request, Some(current_generation));
        assert_eq!(app.folder_dialog_generation, current_generation);
    }
    app.tx
        .send(WorkspaceMessage::Folder {
            generation: current_generation,
            path: Some(new_root.path().to_path_buf()),
        })
        .unwrap();
    app.poll(&ctx);
    assert_eq!(app.project.root(), Some(new_root.path()));
    assert!(!app.folder_dialog_pending);
    assert!(app.folder_dialog_request.is_none());
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.active, first);
    assert_eq!(bytes(&mut app, first), b"keep editor");
    assert!(editor(&mut app, first).doc.is_modified());
    assert!(old_root.path().is_dir());
    assert!(new_root.path().is_dir());
}

#[test]
fn missing_initial_cli_path_renders_clean_editable_untitled_without_warning_or_dialog() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing-startup.txt");
    let ctx = egui::Context::default();
    let mut app = WorkspaceApp::from_startup_path(&ctx, Some(missing.clone()), false);
    let first = app.active;
    assert_eq!(app.tabs.len(), 1);
    assert!(editor(&mut app, first).ready);
    assert!(editor(&mut app, first).doc.is_empty());
    assert!(!editor(&mut app, first).doc.is_modified());
    assert!(editor(&mut app, first).path.is_none());
    assert!(editor(&mut app, first).stamp.is_none());
    assert!(editor(&mut app, first).work.is_none());
    assert!(editor(&mut app, first).error.is_none());
    assert!(editor(&mut app, first).pending.is_none());
    assert_eq!(editor(&mut app, first).cursor, 0);
    assert!(!app.modal_open());
    assert!(app.startup_warning.is_none());
    assert!(app.startup_load.is_none());
    assert!(editor(&mut app, first).save_destination.is_none());

    // Use the actual workspace render, rather than only inspecting model state.
    // Untitled tabs/toolbar and the editor gutter must be painted, with no
    // error banner or dialog text.
    let output = render_frame(&ctx, &mut app, Vec::new());
    let text = painted_text(&output);
    for expected in ["Untitled", "New", "Open…", "1", "Ln 1 · byte 1"] {
        assert!(
            text.iter().any(|(text, _)| text == expected),
            "missing editor UI text {expected:?}"
        );
    }
    for forbidden in [
        "Could not open",
        "Cannot open",
        "Cannot inspect",
        "missing-startup.txt",
        "Dismiss",
        "OK",
        "Read-only preview",
    ] {
        assert!(
            !text.iter().any(|(text, _)| text.contains(forbidden)),
            "unexpected startup error UI text {forbidden:?}"
        );
    }
    assert!(editor(&mut app, first).rendered_rows > 0);
    assert!(output.viewport_output.values().any(|viewport| {
        viewport.commands.iter().any(|command| {
            matches!(command, egui::ViewportCommand::Title(title) if title == "Untitled — SimpTxt")
        })
    }));

    let output = render_frame(
        &ctx,
        &mut app,
        vec![egui::Event::Text("usable editor".into())],
    );
    assert_eq!(bytes(&mut app, first), b"usable editor");
    assert!(editor(&mut app, first).doc.is_modified());
    assert!(
        painted_text(&output)
            .iter()
            .any(|(text, _)| text == "usable editor")
    );
    assert!(!app.modal_open());
    assert!(editor(&mut app, first).path.is_none());
    assert!(
        !missing.exists(),
        "startup recovery must not create the file"
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn missing_initial_path_keeps_new_and_valid_open_usable_without_any_error_to_dismiss() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("never-created.txt");
    let valid = directory.path().join("valid.txt");
    std::fs::write(&valid, "valid α\r\n").unwrap();
    let ctx = egui::Context::default();
    let mut app = WorkspaceApp::from_startup_path(&ctx, Some(missing.clone()), false);
    let recovered = app.active;
    render_frame(
        &ctx,
        &mut app,
        vec![egui::Event::Text("keep these new edits".into())],
    );
    let output = render_frame(&ctx, &mut app, Vec::new());
    let new_button = painted_text(&output)
        .into_iter()
        .find(|(text, _)| text == "New")
        .expect("New button is visibly rendered")
        .1
        .center();
    for pressed in [true, false] {
        render_frame(
            &ctx,
            &mut app,
            vec![
                egui::Event::PointerMoved(new_button),
                egui::Event::PointerButton {
                    pos: new_button,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: Default::default(),
                },
            ],
        );
    }
    let new = app.active;
    assert_ne!(
        new, recovered,
        "the actual New toolbar button remains usable"
    );
    assert_eq!(app.tabs.len(), 2);
    assert!(editor(&mut app, new).ready);
    assert!(editor(&mut app, new).doc.is_empty());
    assert!(editor(&mut app, new).path.is_none());
    assert_eq!(bytes(&mut app, recovered), b"keep these new edits");
    assert!(!app.modal_open());
    assert!(app.startup_warning.is_none());
    render_frame(
        &ctx,
        &mut app,
        vec![egui::Event::Text("new tab also editable".into())],
    );
    assert_eq!(bytes(&mut app, new), b"new tab also editable");

    // Complete the same per-tab channel used by the native Open dialog without
    // spawning platform UI in the test process.
    editor(&mut app, new).dialog_pending = true;
    editor(&mut app, new)
        .tx
        .send(Message::OpenDialog(Some(valid.clone())))
        .unwrap();
    await_loads(&mut app, &ctx);
    let opened = app.active;
    assert_eq!(app.tabs.len(), 3);
    assert_eq!(bytes(&mut app, opened), "valid α\r\n".as_bytes());
    assert_eq!(editor(&mut app, opened).path.as_ref(), Some(&valid));
    assert!(editor(&mut app, opened).ready);
    assert!(!editor(&mut app, opened).doc.is_modified());
    assert_eq!(bytes(&mut app, recovered), b"keep these new edits");
    assert!(editor(&mut app, recovered).doc.is_modified());
    assert_eq!(bytes(&mut app, new), b"new tab also editable");
    assert!(editor(&mut app, new).doc.is_modified());
    assert!(!app.modal_open());
    assert!(app.startup_warning.is_none());
    assert!(!missing.exists());
    assert_eq!(std::fs::read(&valid).unwrap(), "valid α\r\n".as_bytes());
}

#[test]
fn startup_directory_rejection_keeps_its_warning_visible() {
    let directory = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let mut app =
        WorkspaceApp::from_startup_path(&ctx, Some(directory.path().to_path_buf()), false);
    let first = app.active;
    let warning = app.startup_warning.clone().expect("real startup error");
    assert!(warning.contains("not a regular file"));
    assert!(editor(&mut app, first).ready);
    assert!(editor(&mut app, first).doc.is_empty());
    assert!(editor(&mut app, first).path.is_none());
    assert!(!app.modal_open());
    let output = render_frame(&ctx, &mut app, Vec::new());
    let text = painted_text(&output);
    assert!(text.iter().any(|(text, _)| text == &warning));
    assert!(
        text.iter()
            .any(|(text, _)| text == "Could not open the startup file")
    );
}

#[test]
fn missing_startup_recovery_does_not_hide_a_real_save_failure_or_lose_edits() {
    let directory = tempfile::tempdir().unwrap();
    let missing_startup = directory.path().join("missing-startup.txt");
    let missing_parent = directory.path().join("missing-parent");
    let save_destination = missing_parent.join("new-document.txt");
    let ctx = egui::Context::default();
    let mut app = WorkspaceApp::from_startup_path(&ctx, Some(missing_startup.clone()), false);
    let first = app.active;
    editor(&mut app, first).insert("keep my edits");
    let revision = editor(&mut app, first).doc.revision();
    app.save_path(first, save_destination.clone(), &ctx);
    await_loads(&mut app, &ctx);
    let error = editor(&mut app, first)
        .error
        .clone()
        .expect("a real save failure must remain visible");
    assert!(app.modal_open());
    assert!(app.startup_warning.is_none());
    assert!(editor(&mut app, first).ready);
    assert!(editor(&mut app, first).path.is_none());
    assert!(editor(&mut app, first).save_destination.is_none());
    assert!(editor(&mut app, first).doc.is_modified());
    assert_eq!(editor(&mut app, first).doc.revision(), revision);
    assert_eq!(bytes(&mut app, first), b"keep my edits");
    render_frame(&ctx, &mut app, Vec::new());
    let output = render_frame(&ctx, &mut app, Vec::new());
    assert!(painted_text(&output).iter().any(|(text, _)| text == &error));
    assert!(!missing_startup.exists());
    assert!(!missing_parent.exists());
    assert!(!save_destination.exists());
    editor(&mut app, first).undo(false);
    assert!(editor(&mut app, first).doc.is_empty());
}

#[test]
fn valid_initial_cli_path_loads_without_blank_recovery_or_warning() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("valid-startup.txt");
    let original = "\u{feff}valid startup α\r\n";
    std::fs::write(&path, original).unwrap();
    let ctx = egui::Context::default();
    let mut app = WorkspaceApp::from_startup_path(&ctx, Some(path.clone()), false);
    let first = app.active;
    assert_eq!(app.startup_load, Some((first, 1)));
    await_loads(&mut app, &ctx);
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.active, first);
    assert!(app.startup_load.is_none());
    assert!(app.startup_warning.is_none());
    assert!(editor(&mut app, first).ready);
    assert!(editor(&mut app, first).error.is_none());
    assert_eq!(editor(&mut app, first).path.as_ref(), Some(&path));
    assert!(editor(&mut app, first).stamp.is_some());
    assert_eq!(bytes(&mut app, first), original.as_bytes());
    assert!(!editor(&mut app, first).doc.is_modified());
    assert_eq!(std::fs::read(path).unwrap(), original.as_bytes());
}

#[test]
fn missing_normal_open_preserves_dirty_document_revision_undo_path_cursor_and_tabs() {
    let (ctx, mut app) = setup();
    let directory = tempfile::tempdir().unwrap();
    let original_path = directory.path().join("original.txt");
    let missing = directory.path().join("missing-normal-open.txt");
    std::fs::write(&original_path, "original α\nsecond line").unwrap();
    app.open_path(original_path.clone(), &ctx);
    await_loads(&mut app, &ctx);
    let first = app.active;
    editor(&mut app, first).cursor = 8;
    editor(&mut app, first).insert("dirty ");
    editor(&mut app, first).anchor = Some(2);
    editor(&mut app, first).top_line = 1;
    editor(&mut app, first).horizontal = 3;
    let second = app.new_tab(&ctx);
    editor(&mut app, second).insert("independent dirty tab");
    app.switch_to(first);
    let original = bytes(&mut app, first);
    let revision = editor(&mut app, first).doc.revision();
    let cursor = editor(&mut app, first).cursor;
    let anchor = editor(&mut app, first).anchor;
    let generation = editor(&mut app, first).generation;
    let tab_ids: Vec<_> = app.tabs.iter().map(|tab| tab.id).collect();
    app.open_path(missing.clone(), &ctx);
    assert_eq!(app.active, first);
    assert_eq!(
        app.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
        tab_ids
    );
    assert_eq!(bytes(&mut app, first), original);
    assert_eq!(editor(&mut app, first).doc.revision(), revision);
    assert_eq!(editor(&mut app, first).generation, generation);
    assert_eq!(editor(&mut app, first).cursor, cursor);
    assert_eq!(editor(&mut app, first).anchor, anchor);
    assert_eq!(editor(&mut app, first).top_line, 1);
    assert_eq!(editor(&mut app, first).horizontal, 3);
    assert_eq!(editor(&mut app, first).path.as_ref(), Some(&original_path));
    assert!(editor(&mut app, first).stamp.is_some());
    assert!(editor(&mut app, first).ready);
    assert!(editor(&mut app, first).work.is_none());
    assert!(editor(&mut app, first).doc.is_modified());
    assert!(editor(&mut app, first).error.is_some());
    assert!(app.startup_warning.is_none());
    let error = editor(&mut app, first).error.clone().unwrap();
    render_frame(&ctx, &mut app, Vec::new());
    let output = render_frame(&ctx, &mut app, Vec::new());
    assert!(painted_text(&output).iter().any(|(text, _)| text == &error));
    assert!(app.modal_open());
    assert_eq!(bytes(&mut app, second), b"independent dirty tab");
    assert!(editor(&mut app, second).doc.is_modified());
    editor(&mut app, first).undo(false);
    assert_eq!(bytes(&mut app, first), "original α\nsecond line".as_bytes());
    assert!(!editor(&mut app, first).doc.is_modified());
    editor(&mut app, first).undo(true);
    assert_eq!(bytes(&mut app, first), original);
    assert_eq!(editor(&mut app, first).doc.revision(), revision);
    assert!(editor(&mut app, first).doc.is_modified());
    assert!(!missing.exists());
    assert_eq!(
        std::fs::read(original_path).unwrap(),
        "original α\nsecond line".as_bytes()
    );
}

#[test]
fn unclassified_initial_open_failure_stays_visible_even_if_startup_path_is_missing() {
    let (ctx, mut app) = setup();
    let directory = tempfile::tempdir().unwrap();
    let failed_path = directory.path().join("disappeared-after-startup.txt");
    let first = app.active;
    seed(&mut app, first, "incomplete preview");
    let startup = editor(&mut app, first);
    startup.generation = 7;
    startup.ready = false;
    startup.path = Some(failed_path.clone());
    startup.work = Some(work("Opening"));
    let late_sender = startup.tx.clone();
    app.startup_load = Some((first, 7));
    let second = app.new_tab(&ctx);
    editor(&mut app, second).insert("other tab edits");
    editor(&mut app, second).cursor = 4;
    let revision = editor(&mut app, second).doc.revision();
    late_sender
        .send(Message::Loaded(
            7,
            Err("file disappeared while opening".into()),
        ))
        .unwrap();
    app.poll(&ctx);
    assert_eq!(app.active, second);
    assert_eq!(app.tabs.len(), 2);
    assert!(app.startup_load.is_none());
    let warning = app.startup_warning.as_ref().expect("startup error warning");
    assert!(warning.contains("disappeared-after-startup.txt"));
    assert!(warning.contains("file disappeared while opening"));
    assert!(editor(&mut app, first).ready);
    assert!(editor(&mut app, first).doc.is_empty());
    assert!(!editor(&mut app, first).doc.is_modified());
    assert!(editor(&mut app, first).path.is_none());
    assert!(editor(&mut app, first).stamp.is_none());
    assert!(editor(&mut app, first).work.is_none());
    assert!(editor(&mut app, first).error.is_none());
    assert_eq!(bytes(&mut app, second), b"other tab edits");
    assert_eq!(editor(&mut app, second).doc.revision(), revision);
    assert_eq!(editor(&mut app, second).cursor, 4);
    assert!(!app.modal_open());
    assert!(
        late_sender
            .send(Message::Loaded(7, Err("stale duplicate failure".into())))
            .is_err(),
        "the discarded startup preview must not retain a live result channel"
    );
    editor(&mut app, second).undo(false);
    assert!(editor(&mut app, second).doc.is_empty());
    assert!(!failed_path.exists());
}

#[test]
fn asynchronous_missing_initial_open_recovers_only_its_inactive_tab_without_warning() {
    let (ctx, mut app) = setup();
    let directory = tempfile::tempdir().unwrap();
    let failed_path = directory.path().join("disappeared-after-startup.txt");
    let first = app.active;
    seed(&mut app, first, "incomplete preview");
    let startup = editor(&mut app, first);
    startup.generation = 7;
    startup.ready = false;
    startup.path = Some(failed_path.clone());
    startup.work = Some(work("Opening"));
    let late_sender = startup.tx.clone();
    app.startup_load = Some((first, 7));
    let second = app.new_tab(&ctx);
    editor(&mut app, second).insert("other tab edits");
    editor(&mut app, second).cursor = 4;
    let revision = editor(&mut app, second).doc.revision();
    late_sender
        .send(Message::Loaded(
            7,
            Err(std::io::Error::from(std::io::ErrorKind::NotFound).into()),
        ))
        .unwrap();
    app.poll(&ctx);
    assert_eq!(app.active, second);
    assert_eq!(app.tabs.len(), 2);
    assert!(app.startup_load.is_none());
    assert!(app.startup_warning.is_none());
    assert!(editor(&mut app, first).ready);
    assert!(editor(&mut app, first).doc.is_empty());
    assert!(!editor(&mut app, first).doc.is_modified());
    assert!(editor(&mut app, first).path.is_none());
    assert!(editor(&mut app, first).stamp.is_none());
    assert!(editor(&mut app, first).save_destination.is_none());
    assert!(editor(&mut app, first).work.is_none());
    assert!(editor(&mut app, first).error.is_none());
    assert_eq!(bytes(&mut app, second), b"other tab edits");
    assert_eq!(editor(&mut app, second).doc.revision(), revision);
    assert_eq!(editor(&mut app, second).cursor, 4);
    assert!(!app.modal_open());
    let output = render_frame(&ctx, &mut app, Vec::new());
    let text = painted_text(&output);
    assert!(!text.iter().any(|(text, _)| {
        text.contains("disappeared-after-startup.txt")
            || text.contains("Could not open")
            || text == "Dismiss"
            || text == "OK"
    }));
    assert!(
        late_sender
            .send(Message::Loaded(7, Err("stale duplicate failure".into())))
            .is_err(),
        "the discarded startup preview must not retain a live result channel"
    );
    app.switch_to(first);
    render_frame(
        &ctx,
        &mut app,
        vec![egui::Event::Text("recovered tab edits".into())],
    );
    assert_eq!(bytes(&mut app, first), b"recovered tab edits");
    assert_eq!(bytes(&mut app, second), b"other tab edits");
    assert!(!failed_path.exists());
}

#[test]
fn startup_permission_denied_stays_visible_even_when_the_path_is_now_missing() {
    let (ctx, mut app) = setup();
    let directory = tempfile::tempdir().unwrap();
    let failed_path = directory.path().join("permission-denied.txt");
    let first = app.active;
    let startup = editor(&mut app, first);
    startup.ready = false;
    startup.path = Some(failed_path.clone());
    startup.work = Some(work("Opening"));
    startup
        .tx
        .send(Message::Loaded(
            startup.generation,
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "permission denied while opening",
            )
            .into()),
        ))
        .unwrap();
    app.startup_load = Some((first, editor(&mut app, first).generation));
    app.poll(&ctx);
    let warning = app
        .startup_warning
        .clone()
        .expect("permission failures must not be silently swallowed");
    assert!(warning.contains("permission denied while opening"));
    assert!(warning.contains("permission-denied.txt"));
    assert!(editor(&mut app, first).ready);
    assert!(editor(&mut app, first).doc.is_empty());
    assert!(editor(&mut app, first).path.is_none());
    let output = render_frame(&ctx, &mut app, Vec::new());
    assert!(
        painted_text(&output)
            .iter()
            .any(|(text, _)| text == &warning)
    );
    assert!(!failed_path.exists());
}

#[test]
fn stale_startup_failure_cannot_replace_a_new_document_generation() {
    let (ctx, mut app) = setup();
    let first = app.active;
    app.startup_load = Some((first, 2));
    editor(&mut app, first).generation = 3;
    editor(&mut app, first).insert("new generation edits");
    let revision = editor(&mut app, first).doc.revision();
    editor(&mut app, first)
        .tx
        .send(Message::Loaded(
            2,
            Err(
                std::io::Error::new(std::io::ErrorKind::NotFound, "outdated startup failure")
                    .into(),
            ),
        ))
        .unwrap();
    app.poll(&ctx);
    assert_eq!(app.active, first);
    assert_eq!(app.tabs.len(), 1);
    assert!(app.startup_load.is_none());
    assert!(app.startup_warning.is_none());
    assert_eq!(bytes(&mut app, first), b"new generation edits");
    assert_eq!(editor(&mut app, first).doc.revision(), revision);
    assert!(editor(&mut app, first).ready);
    assert!(editor(&mut app, first).error.is_none());
    assert!(editor(&mut app, first).open_error_kind.is_none());
    editor(&mut app, first).undo(false);
    assert!(editor(&mut app, first).doc.is_empty());
}

#[test]
fn normal_asynchronous_open_failure_does_not_apply_startup_blank_recovery() {
    let (ctx, mut app) = setup();
    let first = app.active;
    editor(&mut app, first).insert("existing dirty document");
    let revision = editor(&mut app, first).doc.revision();
    let opened = app.new_tab(&ctx);
    seed(&mut app, opened, "read-only preview");
    let path = PathBuf::from("failed-normal-open.txt");
    let loading = editor(&mut app, opened);
    loading.ready = false;
    loading.path = Some(path.clone());
    loading.work = Some(work("Opening"));
    loading
        .tx
        .send(Message::Loaded(
            loading.generation,
            Err(
                std::io::Error::new(std::io::ErrorKind::NotFound, "normal open did not complete")
                    .into(),
            ),
        ))
        .unwrap();
    app.poll(&ctx);
    assert_eq!(app.active, opened);
    assert_eq!(app.tabs.len(), 2);
    assert!(app.startup_warning.is_none());
    assert!(app.startup_load.is_none());
    assert_eq!(bytes(&mut app, first), b"existing dirty document");
    assert_eq!(editor(&mut app, first).doc.revision(), revision);
    assert!(editor(&mut app, first).doc.is_modified());
    assert_eq!(bytes(&mut app, opened), b"read-only preview");
    assert_eq!(editor(&mut app, opened).path.as_ref(), Some(&path));
    assert!(!editor(&mut app, opened).ready);
    assert!(editor(&mut app, opened).work.is_none());
    assert_eq!(
        editor(&mut app, opened).error.as_deref(),
        Some("normal open did not complete")
    );
    editor(&mut app, first).undo(false);
    assert!(editor(&mut app, first).doc.is_empty());
}
