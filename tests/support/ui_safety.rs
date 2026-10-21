use super::*;

fn setup() -> (egui::Context, App) {
    let ctx = egui::Context::default();
    let app = App::isolated(&ctx);
    (ctx, app)
}

#[test]
fn dirty_close_retains_document_until_decision() {
    let (_ctx, mut app) = setup();
    app.tabs[0].doc.set_text("keep me".into());
    app.close_tab(0);
    assert!(matches!(app.pending, Some(Pending::Tab(0))));
    assert_eq!(app.tabs[0].doc.text(), "keep me");
    app.pending = None; // Same state transition as the Cancel button.
    assert_eq!(app.tabs.len(), 1);
    assert!(app.tabs[0].doc.is_dirty());
}

#[test]
fn exit_prompts_each_dirty_tab_in_turn() {
    let (ctx, mut app) = setup();
    app.tabs[0].doc.set_text("one".into());
    app.new_tab();
    app.tabs[1].doc.set_text("two".into());
    app.exit(&ctx);
    assert!(!app.allow_close);
    assert_eq!(app.active, 0);
    assert!(matches!(app.pending, Some(Pending::Exit)));
    app.resolved(Pending::Exit, &ctx); // Don't save first tab.
    assert!(!app.allow_close);
    assert_eq!(app.tabs[app.active].doc.text(), "two");
    assert!(matches!(app.pending, Some(Pending::Exit)));
    app.resolved(Pending::Exit, &ctx);
    assert!(app.allow_close);
}

#[test]
fn failed_save_does_not_resolve_close() {
    let (_ctx, mut app) = setup();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("note.txt");
    std::fs::write(&path, "before").unwrap();
    app.open_path(&path);
    app.tabs[0].doc.set_text("local".into());
    app.close_tab(0);
    std::fs::write(&path, "external").unwrap();
    assert!(!app.save(false));
    assert!(app.pending.is_some());
    assert!(app.error.is_some());
    assert_eq!(app.tabs[0].doc.text(), "local");
    assert_eq!(std::fs::read_to_string(path).unwrap(), "external");
}

#[test]
fn invalid_open_keeps_current_document() {
    let (_ctx, mut app) = setup();
    app.tabs[0].doc.set_text("keep me".into());
    let dir = tempfile::tempdir().unwrap();
    app.open_path(&dir.path().join("missing.txt"));
    assert!(app.error.is_some());
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.tabs[0].doc.text(), "keep me");
}

#[test]
fn unicode_find_selection_uses_character_positions() {
    let (_ctx, mut app) = setup();
    app.tabs[0].doc.set_text("é 🦀 中文".into());
    app.query = "中文".into();
    app.find(false);
    assert_eq!(app.reveal, Some((4, 6)));
    assert_eq!(app.tabs[0].cursor, "é 🦀 中文".len());
}

#[test]
fn isolated_settings_do_not_persist_or_change_document() {
    let (ctx, mut app) = setup();
    assert!(!app.persist_settings);
    app.tabs[0].doc.set_text("unsaved note".into());
    app.show_settings();
    app.dark = true;
    app.font_size = 20.;
    app.wrap = false;
    for _ in 0..3 {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200., 800.),
                )),
                ..Default::default()
            },
            |ui| app.render(ui),
        );
        output.textures_delta.clear();
    }
    assert!(app.settings);
    assert_eq!(app.tabs[0].doc.text(), "unsaved note");
    assert!(app.tabs[0].doc.is_dirty());
    assert!(app.error.is_none());
}
