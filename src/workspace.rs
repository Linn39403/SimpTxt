//! Multi-document ownership. Each tab owns its editor and worker channel, so a
//! completion can never be delivered to a different document after a switch.
use super::*;
use crate::project_tree::ProjectTree;
use std::{collections::HashMap, path::Path};

const MAX_OPEN_DOCUMENT_BYTES: usize = simptxt::document::MAX_DOCUMENT_BYTES;
const MAX_CONCURRENT_LOADS: usize = 2;
const MAX_TABS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct TabId(u64);

pub(super) enum WorkspaceAction {
    New,
    OpenDialog,
    OpenPath(PathBuf),
    ReloadPath(PathBuf),
    SavePath(PathBuf),
    Save,
    SaveAs,
    Close,
    CloseApproved,
    Quit,
    QuitApproved,
    CancelClose,
}

struct Tab {
    id: TabId,
    editor: EditorApp,
}

#[derive(Default)]
struct QuitState {
    // A discard/save decision approves exactly one document revision, not any
    // edits or replace-all results which happen to arrive later.
    approved: HashMap<TabId, u64>,
    prompted: Option<TabId>,
}

enum WorkspaceMessage {
    Folder(Option<PathBuf>),
}

/// The native window owns a workspace, not an interchangeable document slot.
pub struct WorkspaceApp {
    tabs: Vec<Tab>,
    active: TabId,
    next_tab_id: u64,
    preferences: EditorPreferences,
    vim_enabled: bool,
    project: ProjectTree,
    folder_dialog_pending: bool,
    deferred_project_path: Option<PathBuf>,
    tx: Sender<WorkspaceMessage>,
    rx: Receiver<WorkspaceMessage>,
    quitting: Option<QuitState>,
    allow_close: bool,
    suppress_input: bool,
    startup_warning: Option<String>,
    startup_load: Option<(TabId, u64)>,
}

impl WorkspaceApp {
    pub fn new(cc: &eframe::CreationContext<'_>, path: Option<PathBuf>, smoke: bool) -> Self {
        Self::from_startup_path(&cc.egui_ctx, path, smoke)
    }

    /// Startup failures are recoverable without dismissing a modal first.
    /// Keep this path shared with native startup so it can be exercised without
    /// a native window or a platform file picker.
    pub(crate) fn from_startup_path(
        ctx: &egui::Context,
        path: Option<PathBuf>,
        smoke: bool,
    ) -> Self {
        let mut app = Self::create(ctx, smoke);
        if let Some(path) = path {
            app.open_path(path, ctx);
            let index = app.active_index();
            if let Some(error) = app.tabs[index].editor.error.take() {
                app.recover_startup_tab(index, error, ctx);
            } else if app.tabs[index].editor.work.is_some() {
                app.startup_load = Some((app.active, app.tabs[index].editor.generation));
            }
        }
        app
    }

    fn create(ctx: &egui::Context, smoke: bool) -> Self {
        let mut editor = EditorApp::create(ctx, smoke);
        editor.workspace_managed = true;
        let (tx, rx) = mpsc::channel();
        Self {
            tabs: vec![Tab {
                id: TabId(1),
                editor,
            }],
            active: TabId(1),
            next_tab_id: 2,
            preferences: EditorPreferences::default(),
            vim_enabled: false,
            project: ProjectTree::new(),
            folder_dialog_pending: false,
            deferred_project_path: None,
            tx,
            rx,
            quitting: None,
            allow_close: false,
            suppress_input: false,
            startup_warning: None,
            startup_load: None,
        }
    }

    fn recover_startup_tab(&mut self, index: usize, error: String, ctx: &egui::Context) {
        let error = match self.tabs[index].editor.path.as_ref() {
            Some(path) => format!("Cannot open {}: {error}", path.display()),
            None => error,
        };
        // A failed initial preview must not become a loaded document or acquire
        // a save destination. Dropping it also cancels its jobs and isolates any
        // late completion on the discarded receiver.
        let mut editor = EditorApp::create(ctx, false);
        editor.workspace_managed = true;
        editor.preferences = self.preferences;
        editor.vim_enabled = self.vim_enabled;
        self.tabs[index].editor = editor;
        self.preferences.apply_theme(ctx);
        self.startup_load = None;
        self.startup_warning = Some(error);
    }

    fn finish_startup_open(&mut self, ctx: &egui::Context) {
        let Some((id, generation)) = self.startup_load else {
            return;
        };
        let Some(index) = self.index(id) else {
            self.startup_load = None;
            return;
        };
        let editor = &mut self.tabs[index].editor;
        if editor.generation != generation || editor.ready {
            // A later Open/Reload owns its own errors and safety dialogs.
            self.startup_load = None;
        } else if editor.work.is_none() {
            self.startup_load = None;
            if let Some(error) = editor.error.take() {
                self.recover_startup_tab(index, error, ctx);
            }
        }
    }

    fn index(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == id)
    }

    fn active_index(&self) -> usize {
        self.index(self.active)
            .expect("workspace always has an active tab")
    }

    fn active_editor(&mut self) -> &mut EditorApp {
        let index = self.active_index();
        &mut self.tabs[index].editor
    }

    fn switch_to(&mut self, id: TabId) {
        if self.active == id || self.index(id).is_none() {
            return;
        }
        self.active_editor().cancel_vim_input();
        self.active_editor().pause_highlight();
        self.active = id;
        self.active_editor().cancel_vim_input();
        self.suppress_input = true;
    }

    fn new_tab(&mut self, ctx: &egui::Context) -> TabId {
        self.try_new_tab(ctx).unwrap_or(self.active)
    }

    fn try_new_tab(&mut self, ctx: &egui::Context) -> Option<TabId> {
        if self.tabs.len() >= MAX_TABS {
            self.error(self.active, "The workspace limit is 128 tabs. Close a tab before creating or opening another file. Your existing documents are unchanged.".into());
            return None;
        }
        let id = TabId(self.next_tab_id);
        self.next_tab_id = self.next_tab_id.checked_add(1).expect("tab ID exhaustion");
        let mut editor = EditorApp::create(ctx, false);
        editor.workspace_managed = true;
        editor.preferences = self.preferences;
        editor.vim_enabled = self.vim_enabled;
        self.preferences.apply_theme(ctx);
        let had_active = self.index(self.active).is_some();
        self.tabs.push(Tab { id, editor });
        if had_active {
            self.switch_to(id);
        } else {
            self.active = id;
            self.suppress_input = true;
        }
        Some(id)
    }

    fn modal_owner(&self) -> Option<TabId> {
        self.tabs
            .iter()
            .find(|tab| tab.editor.modal_open())
            .map(|tab| tab.id)
    }

    fn modal_open(&self) -> bool {
        self.folder_dialog_pending || self.modal_owner().is_some()
    }

    fn error(&mut self, id: TabId, error: String) {
        if let Some(index) = self.index(id) {
            self.tabs[index].editor.error = Some(error);
            self.tabs[index].editor.cancel_vim_input();
        }
    }

    fn resident_bytes(&self) -> usize {
        self.tabs.iter().fold(0usize, |total, tab| {
            let editor = &tab.editor;
            let reserved = editor
                .work
                .as_ref()
                .filter(|work| work.label == "Opening")
                .map_or(0, |work| work.total);
            total.saturating_add(editor.doc.len().max(reserved))
        })
    }

    fn matching_tab(&self, path: &Path, excluding: Option<TabId>) -> Option<TabId> {
        self.tabs
            .iter()
            .filter(|tab| Some(tab.id) != excluding)
            .find_map(|tab| {
                let editor = &tab.editor;
                let matches = editor
                    .path
                    .iter()
                    .chain(editor.save_destination.iter())
                    .chain(editor.overwrite.iter())
                    .any(|other| same_file(path, other));
                matches.then_some(tab.id)
            })
    }

    fn open_path(&mut self, path: PathBuf, ctx: &egui::Context) {
        self.open_path_from(self.active, path, ctx);
    }

    fn open_path_from(&mut self, owner: TabId, path: PathBuf, ctx: &egui::Context) {
        if self.index(owner).is_none() {
            return;
        }
        // Check the supplied leaf before canonicalization. Otherwise a symlink
        // would silently evade file_io's explicit-target-only safety policy.
        let stamp = match FileStamp::read(&path) {
            Ok(stamp) => stamp,
            Err(error) => {
                self.error(owner, error);
                return;
            }
        };
        if let Some(id) = self.matching_tab(&path, None) {
            self.switch_to(id);
            return;
        }
        let size = usize::try_from(stamp.len).unwrap_or(usize::MAX);
        if size > MAX_OPEN_DOCUMENT_BYTES.saturating_sub(self.resident_bytes()) {
            self.error(owner, "Opening this file would exceed the 2 GiB combined open-document budget. Close another tab first. No existing document was changed.".into());
            return;
        }
        let loading = self
            .tabs
            .iter()
            .filter(|tab| tab.editor.work.is_some())
            .count();
        if loading >= MAX_CONCURRENT_LOADS {
            self.error(owner, "Two file operations are already running. Wait for one to finish or cancel it before opening another file.".into());
            return;
        }
        // Reuse only the pristine startup/last-close placeholder. Never replace
        // even an empty edited tab, an in-flight load, or a failed preview.
        let reuse = self.tabs.len() == 1 && {
            let editor = &self.tabs[0].editor;
            editor.path.is_none()
                && editor.doc.is_empty()
                && !editor.doc.is_modified()
                && editor.work.is_none()
                && editor.pending.is_none()
                && editor.ready
        };
        let id = if reuse {
            self.active
        } else {
            let Some(id) = self.try_new_tab(ctx) else {
                return;
            };
            id
        };
        let index = self.index(id).expect("new tab is present");
        self.tabs[index].editor.open_path(path, ctx);
        self.suppress_input = true;
    }

    fn reload_path(&mut self, owner: TabId, path: PathBuf, ctx: &egui::Context) {
        let Some(index) = self.index(owner) else {
            return;
        };
        if self.tabs[index].editor.work.is_some() {
            return;
        }
        let stamp = match FileStamp::read(&path) {
            Ok(stamp) => stamp,
            Err(error) => {
                self.error(owner, error);
                return;
            }
        };
        if self.matching_tab(&path, Some(owner)).is_some() {
            self.error(owner, "Cannot reload: this destination is owned by another open tab. Your existing buffer and undo history are unchanged.".into());
            return;
        }
        let other_bytes =
            self.tabs
                .iter()
                .filter(|tab| tab.id != owner)
                .fold(0usize, |total, tab| {
                    let reserved = tab
                        .editor
                        .work
                        .as_ref()
                        .filter(|work| work.label == "Opening")
                        .map_or(0, |work| work.total);
                    total.saturating_add(tab.editor.doc.len().max(reserved))
                });
        let size = usize::try_from(stamp.len).unwrap_or(usize::MAX);
        if size > MAX_OPEN_DOCUMENT_BYTES.saturating_sub(other_bytes) {
            self.error(owner, "Reloading this file would exceed the 2 GiB combined open-document budget. Close another tab first. Your existing buffer and undo history are unchanged.".into());
            return;
        }
        if self
            .tabs
            .iter()
            .filter(|tab| tab.editor.work.is_some())
            .count()
            >= MAX_CONCURRENT_LOADS
        {
            self.error(owner, "Two file operations are already running. Wait for one to finish or cancel it before reloading. Your existing buffer and undo history are unchanged.".into());
            return;
        }
        self.tabs[index].editor.open_path(path, ctx);
        self.suppress_input = true;
    }

    fn begin_save(&mut self, owner: TabId, save_as: bool, ctx: &egui::Context) {
        let Some(index) = self.index(owner) else {
            return;
        };
        if self.tabs[index].editor.work.is_some() || !self.tabs[index].editor.ready {
            return;
        }
        if self.folder_dialog_pending
            || self
                .tabs
                .iter()
                .any(|tab| tab.id != owner && tab.editor.modal_open())
        {
            self.tabs[index].editor.status =
                "Finish or cancel the other tab's dialog before saving".into();
            self.tabs[index].editor.continue_after_save = false;
            return;
        }
        if self
            .tabs
            .iter()
            .filter(|tab| tab.editor.work.is_some())
            .count()
            >= MAX_CONCURRENT_LOADS
        {
            self.tabs[index].editor.status = "Two file operations are already running. Wait for one to finish or cancel it before saving".into();
            self.tabs[index].editor.continue_after_save = false;
            return;
        }
        if save_as {
            self.tabs[index].editor.save_as_internal(ctx);
        } else {
            self.tabs[index].editor.save_internal(ctx);
        }
    }

    fn save_path(&mut self, owner: TabId, path: PathBuf, ctx: &egui::Context) {
        let Some(owner_index) = self.index(owner) else {
            return;
        };
        // Reject symlinks even when the canonical target is another open tab.
        if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            self.error(owner, "The Save As destination is a symbolic link. Choose its intended target explicitly instead.".into());
            self.tabs[owner_index].editor.continue_after_save = false;
            return;
        }
        if let Some(other) = self.matching_tab(&path, Some(owner)) {
            let name = self.tabs[self.index(other).unwrap()]
                .editor
                .path
                .as_ref()
                .map_or_else(
                    || path.display().to_string(),
                    |path| path.display().to_string(),
                );
            self.error(owner, format!("Cannot save as {name}: that file is already open or is a pending save destination in another tab. Switch to that tab or choose a different destination. Your edits are unchanged."));
            self.tabs[owner_index].editor.continue_after_save = false;
            return;
        }
        self.tabs[owner_index].editor.accept_save_path(path, ctx);
    }

    fn begin_open_dialog(&mut self, owner: TabId, ctx: &egui::Context) {
        if self.modal_open() {
            return;
        }
        if let Some(index) = self.index(owner) {
            self.tabs[index].editor.cancel_vim_input();
            self.tabs[index].editor.perform(Pending::Open, ctx);
            self.suppress_input = true;
        }
    }

    fn begin_folder_dialog(&mut self, ctx: &egui::Context) {
        if self.modal_open() {
            return;
        }
        self.active_editor().cancel_vim_input();
        self.folder_dialog_pending = true;
        self.suppress_input = true;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let path = rfd::FileDialog::new().pick_folder();
            let _ = tx.send(WorkspaceMessage::Folder(path));
            ctx.request_repaint();
        });
    }

    fn request_close(&mut self, id: TabId, ctx: &egui::Context) {
        let Some(index) = self.index(id) else {
            return;
        };
        if self.modal_open() {
            return;
        }
        if self.tabs[index]
            .editor
            .work
            .as_ref()
            .is_some_and(|work| work.label == "Saving")
        {
            self.tabs[index].editor.status =
                "Wait for the save to finish, or cancel it before closing this tab".into();
            return;
        }
        self.switch_to(id);
        let editor = self.active_editor();
        editor.cancel_search();
        editor.cancel_vim_input();
        if editor.ready && editor.doc.is_modified() {
            editor.pending = Some(Pending::CloseTab);
        } else {
            self.remove_tab(id, ctx);
        }
        self.suppress_input = true;
    }

    fn remove_tab(&mut self, id: TabId, ctx: &egui::Context) {
        let Some(index) = self.index(id) else {
            return;
        };
        // Dropping a tab cancels all its jobs; late sends fail on its receiver.
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.new_tab(ctx);
        } else if self.active == id {
            self.active = self.tabs[index.min(self.tabs.len() - 1)].id;
            self.active_editor().cancel_vim_input();
        }
        self.suppress_input = true;
    }

    fn request_quit(&mut self) {
        if self.quitting.is_some() {
            return;
        }
        if self.modal_open() {
            self.active_editor().status =
                "Finish or cancel the current dialog before quitting".into();
            return;
        }
        self.quitting = Some(QuitState::default());
        for tab in &mut self.tabs {
            tab.editor.cancel_search();
            tab.editor.cancel_vim_input();
        }
        self.suppress_input = true;
    }

    fn advance_quit(&mut self, ctx: &egui::Context) {
        let Some(quit) = &self.quitting else {
            return;
        };
        if quit.prompted.is_some() {
            return;
        }
        // A save may still be committing. Never destroy its owner or exit early.
        if self.tabs.iter().any(|tab| {
            tab.editor
                .work
                .as_ref()
                .is_some_and(|work| work.label == "Saving")
        }) {
            self.active_editor().status = "Waiting for saves to finish before quitting…".into();
            return;
        }
        if self.modal_open() {
            return;
        }
        let next = self
            .tabs
            .iter()
            .find(|tab| {
                tab.editor.ready
                    && tab.editor.doc.is_modified()
                    && quit.approved.get(&tab.id) != Some(&tab.editor.doc.revision())
            })
            .map(|tab| tab.id);
        if let Some(id) = next {
            self.switch_to(id);
            self.active_editor().pending = Some(Pending::QuitWorkspace);
            self.quitting.as_mut().unwrap().prompted = Some(id);
        } else {
            self.allow_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn process_actions(&mut self, ctx: &egui::Context) {
        let mut actions = Vec::new();
        for tab in &mut self.tabs {
            actions.extend(
                tab.editor
                    .workspace_actions
                    .drain(..)
                    .map(|action| (tab.id, action)),
            );
        }
        for (owner, action) in actions {
            if self.index(owner).is_none() {
                continue;
            }
            match action {
                WorkspaceAction::New => {
                    if !self.modal_open() {
                        self.new_tab(ctx);
                    }
                }
                WorkspaceAction::OpenDialog => self.begin_open_dialog(owner, ctx),
                WorkspaceAction::OpenPath(path) => self.open_path_from(owner, path, ctx),
                WorkspaceAction::ReloadPath(path) => self.reload_path(owner, path, ctx),
                WorkspaceAction::SavePath(path) => self.save_path(owner, path, ctx),
                WorkspaceAction::Save => self.begin_save(owner, false, ctx),
                WorkspaceAction::SaveAs => self.begin_save(owner, true, ctx),
                WorkspaceAction::Close => self.request_close(owner, ctx),
                WorkspaceAction::CloseApproved => {
                    let index = self.index(owner).unwrap();
                    if self.tabs[index]
                        .editor
                        .work
                        .as_ref()
                        .is_some_and(|work| work.label == "Saving")
                    {
                        self.tabs[index].editor.status =
                            "Wait for the save to finish, or cancel it before closing this tab"
                                .into();
                    } else {
                        self.remove_tab(owner, ctx);
                    }
                }
                WorkspaceAction::Quit => self.request_quit(),
                WorkspaceAction::QuitApproved => {
                    let revision = self.tabs[self.index(owner).unwrap()].editor.doc.revision();
                    if let Some(quit) = &mut self.quitting {
                        quit.approved.insert(owner, revision);
                        quit.prompted = None;
                    }
                }
                WorkspaceAction::CancelClose => self.quitting = None,
            }
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        for tab in &mut self.tabs {
            tab.editor.poll(ctx);
        }
        self.finish_startup_open(ctx);
        while let Ok(WorkspaceMessage::Folder(path)) = self.rx.try_recv() {
            self.folder_dialog_pending = false;
            self.suppress_input = true;
            if let Some(path) = path {
                self.deferred_project_path = None;
                self.project.set_root(path);
            }
        }
        self.project.poll(ctx);
        self.process_actions(ctx);
        self.advance_quit(ctx);
    }

    fn global_shortcuts(&mut self, ctx: &egui::Context) {
        if self.modal_open() || self.quitting.is_some() {
            return;
        }
        let (command, shift, tab, close) = ctx.input(|input| {
            (
                input.modifiers.ctrl || input.modifiers.command,
                input.modifiers.shift,
                input.key_pressed(Key::Tab),
                input.key_pressed(Key::W),
            )
        });
        if !command {
            return;
        }
        if tab {
            let index = self.active_index();
            let next = if shift {
                (index + self.tabs.len() - 1) % self.tabs.len()
            } else {
                (index + 1) % self.tabs.len()
            };
            self.switch_to(self.tabs[next].id);
            // Even a one-tab switch must not insert a literal Tab.
            self.suppress_input = true;
        }
        if close {
            self.request_close(self.active, ctx);
        }
    }

    fn controls(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        egui::Panel::top("workspace_controls").show(root, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Open folder…").clicked() {
                    self.begin_folder_dialog(&ctx);
                }
                let theme_label = match self.preferences.theme {
                    crate::preferences::Theme::Dark => "Light theme",
                    crate::preferences::Theme::Light => "Dark theme",
                };
                if ui.button(theme_label).clicked() {
                    self.preferences.toggle_theme();
                    self.preferences.apply_theme(&ctx);
                }
                if ui.checkbox(&mut self.vim_enabled, "Vim mode").changed() {
                    for tab in &mut self.tabs {
                        tab.editor.cancel_vim_input();
                        tab.editor.vim_enabled = self.vim_enabled;
                        tab.editor.vim.reset();
                    }
                    self.suppress_input = true;
                }
                if ui
                    .small_button("−")
                    .on_hover_text("Zoom out · Ctrl/⌘ −")
                    .clicked()
                {
                    self.preferences.font_size =
                        (self.preferences.font_size - 1.0).max(crate::preferences::MIN_FONT_SIZE);
                }
                if ui
                    .small_button(format!("{} px", self.preferences.font_size.round()))
                    .on_hover_text("Reset zoom · Ctrl/⌘ 0")
                    .clicked()
                {
                    self.preferences.reset_zoom();
                }
                if ui
                    .small_button("+")
                    .on_hover_text("Zoom in · Ctrl/⌘ +")
                    .clicked()
                {
                    self.preferences.font_size =
                        (self.preferences.font_size + 1.0).min(crate::preferences::MAX_FONT_SIZE);
                }
            });
        });
    }

    fn tab_bar(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        let mut switch = None;
        let mut close = None;
        let mut add = false;
        egui::Panel::top("file_tabs").show(root, |ui| {
            egui::ScrollArea::horizontal()
                .id_salt("tab_scroll")
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for tab in &self.tabs {
                            let editor = &tab.editor;
                            let name = editor
                                .path
                                .as_ref()
                                .and_then(|path| path.file_name())
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "Untitled".into());
                            let label = format!(
                                "{}{}",
                                if editor.ready && editor.doc.is_modified() {
                                    "● "
                                } else {
                                    ""
                                },
                                name
                            );
                            ui.push_id(tab.id.0, |ui| {
                                if ui
                                    .selectable_label(tab.id == self.active, label)
                                    .on_hover_text(
                                        editor
                                            .path
                                            .as_ref()
                                            .map(|path| path.display().to_string())
                                            .unwrap_or_else(|| "Unsaved file".into()),
                                    )
                                    .clicked()
                                {
                                    switch = Some(tab.id);
                                }
                                if ui
                                    .small_button("×")
                                    .on_hover_text("Close tab · Ctrl/⌘ W")
                                    .clicked()
                                {
                                    close = Some(tab.id);
                                }
                            });
                            ui.separator();
                        }
                        add = ui
                            .small_button("+")
                            .on_hover_text("New tab · Ctrl/⌘ N")
                            .clicked();
                    });
                });
        });
        if let Some(id) = switch {
            self.switch_to(id);
        }
        if let Some(id) = close {
            self.request_close(id, &ctx);
        }
        if add {
            self.new_tab(&ctx);
        }
    }

    fn startup_banner(&mut self, root: &mut egui::Ui) {
        let Some(warning) = self.startup_warning.as_ref() else {
            return;
        };
        let mut dismiss = false;
        egui::Panel::top("startup_warning").show(root, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Could not open the startup file")
                        .strong()
                        .color(self.preferences.palette().warning),
                );
                dismiss = ui.small_button("Dismiss").clicked();
            });
            ui.add(egui::Label::new(warning).truncate())
                .on_hover_text(warning);
            ui.label(
                "A blank Untitled tab is ready. Keep editing, choose New, or Open another file. The failed open did not create a file.",
            );
        });
        if dismiss {
            self.startup_warning = None;
        }
    }

    pub(crate) fn headless(ctx: &egui::Context, path: &Path) -> Self {
        let mut app = Self::create(ctx, false);
        app.open_path(path.to_path_buf(), ctx);
        app
    }

    pub(crate) fn capture_cursor(&mut self, offset: usize) {
        self.active_editor().move_to(offset, false);
    }

    pub(crate) fn ready_to_capture(&self) -> bool {
        self.tabs
            .iter()
            .all(|tab| tab.editor.ready && tab.editor.work.is_none())
            && self.tabs[self.active_index()].editor.ready_to_capture()
            && self.project.is_settled()
    }

    pub(crate) fn headless_exercise(&mut self, ctx: &egui::Context, step: usize) {
        // This benchmark exercises ordinary editable input independently of
        // whichever optional keyboard mode is being shown in the capture.
        let editor = self.active_editor();
        let enabled = editor.vim_enabled;
        editor.vim_enabled = false;
        editor.headless_exercise(ctx, step);
        editor.vim_enabled = enabled;
    }

    pub(crate) fn configure_capture(
        &mut self,
        theme: crate::preferences::Theme,
        vim: bool,
        folder: Option<PathBuf>,
        extra_paths: Vec<PathBuf>,
        ctx: &egui::Context,
    ) {
        self.preferences.theme = theme;
        self.preferences.apply_theme(ctx);
        self.vim_enabled = vim;
        if let Some(folder) = folder {
            self.project.set_root(folder);
        }
        let original_active = self.active;
        for path in extra_paths {
            self.open_path(path, ctx);
        }
        self.switch_to(original_active);
        if let Some(path) = self.active_editor().path.clone() {
            self.project.reveal_file(&path);
        }
        for tab in &mut self.tabs {
            tab.editor.preferences = self.preferences;
            tab.editor.vim_enabled = vim;
            tab.editor.cancel_vim_input();
        }
    }

    pub(crate) fn render(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        self.suppress_input = false;
        self.poll(&ctx);
        if ctx.input(|input| {
            input
                .events
                .iter()
                .any(|event| matches!(event, egui::Event::WindowFocused(false)))
        }) {
            for tab in &mut self.tabs {
                tab.editor.cancel_vim_input();
            }
            self.suppress_input = true;
        }
        if !self.allow_close && ctx.input(|input| input.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.request_quit();
        }
        self.preferences.handle_zoom(&ctx);
        self.global_shortcuts(&ctx);
        if !self.modal_open() && self.quitting.is_none() {
            self.active_editor().shortcuts(&ctx);
            self.process_actions(&ctx);
        }
        if let Some(owner) = self.modal_owner() {
            self.switch_to(owner);
            self.active_editor().cancel_vim_input();
        }
        let blocked = self.modal_open() || self.quitting.is_some();
        if blocked {
            root.disable();
        }
        self.controls(root);
        self.tab_bar(root);
        self.startup_banner(root);
        let mut selected_path = None;
        if self.project.root().is_some() {
            let panel = egui::Panel::left("project_sidebar")
                .resizable(true)
                .default_size(220.0)
                .size_range(150.0..=420.0)
                .show(root, |ui| {
                    selected_path = self.project.render(ui);
                });
            if ctx.input(|input| {
                input.pointer.primary_clicked()
                    && input
                        .pointer
                        .interact_pos()
                        .is_some_and(|position| panel.response.rect.contains(position))
            }) {
                self.active_editor().cancel_vim_input();
                self.active_editor().editor_focus = false;
            }
        }
        if let Some(path) = selected_path {
            self.deferred_project_path = Some(path);
        }
        if !self.modal_open()
            && self.quitting.is_none()
            && let Some(path) = self.deferred_project_path.take()
        {
            self.open_path(path, &ctx);
        }
        let preferences = self.preferences;
        self.active_editor().preferences = preferences;
        self.active_editor().toolbar(root);
        self.process_actions(&ctx);
        self.active_editor().preferences = preferences;
        self.active_editor().footer(root);
        let blocked = blocked || self.modal_open();
        if blocked {
            root.disable();
        }
        if ctx.memory(|memory| memory.focused().is_some()) {
            if self.active_editor().editor_focus {
                self.active_editor().cancel_vim_input();
            }
            self.active_editor().editor_focus = false;
        }
        if !blocked && !self.suppress_input {
            for event in ctx.input(|input| input.events.clone()) {
                self.active_editor().event(event, &ctx);
            }
        }
        self.active_editor().editor(root);
        self.active_editor().dialogs(&ctx);
        if self
            .quitting
            .as_ref()
            .is_some_and(|quit| quit.prompted.is_none())
            && !self.modal_open()
            && !self.allow_close
        {
            egui::Window::new("Finishing file operations")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(&ctx, |ui| {
                    ui.label("Waiting for saves to finish safely before quitting.");
                    if ui.button("Cancel quit").clicked() {
                        self.quitting = None;
                    }
                });
        }
        self.process_actions(&ctx);
        self.advance_quit(&ctx);
        // Only the selected document starts syntax work. Inactive workers can
        // finish and be polled, but switching does not launch N new scans.
        self.active_editor().start_highlight(&ctx);
        let editor = self.active_editor();
        let name = editor
            .path
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into());
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
            "{}{} — SimpTxt",
            if editor.ready && editor.doc.is_modified() {
                "● "
            } else {
                ""
            },
            name
        )));
        if self.tabs.iter().any(|tab| {
            let editor = &tab.editor;
            editor.work.is_some()
                || editor.search_work.is_some()
                || editor.highlight_cancel.is_some()
                || editor.last_edit.elapsed() < Duration::from_millis(500)
        }) {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
        self.smoke(&ctx);
    }

    fn smoke(&mut self, ctx: &egui::Context) {
        let editor = self.active_editor();
        if !editor.smoke {
            return;
        }
        editor.smoke_frames += 1;
        if editor.smoke_frames == 2 {
            editor.event(egui::Event::Text("// test\n".into()), ctx);
            assert!(editor.doc.is_modified());
            editor.undo(false);
            assert!(!editor.doc.is_modified());
            editor.undo(true);
            assert!(editor.doc.is_modified());
        }
        if editor.smoke_frames >= 5 {
            assert!(editor.rendered_rows >= 5);
            println!(
                "SIMPTXT_SMOKE_OK frames={} rendered_rows={}",
                editor.smoke_frames, editor.rendered_rows
            );
            editor.doc.mark_saved();
            editor.smoke = false;
            self.allow_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint();
    }
}

impl eframe::App for WorkspaceApp {
    fn ui(&mut self, root: &mut egui::Ui, _: &mut eframe::Frame) {
        self.render(root);
    }
}

fn canonical_key(path: &Path) -> Option<PathBuf> {
    if let Ok(path) = std::fs::canonicalize(path) {
        return Some(path);
    }
    // A new Save As target does not exist yet, but its parent does. Preserve
    // the leaf exactly; don't lowercase names on case-sensitive filesystems.
    let name = path.file_name()?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::canonicalize(parent)
        .ok()
        .map(|parent| parent.join(name))
}

fn same_file(first: &Path, second: &Path) -> bool {
    if first == second {
        return true;
    }
    if let (Some(first), Some(second)) = (canonical_key(first), canonical_key(second))
        && first == second
    {
        return true;
    }
    // Compare native file handles for existing files (including Windows file
    // IDs). Never case-fold lossy display names or assume all Windows folders
    // are case-insensitive. Invalid/unavailable paths simply do not match.
    if same_file::is_same_file(first, second).unwrap_or(false) {
        return true;
    }
    false
}

#[cfg(test)]
#[path = "workspace_regression_tests.rs"]
mod regression_tests;
