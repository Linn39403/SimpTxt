//! Revision/generation races are injected deterministically, without worker timing.
use super::*;

fn highlighted_editor() -> (egui::Context, EditorApp) {
    let ctx = egui::Context::default();
    let mut app = EditorApp::create(&ctx, false);
    app.doc = Document::from_bytes(b"let value = 42;\n".repeat(20_000)).unwrap();
    app.language = Language::Rust;
    let index = HighlightIndex::build(&app.doc, app.language, &AtomicBool::new(false)).unwrap();
    app.highlight_base = Some(index.clone());
    app.highlight = Some(index);
    app.highlight_revision = Some(app.doc.revision());
    (ctx, app)
}

#[test]
fn edits_retain_reusable_base_but_never_render_stale_spans() {
    let (_, mut app) = highlighted_editor();
    let old_revision = app.doc.revision();
    let cancel = Arc::new(AtomicBool::new(false));
    app.highlight_cancel = Some(cancel.clone());
    app.viewport_highlight.prepare(
        &app.doc,
        app.highlight.as_ref(),
        std::slice::from_ref(&(0..15)),
    );
    assert!(!app.viewport_highlight.row(0).is_empty());
    app.doc.replace(0..3, "   ").unwrap();
    app.changed();
    assert!(cancel.load(Ordering::Relaxed));
    assert_eq!(app.highlight_revision, None);
    assert!(app.highlight_base.is_some());
    assert!(!app.highlight_base.as_ref().unwrap().is_current(&app.doc));
    assert_ne!(old_revision, app.doc.revision());
    app.viewport_highlight.prepare(
        &app.doc,
        app.highlight.as_ref(),
        std::slice::from_ref(&(0..15)),
    );
    assert!(app.viewport_highlight.row(0).is_empty());
}

#[test]
fn stale_revision_generation_and_cancelled_job_messages_are_ignored() {
    let (ctx, mut app) = highlighted_editor();
    let old_revision = app.doc.revision();
    let old_id = app.highlight_id;
    let old = app.highlight.as_ref().unwrap().clone();
    app.doc.replace(0..3, "pub").unwrap();
    app.changed();
    let current = HighlightIndex::refresh(
        &app.doc,
        app.language,
        Some(&old),
        app.doc.len(),
        &AtomicBool::new(false),
    )
    .unwrap();
    for (generation, revision, id, index) in [
        (app.generation, old_revision, app.highlight_id, old),
        (
            app.generation + 1,
            app.doc.revision(),
            app.highlight_id,
            current.clone(),
        ),
        (app.generation, app.doc.revision(), old_id, current.clone()),
    ] {
        app.tx
            .send(Message::Highlighted(
                generation,
                revision,
                id,
                true,
                Ok(index),
            ))
            .unwrap();
        app.poll(&ctx);
        assert!(!app.highlight.as_ref().unwrap().is_current(&app.doc));
    }
    app.tx
        .send(Message::Highlighted(
            app.generation,
            app.doc.revision(),
            app.highlight_id,
            true,
            Ok(current),
        ))
        .unwrap();
    app.poll(&ctx);
    assert!(app.highlight.as_ref().unwrap().is_current(&app.doc));
    assert!(app.highlight_base.as_ref().unwrap().is_current(&app.doc));
}

#[test]
fn intermediate_prefix_does_not_discard_complete_reuse_base() {
    let (ctx, mut app) = highlighted_editor();
    let old = app.highlight_base.as_ref().unwrap().clone();
    app.doc.replace(100..100, "/*").unwrap();
    app.changed();
    let partial = HighlightIndex::refresh(
        &app.doc,
        app.language,
        Some(&old),
        128 * 1024,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(partial.indexed_until() < app.doc.len());
    app.tx
        .send(Message::Highlighted(
            app.generation,
            app.doc.revision(),
            app.highlight_id,
            false,
            Ok(partial),
        ))
        .unwrap();
    app.poll(&ctx);
    assert!(app.highlight.as_ref().unwrap().is_current(&app.doc));
    assert_eq!(
        app.highlight_base.as_ref().unwrap().indexed_until(),
        old.indexed_until()
    );
    assert!(!app.highlight_base.as_ref().unwrap().is_current(&app.doc));
    app.cancel_highlight();
    assert!(app.highlight.is_none());
    assert!(app.highlight_base.is_none());
}

#[test]
fn latest_initial_progress_survives_edit_and_old_slot_is_detached() {
    let (ctx, mut app) = highlighted_editor();
    app.highlight_base = None;
    let old_revision = app.doc.revision();
    let old_id = app.highlight_id;
    let cancel = Arc::new(AtomicBool::new(false));
    app.highlight_cancel = Some(cancel.clone());
    let prefix =
        HighlightIndex::build_prefix(&app.doc, app.language, 64 * 1024, &AtomicBool::new(false))
            .unwrap();
    let progressed = HighlightIndex::refresh(
        &app.doc,
        app.language,
        Some(&prefix),
        192 * 1024,
        &AtomicBool::new(false),
    )
    .unwrap();
    app.highlight = Some(prefix.clone());
    let old_slot = Arc::new(Mutex::new(Some(HighlightProgress {
        generation: app.generation,
        revision: old_revision,
        id: old_id,
        index: prefix,
    })));
    app.highlight_progress = Some(old_slot.clone());
    // Overwrite rather than enqueue: there is exactly one pending prefix.
    *old_slot.lock().unwrap() = Some(HighlightProgress {
        generation: app.generation,
        revision: old_revision,
        id: old_id,
        index: progressed.clone(),
    });
    assert!(app.rx.try_recv().is_err());
    app.doc.replace(10..10, " ").unwrap();
    app.changed();
    assert!(cancel.load(Ordering::Relaxed));
    assert!(app.highlight_progress.is_none());
    assert_eq!(app.highlight.as_ref().unwrap().indexed_until(), 192 * 1024);
    assert!(!app.highlight.as_ref().unwrap().is_current(&app.doc));
    let current = HighlightIndex::refresh(
        &app.doc,
        app.language,
        app.highlight.as_ref(),
        app.doc.len(),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(current.scanned_bytes() < app.doc.len() - 128 * 1024);
    app.highlight = Some(current.clone());
    app.highlight_base = Some(current);
    let new_slot = Arc::new(Mutex::new(None));
    app.highlight_progress = Some(new_slot.clone());
    app.highlight_cancel = Some(Arc::new(AtomicBool::new(false)));
    app.highlight_revision = Some(app.doc.revision());
    // A cancelled worker racing after the pause can write only its old slot.
    *old_slot.lock().unwrap() = Some(HighlightProgress {
        generation: app.generation,
        revision: old_revision,
        id: old_id,
        index: progressed.clone(),
    });
    app.tx
        .send(Message::Highlighted(
            app.generation,
            old_revision,
            old_id,
            true,
            Ok(progressed),
        ))
        .unwrap();
    app.poll(&ctx);
    assert!(app.highlight.as_ref().unwrap().is_current(&app.doc));
    assert!(app.highlight_base.as_ref().unwrap().is_current(&app.doc));
    assert!(new_slot.lock().unwrap().is_none());
    assert!(app.highlight_cancel.is_some());
}

#[test]
fn edit_beyond_initial_prefix_does_not_shrink_progress_at_top_viewport() {
    let (_, mut app) = highlighted_editor();
    let partial =
        HighlightIndex::build_prefix(&app.doc, app.language, 192 * 1024, &AtomicBool::new(false))
            .unwrap();
    app.highlight_base = None;
    app.highlight = Some(partial.clone());
    app.doc.replace(250 * 1024..250 * 1024, " ").unwrap();
    app.changed();
    let through = initial_highlight_through(app.doc.len(), 1500, partial.indexed_until(), false);
    assert_eq!(through, partial.indexed_until());
    let refreshed = HighlightIndex::refresh(
        &app.doc,
        app.language,
        Some(&partial),
        through,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(refreshed.indexed_until() >= partial.indexed_until());
    assert_eq!(refreshed.scanned_bytes(), 0);
    assert_eq!(
        initial_highlight_through(1_000_000_000, 900_000_000, 0, false),
        HIGHLIGHT_PROGRESS_BYTES
    );
}
