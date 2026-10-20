use crate::{preferences::EditorPreferences, view};

#[path = "app_input.rs"]
mod app_input;
use app_input::{CursorHistory, TypingGroup};

#[path = "workspace.rs"]
mod workspace;
use workspace::WorkspaceAction;
pub use workspace::WorkspaceApp;

#[path = "app_vim.rs"]
mod app_vim;
use eframe::egui::{self, Color32, FontId, Key, Pos2, Rect, Sense, Vec2};
use simptxt::{
    document::Document,
    file_io::{self, FileStamp, LoadedFile, OpenError},
    search,
    syntax::{HighlightIndex, HighlightSpan, Language, TokenKind, ViewportCache},
};
use std::{
    ops::Range,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Eq)]
enum ExternalState {
    Current,
    Changed(FileStamp),
    Missing,
    Unavailable(String),
}

enum Message {
    OpenDialog(Option<PathBuf>),
    SaveDialog(Option<PathBuf>),
    Loaded(u64, Result<LoadedFile, OpenError>),
    Saved(u64, u64, PathBuf, Result<FileStamp, String>),
    Found(u64, u64, u64, Result<Option<Range<usize>>, String>),
    Replaced(u64, u64, u64, Result<(Document, usize), String>),
    Highlighted(u64, u64, u64, bool, Result<HighlightIndex, String>),
}
const HIGHLIGHT_PROGRESS_BYTES: usize = 8 * 1024 * 1024;

fn initial_highlight_through(
    document_len: usize,
    visible_end: usize,
    previous_until: usize,
    has_complete_base: bool,
) -> usize {
    let desired = visible_end.max(128 * 1024).min(document_len);
    if has_complete_base {
        return desired;
    }
    desired
        .max(previous_until.min(document_len))
        .min(previous_until.saturating_add(HIGHLIGHT_PROGRESS_BYTES))
}

struct HighlightProgress {
    generation: u64,
    revision: u64,
    id: u64,
    index: HighlightIndex,
}

#[derive(Clone)]
enum Pending {
    New,
    Open,
    Reload,
    Exit,
    CloseTab,
    QuitWorkspace,
}
struct Work {
    label: &'static str,
    cancel: Arc<AtomicBool>,
    progress: Arc<AtomicU64>,
    total: usize,
}
struct SearchWork {
    id: u64,
    cancel: Arc<AtomicBool>,
    progress: Arc<AtomicUsize>,
}

pub struct EditorApp {
    preferences: EditorPreferences,
    vim_enabled: bool,
    vim: simptxt::vim::VimState,
    vim_register: simptxt::vim::Register,
    bracket_cache: simptxt::brackets::BracketCache,
    workspace_managed: bool,
    workspace_actions: Vec<WorkspaceAction>,
    save_destination: Option<PathBuf>,
    doc: Document,
    path: Option<PathBuf>,
    stamp: Option<FileStamp>,
    cursor: usize,
    anchor: Option<usize>,
    preferred_column: Option<usize>,
    cursor_history: CursorHistory,
    typing_group: Option<TypingGroup>,
    top_line: usize,
    horizontal: usize,
    ready: bool,
    reloading: bool,
    generation: u64,
    next_search_id: u64,
    work: Option<Work>,
    search_work: Option<SearchWork>,
    tx: Sender<Message>,
    rx: Receiver<Message>,
    language: Language,
    highlight: Option<HighlightIndex>,
    highlight_base: Option<HighlightIndex>,
    viewport_highlight: ViewportCache,
    highlight_cancel: Option<Arc<AtomicBool>>,
    highlight_progress: Option<Arc<Mutex<Option<HighlightProgress>>>>,
    highlight_revision: Option<u64>,
    highlight_id: u64,
    last_edit: Instant,
    status: String,
    error: Option<String>,
    open_error_kind: Option<std::io::ErrorKind>,
    external_observed: Option<ExternalState>,
    external_notice: Option<String>,
    pending: Option<Pending>,
    continue_after_save: bool,
    overwrite: Option<PathBuf>,
    conflict: Option<PathBuf>,
    dialog_pending: bool,
    find_open: bool,
    find: String,
    replacement: String,
    find_focus: bool,
    goto_open: bool,
    goto: String,
    editor_focus: bool,
    composition: String,
    scroll_y: f32,
    scroll_x: f32,
    reveal_cursor: bool,
    allow_close: bool,
    visible_rows: usize,
    visible_bytes: usize,
    smoke: bool,
    smoke_frames: u8,
    rendered_rows: usize,
}

impl EditorApp {
    fn create(ctx: &egui::Context, smoke: bool) -> Self {
        EditorPreferences::default().apply_theme(ctx);
        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            preferences: EditorPreferences::default(),
            vim_enabled: false,
            vim: simptxt::vim::VimState::default(),
            vim_register: simptxt::vim::Register::default(),
            bracket_cache: simptxt::brackets::BracketCache::default(),
            workspace_managed: false,
            workspace_actions: Vec::new(),
            save_destination: None,
            doc: Document::new(),
            path: None,
            stamp: None,
            cursor: 0,
            anchor: None,
            preferred_column: None,
            cursor_history: CursorHistory::default(),
            typing_group: None,
            top_line: 0,
            horizontal: 0,
            ready: true,
            reloading: false,
            generation: 0,
            next_search_id: 0,
            work: None,
            search_work: None,
            tx,
            rx,
            language: Language::Plain,
            highlight: None,
            highlight_base: None,
            viewport_highlight: ViewportCache::default(),
            highlight_cancel: None,
            highlight_progress: None,
            highlight_revision: None,
            highlight_id: 0,
            last_edit: Instant::now(),
            status: "Ready".into(),
            error: None,
            open_error_kind: None,
            external_observed: None,
            external_notice: None,
            pending: None,
            continue_after_save: false,
            overwrite: None,
            conflict: None,
            dialog_pending: false,
            find_open: false,
            find: String::new(),
            replacement: String::new(),
            find_focus: false,
            goto_open: false,
            goto: String::new(),
            editor_focus: true,
            composition: String::new(),
            scroll_y: 0.0,
            scroll_x: 0.0,
            reveal_cursor: true,
            allow_close: false,
            visible_rows: 20,
            visible_bytes: 120,
            smoke,
            smoke_frames: 0,
            rendered_rows: 0,
        };
        if smoke {
            app.doc =
                Document::from_bytes(b"fn main() {\n    // SimpTxt startup smoke\n}\n".to_vec())
                    .expect("smoke fixture");
            app.language = Language::Rust;
        }
        app
    }

    fn selection(&self) -> Range<usize> {
        if self.vim_enabled && self.vim.mode() == simptxt::vim::Mode::Visual {
            return simptxt::vim::visual_selection_range(
                &self.doc,
                self.anchor.unwrap_or(self.cursor),
                self.cursor,
            );
        }
        match self.anchor {
            Some(a) => a.min(self.cursor)..a.max(self.cursor),
            None => self.cursor..self.cursor,
        }
    }
    fn reset_view(&mut self) {
        self.preferred_column = None;
        self.cursor_history = CursorHistory::default();
        self.break_typing_group();
        self.cursor = view::content_start(&self.doc);
        self.anchor = None;
        self.top_line = 0;
        self.horizontal = 0;
        self.editor_focus = true;
    }
    fn cancel_highlight(&mut self) {
        self.pause_highlight();
        self.highlight = None;
        self.highlight_base = None;
        self.viewport_highlight.clear();
    }
    fn pause_highlight(&mut self) {
        if let Some(cancel) = self.highlight_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        // A per-job latest-value slot retains at most one progress index. Drain
        // its last completed prefix for reuse even if an edit happened before
        // the next UI poll. Old workers can only write to their detached slot.
        if let Some(slot) = self.highlight_progress.take()
            && let Some(progress) = slot.lock().unwrap().take()
            && progress.generation == self.generation
            && progress.id == self.highlight_id
            && self.highlight_revision == Some(progress.revision)
            && self.highlight_base.is_none()
        {
            self.highlight = Some(progress.index);
        }
        self.highlight_id += 1;
        self.highlight_revision = None;
    }
    fn changed(&mut self) {
        self.preferred_column = None;
        self.pause_highlight();
        self.last_edit = Instant::now();
        self.cursor = self.cursor.min(self.doc.len());
        while self.cursor > 0 && !self.doc.char_boundary(self.cursor) {
            self.cursor -= 1;
        }
        if self.cursor > 0
            && self.cursor < self.doc.len()
            && self.doc.read(self.cursor - 1..self.cursor + 1) == b"\r\n"
        {
            self.cursor -= 1;
        }
        self.cursor = self.cursor.max(view::content_start(&self.doc));
        self.status = if self.doc.is_modified() {
            "Modified"
        } else {
            "No unsaved changes"
        }
        .into();
        if self.doc.history_evictions() > 0 {
            self.status
                .push_str(" · Undo history was trimmed to stay within history limits");
        }
        self.ensure_visible();
    }
    fn move_to(&mut self, mut offset: usize, select: bool) {
        self.break_typing_group();
        self.preferred_column = None;
        offset = offset.clamp(view::content_start(&self.doc), self.doc.len());
        while offset > 0 && !self.doc.char_boundary(offset) {
            offset -= 1;
        }
        if offset > 0 && offset < self.doc.len() && self.doc.read(offset - 1..offset + 1) == b"\r\n"
        {
            offset -= 1;
        }
        if select {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = offset;
        self.ensure_visible();
    }
    fn ensure_visible(&mut self) {
        self.reveal_cursor = true;
        let line = self.doc.line_of_offset(self.cursor);
        if line < self.top_line {
            self.top_line = line;
        }
        if line >= self.top_line + self.visible_rows {
            self.top_line = line.saturating_sub(self.visible_rows.saturating_sub(1));
        }
        let start = view::line_range(&self.doc, line).start;
        let byte = self.cursor.saturating_sub(start);
        if byte < self.horizontal {
            self.horizontal = byte;
        }
        if byte > self.horizontal + self.visible_bytes {
            self.horizontal = byte.saturating_sub(self.visible_bytes / 2);
        }
    }
    fn newline(&self) -> &'static str {
        self.newline_at(self.cursor)
    }
    fn newline_at(&self, offset: usize) -> &'static str {
        // Match the current line first; no unrelated line ending is ever rewritten.
        let line = self.doc.line_of_offset(offset);
        if line + 1 < self.doc.line_count() {
            let end = self.doc.line_start(line + 1);
            if end >= 2 && self.doc.read(end - 2..end) == b"\r\n" {
                return "\r\n";
            }
            return "\n";
        }
        if line > 0 {
            let start = self.doc.line_start(line);
            if start >= 2 && self.doc.read(start - 2..start) == b"\r\n" {
                return "\r\n";
            }
            return "\n";
        }
        if cfg!(windows) { "\r\n" } else { "\n" }
    }
    fn copy(&mut self, ctx: &egui::Context, cut: bool) {
        self.break_typing_group();
        let r = self.selection();
        if r.is_empty() {
            return;
        }
        if r.len() > view::CLIPBOARD_LIMIT {
            self.error = Some("Clipboard selection exceeds the 16 MiB safety limit. Select a smaller region; no bytes were cut.".into());
            return;
        }
        let text = String::from_utf8(self.doc.read(r)).expect("validated UTF-8 document");
        ctx.copy_text(text);
        if cut {
            self.delete_selection();
        }
    }
    fn request(&mut self, pending: Pending, ctx: &egui::Context) {
        if self.workspace_managed
            && matches!(pending, Pending::Reload)
            && matches!(
                self.pending,
                Some(Pending::CloseTab | Pending::QuitWorkspace)
            )
        {
            // Reload is a new destructive decision, not approval of a pending
            // close/quit. Abort that continuation before replacing its prompt.
            self.workspace_actions.push(WorkspaceAction::CancelClose);
            self.pending = None;
            self.continue_after_save = false;
        }
        if self.workspace_managed {
            let action = match pending {
                Pending::New => Some(WorkspaceAction::New),
                Pending::Open => Some(WorkspaceAction::OpenDialog),
                Pending::Exit => Some(WorkspaceAction::Quit),
                Pending::CloseTab => Some(WorkspaceAction::Close),
                _ => None,
            };
            if let Some(action) = action {
                self.workspace_actions.push(action);
                self.cancel_vim_input();
                return;
            }
        }
        if self.work.is_some() || self.dialog_pending {
            self.status = "Finish or cancel the current file operation first".into();
            return;
        }
        self.cancel_search();
        self.cancel_vim_input();
        if self.doc.is_modified() && self.ready {
            self.pending = Some(pending);
        } else {
            self.perform(pending, ctx);
        }
    }
    fn perform(&mut self, pending: Pending, ctx: &egui::Context) {
        self.cancel_search();
        self.pending = None;
        match pending {
            Pending::New => {
                self.cancel_highlight();
                self.cancel_search();
                self.doc = Document::new();
                self.path = None;
                self.stamp = None;
                self.ready = true;
                self.generation += 1;
                self.language = Language::Plain;
                self.reset_view();
                self.status = "New file".into();
            }
            Pending::Open => {
                self.dialog_pending = true;
                let tx = self.tx.clone();
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let p = rfd::FileDialog::new().pick_file();
                    let _ = tx.send(Message::OpenDialog(p));
                    ctx.request_repaint();
                });
            }
            Pending::Reload => {
                if let Some(path) = self.path.clone() {
                    if self.workspace_managed {
                        self.workspace_actions
                            .push(WorkspaceAction::ReloadPath(path));
                    } else {
                        self.reload_path(path, ctx);
                    }
                }
            }
            Pending::CloseTab => self.workspace_actions.push(WorkspaceAction::CloseApproved),
            Pending::QuitWorkspace => self.workspace_actions.push(WorkspaceAction::QuitApproved),
            Pending::Exit => {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
    fn open_path(&mut self, path: PathBuf, ctx: &egui::Context) {
        self.reloading = false;
        self.open_error_kind = None;
        self.cancel_highlight();
        self.cancel_search();
        self.generation += 1;
        let generation = self.generation;
        self.language = Language::from_path(&path);
        self.path = Some(path.clone());
        self.stamp = None;
        self.ready = false;
        self.doc = match file_io::preview_for_open(&path, 64 * 1024) {
            Ok(preview) => {
                Document::from_bytes(preview.into_bytes()).unwrap_or_else(|_| Document::new())
            }
            Err(e) => {
                self.open_error_kind = e.kind();
                self.error = Some(e.to_string());
                Document::new()
            }
        };
        self.reset_view();
        self.status = "Read-only preview · validating UTF-8 and building the line index…".into();
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        let total = std::fs::metadata(&path).map_or(0, |m| m.len() as usize);
        self.work = Some(Work {
            label: "Opening",
            cancel: cancel.clone(),
            progress: progress.clone(),
            total,
        });
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = file_io::load_for_open(&path, &cancel, &progress);
            let _ = tx.send(Message::Loaded(generation, result));
            ctx.request_repaint();
        });
    }
    fn reload_path(&mut self, path: PathBuf, ctx: &egui::Context) {
        if self.work.is_some() || !self.ready {
            return;
        }
        self.break_typing_group();
        self.cancel_highlight();
        self.cancel_search();
        self.generation += 1;
        let generation = self.generation;
        self.reloading = true;
        self.ready = false;
        self.open_error_kind = None;
        self.status = "Reloading · existing buffer retained until validation succeeds…".into();
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        let total = std::fs::metadata(&path).map_or(0, |m| m.len() as usize);
        self.work = Some(Work {
            label: "Opening",
            cancel: cancel.clone(),
            progress: progress.clone(),
            total,
        });
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = file_io::load_for_open(&path, &cancel, &progress);
            let _ = tx.send(Message::Loaded(generation, result));
            ctx.request_repaint();
        });
    }
    fn save_as(&mut self, ctx: &egui::Context) {
        if self.workspace_managed {
            self.workspace_actions.push(WorkspaceAction::SaveAs);
            return;
        }
        self.save_as_internal(ctx);
    }
    fn save_as_internal(&mut self, ctx: &egui::Context) {
        if !self.ready || self.work.is_some() || self.dialog_pending {
            return;
        }
        self.dialog_pending = true;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let name = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "untitled.txt".into());
        std::thread::spawn(move || {
            let p = rfd::FileDialog::new().set_file_name(name).save_file();
            let _ = tx.send(Message::SaveDialog(p));
            ctx.request_repaint();
        });
    }
    fn accept_save_path(&mut self, path: PathBuf, ctx: &egui::Context) {
        if path.exists() {
            self.overwrite = Some(path);
        } else {
            self.start_save(path, None, ctx);
        }
    }
    fn save(&mut self, ctx: &egui::Context) {
        if self.workspace_managed {
            self.workspace_actions.push(WorkspaceAction::Save);
            return;
        }
        self.save_internal(ctx);
    }
    fn save_internal(&mut self, ctx: &egui::Context) {
        if !self.ready || self.work.is_some() {
            return;
        }
        if let Some(path) = self.path.clone() {
            if let Some(stamp) = &self.stamp
                && !stamp.matches_current(&path).unwrap_or(false)
            {
                self.conflict = Some(path);
                return;
            }
            self.start_save(path, self.stamp.clone(), ctx);
        } else {
            self.save_as_internal(ctx);
        }
    }
    fn start_save(&mut self, path: PathBuf, expected: Option<FileStamp>, ctx: &egui::Context) {
        self.break_typing_group();
        if self.work.is_some() || !self.ready {
            return;
        }
        let doc = self.doc.snapshot();
        let rev = self.doc.revision();
        let generation = self.generation;
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        self.save_destination = Some(path.clone());
        self.work = Some(Work {
            label: "Saving",
            cancel: cancel.clone(),
            progress: progress.clone(),
            total: doc.len(),
        });
        self.status = "Saving to a separate temporary file…".into();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = file_io::save(&doc, &path, expected.as_ref(), &cancel, &progress);
            let _ = tx.send(Message::Saved(generation, rev, path, result));
            ctx.request_repaint();
        });
    }
    fn cancel_search(&mut self) {
        if let Some(w) = self.search_work.take() {
            w.cancel.store(true, Ordering::Relaxed);
        }
    }
    fn start_find(&mut self, backwards: bool, all: bool, ctx: &egui::Context) {
        if !self.ready || self.find.is_empty() || self.search_work.is_some() {
            return;
        }
        if all && self.replacement.contains('\0') {
            self.error = Some(
                "NUL bytes are not supported in editable UTF-8 text. The document was not changed."
                    .into(),
            );
            return;
        }
        let doc = self.doc.snapshot();
        let rev = doc.revision();
        let needle = self.find.clone();
        let replacement = self.replacement.clone();
        let start = if backwards {
            self.selection().start
        } else {
            self.selection().end
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicUsize::new(0));
        self.next_search_id += 1;
        let job_id = self.next_search_id;
        let generation = self.generation;
        self.search_work = Some(SearchWork {
            id: job_id,
            cancel: cancel.clone(),
            progress: progress.clone(),
        });
        self.status = if all {
            "Preparing replace all…".into()
        } else {
            "Searching…".into()
        };
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let message = if all {
                Message::Replaced(
                    generation,
                    job_id,
                    rev,
                    search::replace_all(&doc, &needle, &replacement, &cancel, &progress),
                )
            } else {
                let result =
                    search::search_wrapping(&doc, &needle, start, backwards, &cancel, &progress);
                Message::Found(generation, job_id, rev, result)
            };
            let _ = tx.send(message);
            ctx.request_repaint();
        });
    }
    fn poll(&mut self, ctx: &egui::Context) {
        let progress = self
            .highlight_progress
            .as_ref()
            .and_then(|slot| slot.lock().unwrap().take());
        if let Some(progress) = progress
            && progress.generation == self.generation
            && progress.revision == self.doc.revision()
            && progress.id == self.highlight_id
            && self.highlight_cancel.is_some()
        {
            self.highlight = Some(progress.index);
        }
        while let Ok(message) = self.rx.try_recv() {
            match message {
                Message::OpenDialog(path) => {
                    self.dialog_pending = false;
                    if let Some(p) = path {
                        if self.workspace_managed {
                            self.workspace_actions.push(WorkspaceAction::OpenPath(p));
                        } else {
                            self.open_path(p, ctx);
                        }
                    }
                }
                Message::SaveDialog(path) => {
                    self.dialog_pending = false;
                    if let Some(p) = path {
                        if self.workspace_managed {
                            self.workspace_actions.push(WorkspaceAction::SavePath(p));
                        } else {
                            self.accept_save_path(p, ctx);
                        }
                    } else {
                        self.continue_after_save = false;
                    }
                }
                Message::Loaded(g, result) if g == self.generation => {
                    let reloading = std::mem::take(&mut self.reloading);
                    self.work = None;
                    match result {
                        Ok(loaded) => {
                            self.open_error_kind = None;
                            self.external_observed = None;
                            self.external_notice = None;
                            self.doc = loaded.document;
                            self.path = Some(loaded.path);
                            self.stamp = Some(loaded.stamp);
                            self.ready = true;
                            self.reset_view();
                            self.status = "Ready · UTF-8 validated".into();
                            self.last_edit = Instant::now() - Duration::from_secs(1);
                        }
                        Err(e) => {
                            self.open_error_kind = e.kind();
                            self.error = Some(e.to_string());
                            if reloading {
                                self.ready = true;
                                self.status = "Reload did not complete · existing buffer and undo history retained".into();
                                self.last_edit = Instant::now() - Duration::from_secs(1);
                            } else {
                                self.status = "Read-only preview · opening did not complete".into();
                            }
                        }
                    }
                }
                Message::Saved(g, rev, path, result) if g == self.generation => {
                    self.work = None;
                    self.save_destination = None;
                    match result {
                        Ok(stamp) => {
                            self.external_observed = None;
                            self.external_notice = None;
                            self.doc.mark_saved_revision(rev);
                            self.stamp = Some(stamp);
                            self.path = Some(path.clone());
                            let language = Language::from_path(&path);
                            if language != self.language {
                                self.language = language;
                                self.cancel_highlight();
                            }
                            self.status = if self.doc.is_modified() {
                                "Saved snapshot · newer edits remain unsaved".into()
                            } else {
                                "Saved safely".into()
                            };
                            if self.continue_after_save {
                                self.continue_after_save = false;
                                if !self.doc.is_modified()
                                    && let Some(p) = self.pending.take()
                                {
                                    self.perform(p, ctx);
                                }
                            }
                        }
                        Err(e) => {
                            self.continue_after_save = false;
                            self.error = Some(e);
                            self.status =
                                "Save did not complete cleanly · edits retained; see details"
                                    .into();
                        }
                    }
                }
                Message::Found(g, id, rev, result)
                    if g == self.generation
                        && self.search_work.as_ref().is_some_and(|w| w.id == id) =>
                {
                    self.search_work = None;
                    if rev == self.doc.revision() {
                        match result {
                            Ok(Some(r)) => {
                                self.break_typing_group();
                                self.preferred_column = None;
                                self.anchor = Some(r.start);
                                self.cursor = r.end;
                                self.ensure_visible();
                                self.status = "Match selected".into();
                            }
                            Ok(None) => self.status = "No match".into(),
                            Err(e) => self.error = Some(e),
                        }
                    } else {
                        self.status = "Search result discarded because the document changed".into();
                    }
                }
                Message::Replaced(g, id, rev, result)
                    if g == self.generation
                        && self.search_work.as_ref().is_some_and(|w| w.id == id) =>
                {
                    self.search_work = None;
                    if rev == self.doc.revision() {
                        match result {
                            Ok((doc, count)) => {
                                if count > 0 {
                                    self.break_typing_group();
                                    let before = self.cursor_state();
                                    let revision = self.doc.revision();
                                    match self.doc.try_apply_snapshot(&doc) {
                                        Ok(true) => {
                                            self.cursor = self.cursor.min(self.doc.len());
                                            self.anchor = None;
                                            self.changed();
                                            self.record_cursor_edit(revision, before);
                                        }
                                        Ok(false) => {}
                                        Err(error) => {
                                            self.error = Some(error);
                                            self.status = "Replace all was not applied · document and undo history unchanged".into();
                                            continue;
                                        }
                                    }
                                }
                                self.status = if self.doc.revision() == rev {
                                    format!("Replaced {count} matches · document unchanged")
                                } else {
                                    format!("Replaced {count} matches · one undo action")
                                };
                                if self.doc.history_evictions() > 0
                                    && self.doc.undo_group_revision() == Some(rev)
                                {
                                    self.status.push_str(" · older undo history was trimmed");
                                }
                            }
                            Err(e) => {
                                self.error = Some(e);
                                self.status = "Replace all was not applied · document and undo history unchanged".into();
                            }
                        }
                    } else {
                        self.status = "Replace all discarded because the document changed".into();
                    }
                }
                Message::Highlighted(g, rev, id, finished, result)
                    if g == self.generation
                        && rev == self.doc.revision()
                        && id == self.highlight_id =>
                {
                    if finished {
                        self.highlight_cancel = None;
                        self.highlight_progress = None;
                    }
                    match result {
                        Ok(index) => {
                            if finished {
                                self.highlight_base = Some(index.clone());
                            }
                            self.highlight = Some(index);
                        }
                        Err(e) => {
                            self.status = format!("Plain text: {e}");
                        }
                    }
                }
                _ => {}
            }
        }
    }
    fn start_highlight(&mut self, ctx: &egui::Context) {
        if !self.ready
            || self.language == Language::Plain
            || self.highlight_revision == Some(self.doc.revision())
            || self.last_edit.elapsed() < Duration::from_millis(300)
        {
            return;
        }
        self.pause_highlight();
        let revision = self.doc.revision();
        self.highlight_revision = Some(revision);
        let cancel = Arc::new(AtomicBool::new(false));
        self.highlight_cancel = Some(cancel.clone());
        let progress_slot = Arc::new(Mutex::new(None));
        self.highlight_progress = Some(progress_slot.clone());
        let doc = self.doc.snapshot();
        // Retain the last complete base while an intermediate prefix is shown.
        let previous = self
            .highlight_base
            .as_ref()
            .or(self.highlight.as_ref())
            .cloned();
        let has_complete_base = self.highlight_base.is_some();
        let final_visible_line = self
            .top_line
            .saturating_add(self.visible_rows)
            .min(doc.line_count().saturating_sub(1));
        let visible_end = view::row_range(
            &doc,
            final_visible_line,
            self.horizontal,
            self.visible_bytes
                .saturating_mul(4)
                .min(view::MAX_ROW_BYTES),
        )
        .end;
        let first_through = initial_highlight_through(
            doc.len(),
            visible_end,
            previous.as_ref().map_or(0, |index| index.indexed_until()),
            has_complete_base,
        );
        let language = self.language;
        let generation = self.generation;
        let id = self.highlight_id;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut through = first_through;
            let mut base = previous.clone();
            loop {
                let result =
                    HighlightIndex::refresh(&doc, language, base.as_ref(), through, &cancel);
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                match result {
                    Ok(index) if index.indexed_until() < doc.len() => {
                        let next = index
                            .indexed_until()
                            .saturating_add(HIGHLIGHT_PROGRESS_BYTES)
                            .min(doc.len());
                        *progress_slot.lock().unwrap() = Some(HighlightProgress {
                            generation,
                            revision,
                            id,
                            index: index.clone(),
                        });
                        ctx.request_repaint();
                        if has_complete_base {
                            // Preserve the old complete suffix for convergence.
                            base = previous.clone();
                            through = doc.len();
                        } else {
                            // Initial indexing publishes bounded, replaceable
                            // progress. Typing cannot reset a 1 GB scan to byte 0.
                            base = Some(index);
                            through = next;
                        }
                    }
                    result => {
                        let _ =
                            tx.send(Message::Highlighted(generation, revision, id, true, result));
                        ctx.request_repaint();
                        break;
                    }
                }
            }
        });
    }
    fn undo(&mut self, redo: bool) {
        self.restore_history(redo);
    }
    fn event(&mut self, event: egui::Event, ctx: &egui::Context) {
        if matches!(event, egui::Event::WindowFocused(false)) {
            self.cancel_vim_input();
            return;
        }
        if !self.editor_focus || self.modal_open() {
            return;
        }
        if self.vim_enabled && self.vim_event(&event, ctx) {
            return;
        }
        match event {
            egui::Event::Text(t) if !t.is_empty() && t != "\n" && t != "\r" => {
                self.insert_typed(&t)
            }
            egui::Event::Paste(t) if !t.is_empty() => self.insert(&t),
            egui::Event::Copy => self.copy(ctx, false),
            egui::Event::Cut => self.copy(ctx, true),
            egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) => {
                self.break_typing_group();
                self.composition = text;
            }
            egui::Event::Ime(egui::ImeEvent::Commit(t)) => {
                self.composition.clear();
                if !t.is_empty() {
                    self.insert(&t);
                }
            }
            egui::Event::Ime(egui::ImeEvent::DeleteSurrounding {
                before_chars,
                after_chars,
            }) if self.ready => self.delete_surrounding(before_chars, after_chars),
            egui::Event::Key {
                key,
                pressed: true,
                modifiers: m,
                ..
            } => {
                if !self.composition.is_empty() {
                    if key == Key::Escape {
                        self.composition.clear();
                    }
                    return;
                }
                let command = m.command;
                if command {
                    match key {
                        Key::A => {
                            self.break_typing_group();
                            self.preferred_column = None;
                            self.anchor = Some(view::content_start(&self.doc));
                            self.cursor = self.doc.len();
                            self.ensure_visible();
                            return;
                        }
                        Key::Z => {
                            self.undo(m.shift);
                            return;
                        }
                        Key::Y => {
                            self.undo(true);
                            return;
                        }
                        _ => {}
                    }
                }
                let line = self.doc.line_of_offset(self.cursor);
                let r = view::line_range(&self.doc, line);
                let word = m.ctrl || m.alt;
                match key {
                    Key::ArrowLeft => {
                        let pos = if !m.shift && !self.selection().is_empty() {
                            self.selection().start
                        } else if m.mac_cmd {
                            r.start
                        } else if word {
                            match self.word_target(true) {
                                Some(pos) => pos,
                                None => return,
                            }
                        } else {
                            view::previous(&self.doc, self.cursor)
                        };
                        self.move_to(pos, m.shift);
                    }
                    Key::ArrowRight => {
                        let pos = if !m.shift && !self.selection().is_empty() {
                            self.selection().end
                        } else if m.mac_cmd {
                            r.end
                        } else if word {
                            match self.word_target(false) {
                                Some(pos) => pos,
                                None => return,
                            }
                        } else {
                            view::next(&self.doc, self.cursor)
                        };
                        self.move_to(pos, m.shift);
                    }
                    Key::ArrowUp | Key::ArrowDown | Key::PageUp | Key::PageDown => {
                        let jump = if matches!(key, Key::PageUp | Key::PageDown) {
                            self.visible_rows
                        } else {
                            1
                        };
                        let target = if matches!(key, Key::ArrowUp | Key::PageUp) {
                            line.saturating_sub(jump)
                        } else {
                            line.saturating_add(jump)
                                .min(self.doc.line_count().saturating_sub(1))
                        };
                        self.move_vertical(target, m.shift);
                    }
                    Key::Home => self.move_to(
                        if command {
                            view::content_start(&self.doc)
                        } else {
                            r.start
                        },
                        m.shift,
                    ),
                    Key::End => self.move_to(if command { self.doc.len() } else { r.end }, m.shift),
                    Key::Enter if self.ready => self.insert_newline(),
                    Key::Tab if self.ready => self.indent_selection(m.shift),
                    Key::Backspace if self.ready => self.delete_direction(true, word, m.mac_cmd),
                    Key::Delete if self.ready => self.delete_direction(false, word, m.mac_cmd),
                    Key::Escape => {
                        self.break_typing_group();
                        self.preferred_column = None;
                        self.anchor = None;
                        self.composition.clear();
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn modal_open(&self) -> bool {
        self.pending.is_some()
            || self.error.is_some()
            || self.overwrite.is_some()
            || self.conflict.is_some()
            || self.goto_open
            || self.dialog_pending
    }
    fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.modal_open() {
            return;
        }
        if self.find_open
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Escape))
        {
            self.cancel_search();
            self.find_open = false;
            self.find_focus = false;
            self.editor_focus = true;
            ctx.memory_mut(|memory| {
                if let Some(id) = memory.focused() {
                    memory.surrender_focus(id);
                }
            });
            return;
        }
        let previous = ctx.input_mut(|input| input.consume_key(egui::Modifiers::SHIFT, Key::F3));
        let next = ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::F3));
        if previous || next {
            self.cancel_vim_input();
            if self.find.is_empty() {
                self.find_open = true;
                self.find_focus = true;
                self.editor_focus = false;
            } else {
                self.start_find(previous, false, ctx);
            }
            return;
        }
        let shortcuts = ctx.input(|i| {
            (
                i.modifiers.command,
                i.modifiers.shift,
                i.key_pressed(Key::N),
                i.key_pressed(Key::O),
                i.key_pressed(Key::S),
                i.key_pressed(Key::F),
                i.key_pressed(Key::G),
                i.key_pressed(Key::Q),
            )
        });
        let (command, shift, n, o, s, f, g, q) = shortcuts;
        if !command {
            return;
        }
        if n {
            self.request(Pending::New, ctx);
        }
        if o {
            self.request(Pending::Open, ctx);
        }
        if s {
            if shift {
                self.save_as(ctx);
            } else {
                self.save(ctx);
            }
        }
        if f {
            self.cancel_vim_input();
            self.find_open = true;
            self.find_focus = true;
            self.editor_focus = false;
        }
        if g {
            self.cancel_vim_input();
            self.goto_open = true;
            self.editor_focus = false;
        }
        if q {
            self.request(Pending::Exit, ctx);
        }
    }

    fn toolbar(&mut self, root: &mut egui::Ui) {
        let context = root.ctx().clone();
        let ctx = &context;
        egui::Panel::top("toolbar").show(root, |ui| {
            ui.add_space(5.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("SimpTxt")
                        .strong()
                        .color(self.preferences.palette().accent)
                        .size(18.0),
                );
                ui.separator();
                if ui.button("New").clicked() {
                    self.request(Pending::New, ctx);
                }
                if ui.button("Open…").clicked() {
                    self.request(Pending::Open, ctx);
                }
                if ui
                    .add_enabled(self.ready && self.work.is_none(), egui::Button::new("Save"))
                    .clicked()
                {
                    self.save(ctx);
                }
                if ui
                    .add_enabled(
                        self.ready && self.work.is_none(),
                        egui::Button::new("Save as…"),
                    )
                    .clicked()
                {
                    self.save_as(ctx);
                }
                if ui
                    .add_enabled(
                        self.path.is_some() && self.ready && self.work.is_none(),
                        egui::Button::new("Reload"),
                    )
                    .on_hover_text("Reload the disk version; unsaved changes require confirmation")
                    .clicked()
                {
                    self.request(Pending::Reload, ctx);
                }
                ui.separator();
                if ui.button("Find").clicked() {
                    self.cancel_vim_input();
                    self.find_open = !self.find_open;
                    self.find_focus = self.find_open;
                    self.editor_focus = !self.find_open;
                }
                if ui.button("Undo").on_hover_text("Undo · Ctrl/⌘ Z").clicked() {
                    self.undo(false);
                }
                if ui
                    .button("Redo")
                    .on_hover_text("Redo · Ctrl/⌘ Shift Z")
                    .clicked()
                {
                    self.undo(true);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let old = self.language;
                    egui::ComboBox::from_id_salt("language")
                        .selected_text(format!("{:?}", self.language))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.language, Language::Plain, "Plain text");
                            ui.selectable_value(&mut self.language, Language::Rust, "Rust");
                            ui.selectable_value(&mut self.language, Language::C, "C / C++");
                            ui.selectable_value(&mut self.language, Language::Json, "JSON");
                        });
                    if self.language != old {
                        self.cancel_highlight();
                    }
                });
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let name = self
                    .path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Untitled".into());
                ui.label(
                    egui::RichText::new(if self.doc.is_modified() && self.ready {
                        format!("●  {name}")
                    } else {
                        name
                    })
                    .color(self.preferences.palette().muted),
                )
                .on_hover_text(
                    self.path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                );
            });
            ui.add_space(3.0);
            if let Some(notice) = self.external_notice.clone() {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&notice).color(self.preferences.palette().warning),
                        )
                        .truncate(),
                    )
                    .on_hover_text(&notice);
                    if ui.small_button("Dismiss").clicked() {
                        self.external_notice = None;
                    }
                });
            }
            if self.find_open {
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Find");
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.find)
                            .desired_width(220.0)
                            .return_key(None),
                    );
                    if self.find_focus {
                        response.request_focus();
                        self.find_focus = false;
                    }
                    if response.has_focus() {
                        if self.editor_focus {
                            self.cancel_vim_input();
                        }
                        self.editor_focus = false;
                    }
                    let enter = response.has_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    let backwards = ui.input(|i| i.modifiers.shift);
                    if ui.button("Previous").clicked() || (enter && backwards) {
                        self.start_find(true, false, ctx);
                    }
                    if ui.button("Next").clicked() || (enter && !backwards) {
                        self.start_find(false, false, ctx);
                    }
                    if ui.button("×").clicked() {
                        self.cancel_search();
                        self.find_open = false;
                        self.editor_focus = true;
                        ui.memory_mut(|m| m.surrender_focus(response.id));
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("With");
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.replacement).desired_width(220.0),
                    );
                    if response.has_focus() {
                        if self.editor_focus {
                            self.cancel_vim_input();
                        }
                        self.editor_focus = false;
                    }
                    if ui.button("Replace").clicked() {
                        let r = self.selection();
                        if !self.find.is_empty()
                            && r.len() == self.find.len()
                            && self.doc.read(r) == self.find.as_bytes()
                        {
                            let text = self.replacement.clone();
                            self.insert(&text);
                            self.start_find(false, false, ctx);
                        } else {
                            self.start_find(false, false, ctx);
                        }
                    }
                    if ui.button("Replace all").clicked() {
                        self.start_find(false, true, ctx);
                    }
                    if let Some(work) = &self.search_work {
                        ui.label(format!(
                            "{} MB scanned",
                            work.progress.load(Ordering::Relaxed) / 1_000_000
                        ));
                        if ui.button("Cancel search").clicked() {
                            self.cancel_search();
                            self.status = "Search cancelled · no replacements applied".into();
                        }
                    }
                });
            }
        });
    }
    fn footer(&mut self, root: &mut egui::Ui) {
        let pair_status = match self.bracket_cache.resolve(
            &self.doc,
            self.cursor,
            self.language,
            self.highlight.as_ref(),
        ) {
            simptxt::brackets::PairResult::UnknownBudget => Some("Pair: beyond scan limit"),
            simptxt::brackets::PairResult::UnknownSyntax => Some("Pair: syntax pending"),
            _ => None,
        };
        egui::Panel::bottom("status").show(root, |ui| {
            ui.horizontal(|ui| {
                if self.vim_enabled {
                    ui.label(
                        egui::RichText::new(format!(
                            "VIM · {}",
                            format!("{:?}", self.vim.mode()).to_uppercase()
                        ))
                        .strong()
                        .color(self.preferences.palette().accent),
                    );
                    if let Some(command) = self.vim.command_line() {
                        ui.monospace(format!(":{command}"));
                    }
                    ui.separator();
                }
                ui.label(egui::RichText::new(&self.status).color(if self.ready {
                    self.preferences.palette().muted
                } else {
                    self.preferences.palette().warning
                }));
                if let Some(work) = &self.work {
                    let progress =
                        work.progress.load(Ordering::Relaxed) as f32 / work.total.max(1) as f32;
                    ui.add(
                        egui::ProgressBar::new(progress.min(1.0))
                            .desired_width(100.0)
                            .text(work.label),
                    );
                    if ui.button("Cancel").clicked() {
                        work.cancel.store(true, Ordering::Relaxed);
                        self.status = "Cancelling at the next safe boundary…".into();
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let line = self.doc.line_of_offset(self.cursor);
                    let col = self
                        .cursor
                        .saturating_sub(view::line_range(&self.doc, line).start);
                    if ui
                        .small_button(format!("Ln {} · byte {}", line + 1, col + 1))
                        .on_hover_text("Go to line · Ctrl/⌘ G")
                        .clicked()
                    {
                        self.cancel_vim_input();
                        self.goto_open = true;
                        self.editor_focus = false;
                    }
                    ui.label(format!("{} bytes", self.doc.len()));
                    if let Some(status) = pair_status {
                        ui.weak(status);
                    }
                    if self.highlight_cancel.is_some() {
                        ui.label(if self.highlight.is_some() {
                            "Indexing remaining syntax…"
                        } else {
                            "Plain text · highlighting…"
                        });
                    }
                });
            });
        });
    }
    fn layout_row(&self, spans: &[HighlightSpan], text: &str) -> egui::text::LayoutJob {
        let mut job = egui::text::LayoutJob::default();
        job.wrap.max_width = f32::INFINITY;
        let mut at = 0;
        for (r, token) in spans {
            if r.start > at && r.start <= text.len() {
                job.append(
                    &text[at..r.start],
                    0.0,
                    text_format(self.preferences.palette().text, self.preferences.font_size),
                );
            }
            if r.start <= r.end && r.end <= text.len() {
                let color = match token {
                    TokenKind::Comment => self.preferences.palette().comment,
                    TokenKind::String => self.preferences.palette().string,
                    TokenKind::Number => self.preferences.palette().number,
                    TokenKind::Keyword => self.preferences.palette().keyword,
                };
                job.append(
                    &text[r.clone()],
                    0.0,
                    text_format(color, self.preferences.font_size),
                );
                at = r.end;
            }
        }
        if at < text.len() {
            job.append(
                &text[at..],
                0.0,
                text_format(self.preferences.palette().text, self.preferences.font_size),
            );
        }
        if text.is_empty() {
            job.append(
                "",
                0.0,
                text_format(self.preferences.palette().text, self.preferences.font_size),
            );
        }
        job
    }
    fn editor(&mut self, root: &mut egui::Ui) {
        let context = root.ctx().clone();
        let ctx = &context;
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(self.preferences.palette().background)
                    .inner_margin(8),
            )
            .show(root, |ui| {
                let available = ui.available_size();
                let (rect, response) = ui.allocate_exact_size(available, Sense::click_and_drag());
                let line_number_width = ui
                    .painter()
                    .layout_no_wrap(
                        self.doc.line_count().to_string(),
                        FontId::monospace(self.preferences.font_size * 0.8),
                        self.preferences.palette().muted,
                    )
                    .size()
                    .x;
                let gutter = (line_number_width + 24.0).max(65.0);
                let text_x = rect.left() + gutter;
                let text_rect = Rect::from_min_max(
                    Pos2::new(text_x, rect.top()),
                    Pos2::new(rect.right() - 16.0, rect.bottom() - 18.0),
                );
                self.visible_rows = (text_rect.height() / self.preferences.row_height())
                    .floor()
                    .max(1.0) as usize;
                self.visible_bytes = (text_rect.width() / (self.preferences.font_size * 0.62))
                    .floor()
                    .max(10.0) as usize;
                self.top_line = self.top_line.min(self.doc.line_count().saturating_sub(1));
                if response.clicked() || response.drag_started() {
                    if self.vim_enabled && self.vim.mode() != simptxt::vim::Mode::Insert {
                        self.cancel_vim_input();
                    }
                    self.editor_focus = true;
                    ui.memory_mut(|m| m.surrender_focus(m.focused().unwrap_or(response.id)));
                }
                if response.hovered() {
                    let delta = ui.input(|i| i.smooth_scroll_delta);
                    let shift = ui.input(|i| i.modifiers.shift);
                    if shift || delta.x.abs() > delta.y.abs() {
                        let d = if shift { delta.y } else { delta.x };
                        self.scroll_x += -d / 8.0;
                        let step = self.scroll_x.trunc();
                        self.scroll_x -= step;
                        self.horizontal = signed_scroll(self.horizontal, step);
                    } else {
                        self.scroll_y += -delta.y / self.preferences.row_height();
                        let step = self.scroll_y.trunc();
                        self.scroll_y -= step;
                        self.top_line = signed_scroll(self.top_line, step)
                            .min(self.doc.line_count().saturating_sub(1));
                    }
                }
                let painter = ui.painter().with_clip_rect(text_rect);
                let bracket_pair = match self.bracket_cache.resolve(
                    &self.doc,
                    self.cursor,
                    self.language,
                    self.highlight.as_ref(),
                ) {
                    simptxt::brackets::PairResult::Match { open, close } => Some([open, close]),
                    _ => None,
                };
                let selection = self.selection();
                let cursor_line = self.doc.line_of_offset(self.cursor);
                let mut click = None;
                let mut caret = None;
                let pointer = response.interact_pointer_pos();
                let row_ranges: Vec<_> = (0..self.visible_rows.saturating_add(1))
                    .map(|i| self.top_line.saturating_add(i))
                    .take_while(|&line| line < self.doc.line_count())
                    .map(|line| {
                        view::row_range(
                            &self.doc,
                            line,
                            self.horizontal,
                            self.visible_bytes
                                .saturating_mul(4)
                                .min(view::MAX_ROW_BYTES),
                        )
                    })
                    .collect();
                self.viewport_highlight
                    .prepare(&self.doc, self.highlight.as_ref(), &row_ranges);
                for (i, range) in row_ranges.iter().enumerate() {
                    let line = self.top_line + i;
                    if line >= self.doc.line_count() {
                        break;
                    }
                    let y = text_rect.top() + i as f32 * self.preferences.row_height();
                    let range = range.clone();
                    let bytes = self.doc.read(range.clone());
                    let text = std::str::from_utf8(&bytes).expect("UTF-8 viewport boundary");
                    let galley = ui.fonts_mut(|f| {
                        f.layout_job(self.layout_row(self.viewport_highlight.row(i), text))
                    });
                    let pos = Pos2::new(text_x, y);
                    ui.painter().text(
                        Pos2::new(text_x - 14.0, y + 2.0),
                        egui::Align2::RIGHT_TOP,
                        (line + 1).to_string(),
                        FontId::monospace(self.preferences.font_size * 0.8),
                        self.preferences.palette().muted,
                    );
                    if cursor_line == line {
                        painter.rect_filled(
                            Rect::from_min_size(
                                Pos2::new(text_x, y),
                                Vec2::new(text_rect.width(), self.preferences.row_height()),
                            ),
                            0.0,
                            self.preferences.palette().current_line,
                        );
                    }
                    let a = selection.start.max(range.start);
                    let b = selection.end.min(range.end);
                    if a < b {
                        let x1 = galley
                            .pos_from_cursor(egui::text::CCursor::new(
                                text[..a - range.start].chars().count(),
                            ))
                            .left();
                        let x2 = galley
                            .pos_from_cursor(egui::text::CCursor::new(
                                text[..b - range.start].chars().count(),
                            ))
                            .left();
                        painter.rect_filled(
                            Rect::from_min_max(
                                Pos2::new(text_x + x1, y),
                                Pos2::new(
                                    text_x + x2.max(x1 + 2.0),
                                    y + self.preferences.row_height(),
                                ),
                            ),
                            2.0,
                            self.preferences.palette().selection,
                        );
                    }
                    if self.cursor >= range.start && self.cursor <= range.end && cursor_line == line
                    {
                        let x = galley
                            .pos_from_cursor(egui::text::CCursor::new(
                                text[..self.cursor - range.start].chars().count(),
                            ))
                            .left();
                        if self.reveal_cursor && x > text_rect.width() - 10.0 {
                            let first = galley.cursor_from_pos(Vec2::new(
                                (x - text_rect.width() * 0.7).max(0.0),
                                0.0,
                            ));
                            let offset = view::byte_at_char(text, first.index.into());
                            self.horizontal = self.horizontal.saturating_add(offset.max(1));
                            ctx.request_repaint();
                        }
                        let cr = Rect::from_min_size(
                            Pos2::new(text_x + x, y + 1.0),
                            Vec2::new(1.5, self.preferences.row_height() - 3.0),
                        );
                        caret = Some(cr);
                    }
                    if let Some(p) = pointer
                        && p.x <= text_rect.right()
                        && p.y < text_rect.bottom()
                        && p.y >= y
                        && p.y < y + self.preferences.row_height()
                        && (response.clicked() || response.dragged() || response.drag_started())
                    {
                        let c = galley.cursor_from_pos(p - pos);
                        click = Some(range.start + view::byte_at_char(text, c.index.into()));
                    }
                    painter.galley(pos, galley.clone(), self.preferences.palette().text);
                    if let Some(pair) = bracket_pair {
                        for offset in pair {
                            if offset >= range.start && offset < range.end {
                                let column = text[..offset - range.start].chars().count();
                                let left = galley
                                    .pos_from_cursor(egui::text::CCursor::new(column))
                                    .left();
                                let right = galley
                                    .pos_from_cursor(egui::text::CCursor::new(column + 1))
                                    .left();
                                // Keep the outline separate from selection fill and glyphs.
                                painter.rect_stroke(
                                    Rect::from_min_max(
                                        Pos2::new(text_x + left - 1.0, y),
                                        Pos2::new(
                                            text_x + right.max(left + 2.0) + 1.0,
                                            y + self.preferences.row_height() - 2.0,
                                        ),
                                    ),
                                    2.0,
                                    egui::Stroke::new(1.5, self.preferences.palette().accent),
                                    egui::StrokeKind::Inside,
                                );
                            }
                        }
                    }
                    if cursor_line == line
                        && self.editor_focus
                        && self.ready
                        && let Some(caret) = caret
                    {
                        painter.rect_filled(caret, 0.0, self.preferences.palette().accent);
                    }
                    self.rendered_rows += 1;
                }
                self.reveal_cursor = false;
                if let Some(offset) = click {
                    let select = response.dragged() || ui.input(|i| i.modifiers.shift);
                    if response.drag_started() {
                        self.anchor = Some(offset);
                    }
                    self.move_to(offset, select);
                }
                if self.editor_focus
                    && self.ready
                    && let Some(caret) = caret
                {
                    ctx.output_mut(|o| {
                        o.ime = Some(egui::output::IMEOutput {
                            purpose: egui::IMEPurpose::Normal,
                            rect: text_rect,
                            cursor_rect: caret,
                            should_interrupt_composition: false,
                        })
                    });
                    if !self.composition.is_empty() {
                        painter.text(
                            caret.left_bottom(),
                            egui::Align2::LEFT_TOP,
                            &self.composition,
                            FontId::monospace(self.preferences.font_size),
                            self.preferences.palette().accent,
                        );
                    }
                }
                // Integer offsets are authoritative; sliders are only a coarse jump aid for huge files.
                let mut scroll = self.top_line as f64;
                let maxline = self.doc.line_count().saturating_sub(self.visible_rows) as f64;
                let bar = Rect::from_min_max(
                    Pos2::new(rect.right() - 14.0, rect.top()),
                    Pos2::new(rect.right(), text_rect.bottom()),
                );
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(bar));
                child.spacing_mut().slider_width = bar.height();
                if child
                    .add(
                        egui::Slider::new(&mut scroll, maxline.max(0.0)..=0.0)
                            .vertical()
                            .show_value(false),
                    )
                    .changed()
                {
                    self.top_line = scroll as usize;
                }
                let line = self.doc.line_of_offset(self.cursor);
                let line_len = view::line_range(&self.doc, line).len();
                if line_len > self.visible_bytes || self.horizontal > 0 {
                    let hbar = Rect::from_min_max(
                        Pos2::new(text_x, rect.bottom() - 16.0),
                        Pos2::new(text_rect.right(), rect.bottom()),
                    );
                    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(hbar));
                    child.spacing_mut().slider_width = hbar.width();
                    let mut h = self.horizontal as f64;
                    if child
                        .add(
                            egui::Slider::new(
                                &mut h,
                                0.0..=line_len.saturating_sub(self.visible_bytes) as f64,
                            )
                            .show_value(false),
                        )
                        .changed()
                    {
                        self.horizontal = h as usize;
                    }
                }
            });
    }
    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(error) = self.error.clone() {
            egui::Window::new("SimpTxt")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.set_max_width(480.0);
                    ui.label(error);
                    if ui.button("OK").clicked() {
                        self.error = None;
                    }
                });
            return;
        }
        if self.pending.is_some()
            && self.overwrite.is_none()
            && self.conflict.is_none()
            && !self.dialog_pending
        {
            egui::Window::new("Unsaved changes")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label("Save your changes before continuing?");
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                self.work.is_none() && !self.dialog_pending,
                                egui::Button::new("Save"),
                            )
                            .clicked()
                        {
                            self.continue_after_save = true;
                            self.save(ctx);
                        }
                        if ui
                            .add_enabled(
                                self.work.is_none() && !self.dialog_pending,
                                egui::Button::new("Discard changes"),
                            )
                            .clicked()
                            && let Some(p) = self.pending.take()
                        {
                            self.perform(p, ctx);
                        }
                        if ui.button("Cancel").clicked() {
                            if matches!(
                                self.pending,
                                Some(Pending::CloseTab | Pending::QuitWorkspace)
                            ) {
                                self.workspace_actions.push(WorkspaceAction::CancelClose);
                            }
                            self.pending = None;
                            self.continue_after_save = false;
                        }
                    });
                });
        }
        if let Some(path) = self.overwrite.clone() {
            egui::Window::new("Replace existing file?")
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label(path.display().to_string());
                    ui.label("The destination already exists. Replace its current contents?");
                    ui.horizontal(|ui| {
                        if ui.button("Replace").clicked() {
                            self.overwrite = None;
                            match FileStamp::read(&path) {
                                Ok(stamp) => self.start_save(path, Some(stamp), ctx),
                                Err(e) => self.error = Some(e.to_string()),
                            }
                        }
                        if ui.button("Cancel").clicked() {
                            self.overwrite = None;
                            self.continue_after_save = false;
                        }
                    });
                });
        }
        if let Some(path) = self.conflict.clone() {
            egui::Window::new("File changed outside SimpTxt").collapsible(false).show(ctx, |ui| { ui.set_max_width(480.0); ui.label("The file on disk no longer matches the version you opened. Your edits have not been written over it."); ui.horizontal(|ui| {
            if ui.button("Save elsewhere…").clicked() { self.conflict = None; self.save_as(ctx); }
            if ui.button("Reload disk version").clicked() { self.conflict = None; self.request(Pending::Reload,ctx); }
            if ui.button("Overwrite disk version").clicked() { self.conflict = None; match FileStamp::read(&path) { Ok(stamp) => self.start_save(path,Some(stamp),ctx),Err(e) => self.error = Some(e.to_string()) } }
            if ui.button("Cancel").clicked() { self.conflict = None; self.continue_after_save = false; }
        }); });
        }
        if self.goto_open {
            egui::Window::new("Go to line")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    let r = ui.add(egui::TextEdit::singleline(&mut self.goto).return_key(None));
                    r.request_focus();
                    let enter = r.has_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    if (ui.button("Go").clicked() || enter)
                        && let Ok(line) = self.goto.parse::<usize>()
                    {
                        let target = self.doc.line_start(
                            line.saturating_sub(1)
                                .min(self.doc.line_count().saturating_sub(1)),
                        );
                        self.move_to(target, false);
                        self.goto_open = false;
                        self.editor_focus = true;
                        ui.memory_mut(|m| m.surrender_focus(r.id));
                    }
                    if ui.button("Cancel").clicked() {
                        self.goto_open = false;
                        self.editor_focus = true;
                    }
                });
        }
    }
}

impl eframe::App for EditorApp {
    fn ui(&mut self, root: &mut egui::Ui, _: &mut eframe::Frame) {
        self.render(root);
    }
}
impl EditorApp {
    /// Exercise the real input/edit/undo path for an offscreen GPU benchmark.
    /// This is in-process input, not native OS keyboard automation.
    pub(crate) fn headless_exercise(&mut self, ctx: &egui::Context, step: usize) {
        assert!(self.ready, "editing must be enabled before the exercise");
        self.editor_focus = true;
        if step.is_multiple_of(2) {
            let offset = match step {
                0 => view::content_start(&self.doc),
                2 => self.doc.len() / 2,
                _ => self.doc.len(),
            };
            self.move_to(offset, false);
            self.event(egui::Event::Text("/* headless edit */".into()), ctx);
            assert!(self.doc.is_modified(), "input must change the document");
        } else {
            self.undo(false);
            assert!(
                !self.doc.is_modified(),
                "undo must restore the loaded document"
            );
        }
    }
    pub(crate) fn ready_to_capture(&self) -> bool {
        self.ready
            && self.work.is_none()
            && self.highlight_cancel.is_none()
            && (self.language == Language::Plain
                || self.highlight_revision == Some(self.doc.revision()))
    }
    pub(crate) fn render(&mut self, root: &mut egui::Ui) {
        let context = root.ctx().clone();
        let ctx = &context;
        self.poll(ctx);
        if !self.allow_close
            && ctx.input(|i| i.viewport().close_requested())
            && (self.doc.is_modified() || self.work.is_some())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.work.is_none() {
                self.pending = Some(Pending::Exit);
            } else {
                self.status = "Finish or cancel the file operation before closing".into();
            }
        }
        self.shortcuts(ctx);
        if self.modal_open() {
            root.disable();
        }
        self.toolbar(root);
        self.footer(root);
        for event in ctx.input(|i| i.events.clone()) {
            self.event(event, ctx);
        }
        self.editor(root);
        self.dialogs(ctx);
        self.start_highlight(ctx);
        let name = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into());
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
            "{}{} — SimpTxt",
            if self.doc.is_modified() && self.ready {
                "● "
            } else {
                ""
            },
            name
        )));
        if self.work.is_some()
            || self.search_work.is_some()
            || self.highlight_cancel.is_some()
            || self.last_edit.elapsed() < Duration::from_millis(500)
        {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
        if self.smoke {
            self.smoke_frames += 1;
            if self.smoke_frames == 2 {
                self.event(egui::Event::Text("// test\n".into()), ctx);
                assert!(self.doc.is_modified());
                self.undo(false);
                assert!(!self.doc.is_modified());
                self.undo(true);
                assert!(self.doc.is_modified());
            }
            if self.smoke_frames >= 5 {
                assert!(self.rendered_rows >= 5);
                println!(
                    "SIMPTXT_SMOKE_OK frames={} rendered_rows={}",
                    self.smoke_frames, self.rendered_rows
                );
                self.doc.mark_saved();
                self.smoke = false;
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            ctx.request_repaint();
        }
    }
}
impl Drop for EditorApp {
    fn drop(&mut self) {
        if let Some(w) = &self.work {
            w.cancel.store(true, Ordering::Relaxed);
        }
        self.cancel_search();
        self.cancel_highlight();
    }
}
fn text_format(color: Color32, font_size: f32) -> egui::TextFormat {
    egui::TextFormat {
        font_id: FontId::monospace(font_size),
        color,
        ..Default::default()
    }
}
fn signed_scroll(current: usize, delta: f32) -> usize {
    if delta < 0.0 {
        current.saturating_sub((-delta).ceil() as usize)
    } else {
        current.saturating_add(delta.ceil() as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(key: Key, shift: bool, command: bool) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers {
                shift,
                command,
                ctrl: command,
                ..Default::default()
            },
        }
    }
    #[test]
    fn keyboard_edit_select_cut_paste_undo() {
        let ctx = egui::Context::default();
        let mut app = EditorApp::create(&ctx, false);
        app.event(egui::Event::Text("a💚b".into()), &ctx);
        app.event(key(Key::ArrowLeft, false, false), &ctx);
        app.event(key(Key::ArrowLeft, true, false), &ctx);
        assert_eq!(app.selection(), 1..5);
        app.event(egui::Event::Cut, &ctx);
        assert_eq!(app.doc.read(0..app.doc.len()), b"ab");
        app.event(egui::Event::Paste("世界".into()), &ctx);
        assert_eq!(app.doc.read(0..app.doc.len()), "a世界b".as_bytes());
        app.event(key(Key::Z, false, true), &ctx);
        assert_eq!(app.doc.read(0..app.doc.len()), b"ab");
        app.event(key(Key::Z, true, true), &ctx);
        assert_eq!(app.doc.read(0..app.doc.len()), "a世界b".as_bytes());
    }
    #[test]
    fn deleting_selection_with_empty_replace_is_valid() {
        let ctx = egui::Context::default();
        let mut app = EditorApp::create(&ctx, false);
        app.insert("abc");
        app.anchor = Some(0);
        app.cursor = 3;
        app.insert("");
        assert!(app.doc.is_empty());
    }
    #[test]
    fn readonly_preview_rejects_every_mutation() {
        let ctx = egui::Context::default();
        let mut app = EditorApp::create(&ctx, false);
        app.doc = Document::from_bytes(b"preview".to_vec()).unwrap();
        app.ready = false;
        app.event(egui::Event::Paste("no".into()), &ctx);
        app.event(key(Key::Delete, false, false), &ctx);
        app.event(key(Key::Enter, false, false), &ctx);
        app.undo(false);
        assert_eq!(app.doc.read(0..app.doc.len()), b"preview");
    }
    #[test]
    fn dirty_replacement_requires_explicit_choice() {
        let ctx = egui::Context::default();
        let mut app = EditorApp::create(&ctx, false);
        app.insert("keep me");
        app.request(Pending::New, &ctx);
        assert!(app.pending.is_some());
        assert_eq!(app.doc.read(0..app.doc.len()), b"keep me");
        app.perform(Pending::New, &ctx);
        assert!(app.doc.is_empty());
        app.insert("again");
        app.perform(Pending::Exit, &ctx);
        assert!(app.allow_close);
    }
    #[test]
    fn egui_renders_only_bounded_viewport_and_unicode() {
        let ctx = egui::Context::default();
        let mut app = EditorApp::create(&ctx, false);
        app.doc = Document::from_bytes("/* long */ α💚".repeat(100_000).into_bytes()).unwrap();
        app.cursor = app.doc.len() / 2;
        while !app.doc.char_boundary(app.cursor) {
            app.cursor -= 1;
        }
        app.ensure_visible();
        for _ in 0..3 {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 700.0))),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                app.toolbar(ui);
                app.footer(ui);
                app.editor(ui);
            });
            assert!(!output.shapes.is_empty());
            assert!(app.rendered_rows < 100);
            output.textures_delta.clear();
        }
    }
    #[test]
    fn undo_never_leaves_cursor_inside_multibyte_character() {
        let ctx = egui::Context::default();
        let mut app = EditorApp::create(&ctx, false);
        app.insert("💚");
        app.move_to(0, false);
        app.insert("ab");
        app.undo(false);
        assert!(app.doc.char_boundary(app.cursor));
    }
}

#[cfg(test)]
#[path = "app_safety_tests.rs"]
mod safety_tests;

#[cfg(test)]
#[path = "app_input_tests.rs"]
mod input_tests;

#[cfg(test)]
#[path = "app_highlight_tests.rs"]
mod highlight_tests;
