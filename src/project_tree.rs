//! A bounded, read-only project browser.
//!
//! There is one worker and at most one outstanding job. Listing a directory
//! reads only a bounded prefix, never its descendants. "Load more" rereads a
//! larger prefix rather than retaining an unbounded number of directory handles.
//! Filesystem iteration order can change between loads; Refresh starts over.
//!
//! Paths, not display strings, identify entries. Links and Windows reparse
//! points are never browsable or selectable. The worker checks every component
//! from the chosen root before listing or selecting. As with the editor's file
//! loader, portable path checks are not a security boundary against concurrent
//! filesystem replacement; the eventual file loader must still reject links.

use eframe::egui;
use std::{
    collections::BTreeMap,
    fs::{self, Metadata},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    time::Duration,
};

const PAGE_ENTRIES: usize = 256;
const MAX_DIRECTORY_ENTRIES: usize = 2_048;
const MAX_CACHED_ENTRIES: usize = 8_192;
const MAX_EXPANDED_DIRECTORIES: usize = 128;
const ROW_HEIGHT: f32 = 24.0;

#[derive(Clone, Debug, PartialEq, Eq)]
enum EntryKind {
    Directory,
    File,
    Link,
    Other,
    Unavailable(String),
}

#[derive(Clone, Debug)]
struct Entry {
    path: PathBuf,
    display_name: String,
    kind: EntryKind,
}

#[derive(Debug)]
enum DirectoryState {
    Queued,
    Loading(u64),
    Ready,
    Failed(String),
}

#[derive(Debug)]
struct Directory {
    entries: Vec<Entry>,
    has_more: bool,
    desired_limit: usize,
    state: DirectoryState,
}

impl Directory {
    fn queued() -> Self {
        Self {
            entries: Vec::new(),
            has_more: false,
            desired_limit: PAGE_ENTRIES,
            state: DirectoryState::Queued,
        }
    }
}

#[derive(Debug)]
struct Listing {
    entries: Vec<Entry>,
    has_more: bool,
}

#[derive(Clone, Debug)]
enum Job {
    List { path: PathBuf, limit: usize },
    Select { path: PathBuf },
}

impl Job {
    fn path(&self) -> &Path {
        match self {
            Self::List { path, .. } | Self::Select { path } => path,
        }
    }
}

struct Request {
    id: u64,
    generation: u64,
    root: PathBuf,
    job: Job,
    cancel: Arc<AtomicBool>,
    context: egui::Context,
}

#[derive(Debug)]
enum Outcome {
    Listing(Result<Listing, String>),
    Selection(Result<PathBuf, String>),
}

#[derive(Debug)]
struct Response {
    id: u64,
    generation: u64,
    path: PathBuf,
    outcome: Outcome,
}

struct Active {
    id: u64,
    generation: u64,
    path: PathBuf,
    cancel: Arc<AtomicBool>,
}

/// Read-only project navigation. Changing its root never changes editor tabs.
///
/// Call `poll` each app frame, then `render` inside a side panel. File selection
/// is verified on the worker, so the selected path may be returned a later frame.
pub struct ProjectTree {
    root: Option<PathBuf>,
    directories: BTreeMap<PathBuf, Directory>,
    cached_entries: usize,
    generation: Arc<AtomicU64>,
    stopped: Arc<AtomicBool>,
    next_request: u64,
    active: Option<Active>,
    tx: Option<SyncSender<Request>>,
    rx: Receiver<Response>,
    pending_selection: Option<PathBuf>,
    selected_file: Option<PathBuf>,
    notice: Option<String>,
    worker_error: Option<String>,
}

impl Default for ProjectTree {
    fn default() -> Self {
        Self::new()
    }
}

impl ProjectTree {
    pub fn new() -> Self {
        let (tx, requests) = mpsc::sync_channel::<Request>(1);
        // Only one request may be outstanding, so this queue is also bounded.
        let (results, rx) = mpsc::sync_channel::<Response>(1);
        let generation = Arc::new(AtomicU64::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_generation = Arc::clone(&generation);
        let worker_stopped = Arc::clone(&stopped);
        let worker = std::thread::Builder::new()
            .name("project-folder".into())
            .spawn(move || {
                while let Ok(request) = requests.recv() {
                    if worker_stopped.load(Ordering::Acquire) {
                        break;
                    }
                    let current = || {
                        !worker_stopped.load(Ordering::Acquire)
                            && !request.cancel.load(Ordering::Acquire)
                            && worker_generation.load(Ordering::Acquire) == request.generation
                    };
                    let outcome = match &request.job {
                        Job::List { path, limit } => {
                            Outcome::Listing(read_directory(&request.root, path, *limit, &current))
                        }
                        Job::Select { path } => {
                            Outcome::Selection(verify_selection(&request.root, path, &current))
                        }
                    };
                    let response = Response {
                        id: request.id,
                        generation: request.generation,
                        path: request.job.path().to_path_buf(),
                        outcome,
                    };
                    if results.send(response).is_err() {
                        break;
                    }
                    request.context.request_repaint();
                }
            });
        let (tx, worker_error) = match worker {
            Ok(_) => (Some(tx), None),
            Err(error) => (None, Some(format!("Cannot start folder browser: {error}"))),
        };
        Self {
            root: None,
            directories: BTreeMap::new(),
            cached_entries: 0,
            generation,
            stopped,
            next_request: 0,
            active: None,
            tx,
            rx,
            pending_selection: None,
            selected_file: None,
            notice: None,
            worker_error,
        }
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Replace only the browser's root and cache. Existing editor tabs are not
    /// owned by this component and must be left alone by the caller.
    pub fn set_root(&mut self, root: PathBuf) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        if let Some(active) = &self.active {
            active.cancel.store(true, Ordering::Release);
        }
        // Keep the old active slot until its response arrives: repeated root
        // changes must not spawn workers or build a queue of stale requests.
        self.directories.clear();
        self.directories.insert(root.clone(), Directory::queued());
        self.root = Some(root);
        self.cached_entries = 0;
        self.pending_selection = None;
        self.selected_file = None;
        self.notice = None;
    }

    pub fn poll(&mut self, context: &egui::Context) {
        while let Ok(response) = self.rx.try_recv() {
            self.apply_response(response);
        }
        self.dispatch(context);
        if self.active.is_some() {
            // A worker also requests a repaint on completion. This fallback
            // covers a response that arrives just before a frame goes idle.
            context.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// Render a virtualized tree and return a validated file click, if ready.
    /// No filesystem access happens on the UI thread.
    pub fn render(&mut self, ui: &mut egui::Ui) -> Option<PathBuf> {
        self.poll(ui.ctx());
        let Some(root) = self.root.clone() else {
            ui.weak("Open a folder to browse project files.");
            return None;
        };
        let mut action = None;
        ui.horizontal(|ui| {
            ui.strong("Project");
            if ui.small_button("Refresh").clicked() {
                action = Some(Action::Refresh);
            }
        });
        ui.label(root.to_string_lossy())
            .on_hover_text(root.display().to_string());
        if let Some(error) = &self.worker_error {
            ui.colored_label(ui.visuals().error_fg_color, error);
            return None;
        }
        if let Some(notice) = &self.notice {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(ui.visuals().error_fg_color, notice);
                if ui.small_button("Dismiss").clicked() {
                    action = Some(Action::Dismiss);
                }
            });
        }
        if self.pending_selection.is_some()
            || self.active.as_ref().is_some_and(|active| {
                active.generation == self.generation.load(Ordering::Acquire)
                    && !self.directories.contains_key(&active.path)
            })
        {
            ui.weak("Checking selected file…");
        }
        ui.separator();

        let mut rows = Vec::new();
        self.collect_rows(&root, 0, &mut rows);
        let current_generation = self.generation.load(Ordering::Acquire);
        egui::ScrollArea::vertical()
            .id_salt(("project-tree", current_generation))
            .auto_shrink([false, false])
            .show_rows(ui, ROW_HEIGHT, rows.len(), |ui, range| {
                for row in &rows[range] {
                    ui.horizontal(|ui| {
                        // Deep trees remain usable in a narrow sidebar.
                        ui.add_space((row.depth() as f32 * 14.0).min(112.0));
                        match row {
                            Row::Entry { entry, expanded, .. } => {
                                match &entry.kind {
                                    EntryKind::Directory => {
                                        let icon = if *expanded { "▾" } else { "▸" };
                                        let label = format!("{icon} {}", entry.display_name);
                                        if ui
                                            .selectable_label(false, label)
                                            .on_hover_text(entry.path.display().to_string())
                                            .clicked()
                                        {
                                            action = Some(Action::Toggle(entry.path.clone()));
                                        }
                                    }
                                    EntryKind::File => {
                                        if ui
                                            .selectable_label(false, &entry.display_name)
                                            .on_hover_text(entry.path.display().to_string())
                                            .clicked()
                                        {
                                            action = Some(Action::Select(entry.path.clone()));
                                        }
                                    }
                                    EntryKind::Link => {
                                        ui.add_enabled(
                                            false,
                                            egui::Label::new(format!("{} (link)", entry.display_name)),
                                        )
                                        .on_hover_text("Links and reparse points are not followed. Open the intended target explicitly.");
                                    }
                                    EntryKind::Other => {
                                        ui.add_enabled(
                                            false,
                                            egui::Label::new(format!("{} (special file)", entry.display_name)),
                                        )
                                        .on_hover_text("Only regular files can be opened.");
                                    }
                                    EntryKind::Unavailable(error) => {
                                        ui.add_enabled(false, egui::Label::new(&entry.display_name))
                                            .on_hover_text(error);
                                    }
                                }
                            }
                            Row::Footer { path, directory, .. } => {
                                match &directory.state {
                                    DirectoryState::Queued | DirectoryState::Loading(_) => {
                                        ui.spinner();
                                        ui.weak("Loading folder…");
                                    }
                                    DirectoryState::Failed(error) => {
                                        ui.colored_label(ui.visuals().error_fg_color, "Cannot read folder")
                                            .on_hover_text(error);
                                        if ui.small_button("Retry").on_hover_text(error).clicked() {
                                            action = Some(Action::Retry((*path).clone()));
                                        }
                                    }
                                    DirectoryState::Ready if directory.has_more => {
                                        if directory.entries.len() >= MAX_DIRECTORY_ENTRIES {
                                            ui.weak(format!("First {} entries shown; folder limit reached.", directory.entries.len()))
                                                .on_hover_text("This is a partial listing, not the whole folder. Open a subfolder as the project root to browse it directly.");
                                        } else if self.cached_entries >= MAX_CACHED_ENTRIES {
                                            ui.weak("Cache limit reached; more entries are hidden.")
                                                .on_hover_text("Collapse another folder to free space, then load more.");
                                        } else if ui
                                            .small_button(format!("Load more ({} shown)", directory.entries.len()))
                                            .on_hover_text("This listing is partial. Load the next bounded prefix of filesystem entries; directory order may change.")
                                            .clicked()
                                        {
                                            action = Some(Action::More((*path).clone()));
                                        }
                                    }
                                    DirectoryState::Ready => {
                                        if directory.entries.is_empty() {
                                            ui.weak("(empty folder)");
                                        }
                                    }
                                }
                            }
                        }
                    });
                }
            });

        if let Some(action) = action {
            self.perform(action);
            ui.ctx().request_repaint();
        }
        self.dispatch(ui.ctx());
        self.selected_file.take()
    }

    fn perform(&mut self, action: Action) {
        match action {
            Action::Refresh => {
                if let Some(root) = self.root.clone() {
                    self.set_root(root);
                }
            }
            Action::Dismiss => self.notice = None,
            Action::Toggle(path) => {
                if self.directories.contains_key(&path) {
                    self.collapse(&path);
                } else if self.directories.len() >= MAX_EXPANDED_DIRECTORIES {
                    self.notice = Some(
                        "Too many expanded folders. Collapse a folder before opening another."
                            .into(),
                    );
                } else if self.cached_entries >= MAX_CACHED_ENTRIES {
                    self.notice =
                        Some("Project cache is full. Collapse a folder to free space.".into());
                } else {
                    self.directories.insert(path, Directory::queued());
                }
            }
            Action::Retry(path) => {
                if let Some(directory) = self.directories.get_mut(&path) {
                    directory.state = DirectoryState::Queued;
                }
            }
            Action::More(path) => {
                if let Some(directory) = self.directories.get_mut(&path) {
                    directory.desired_limit =
                        (directory.entries.len() + PAGE_ENTRIES).min(MAX_DIRECTORY_ENTRIES);
                    directory.state = DirectoryState::Queued;
                }
            }
            Action::Select(path) => {
                // At most one queued click; the latest click wins.
                self.pending_selection = Some(path);
                self.selected_file = None;
                self.notice = None;
            }
        }
    }

    fn collapse(&mut self, path: &Path) {
        if self.root.as_deref() == Some(path) {
            return;
        }
        if let Some(active) = &self.active
            && active.path.starts_with(path)
        {
            active.cancel.store(true, Ordering::Release);
        }
        self.directories.retain(|candidate, directory| {
            if candidate.starts_with(path) {
                self.cached_entries -= directory.entries.len();
                false
            } else {
                true
            }
        });
    }

    fn collect_rows<'a>(&'a self, path: &'a PathBuf, depth: usize, rows: &mut Vec<Row<'a>>) {
        let Some(directory) = self.directories.get(path) else {
            return;
        };
        for entry in &directory.entries {
            let expanded = self.directories.contains_key(&entry.path);
            rows.push(Row::Entry {
                entry,
                depth,
                expanded,
            });
            if expanded {
                self.collect_rows(&entry.path, depth + 1, rows);
            }
        }
        if !matches!(directory.state, DirectoryState::Ready)
            || directory.has_more
            || directory.entries.is_empty()
        {
            rows.push(Row::Footer {
                path,
                directory,
                depth,
            });
        }
    }

    fn dispatch(&mut self, context: &egui::Context) {
        if self.active.is_some() || self.tx.is_none() {
            return;
        }
        let Some(root) = self.root.clone() else {
            return;
        };
        let job = if let Some(path) = self.pending_selection.take() {
            Job::Select { path }
        } else if let Some((path, directory)) = self
            .directories
            .iter()
            .find(|(_, directory)| matches!(directory.state, DirectoryState::Queued))
        {
            let available = MAX_CACHED_ENTRIES - self.cached_entries + directory.entries.len();
            let limit = directory
                .desired_limit
                .min(available)
                .min(MAX_DIRECTORY_ENTRIES);
            if limit == 0 {
                let directory = self
                    .directories
                    .get_mut(&path.clone())
                    .expect("existing folder");
                directory.state = DirectoryState::Failed(
                    "Project cache is full. Collapse a folder to free space, then Retry.".into(),
                );
                return;
            }
            Job::List {
                path: path.clone(),
                limit,
            }
        } else {
            return;
        };
        self.next_request = self.next_request.wrapping_add(1);
        let id = self.next_request;
        let generation = self.generation.load(Ordering::Acquire);
        let path = job.path().to_path_buf();
        let cancel = Arc::new(AtomicBool::new(false));
        let request = Request {
            id,
            generation,
            root,
            job,
            cancel: Arc::clone(&cancel),
            context: context.clone(),
        };
        match self.tx.as_ref().expect("checked worker").try_send(request) {
            Ok(()) => {
                if let Some(directory) = self.directories.get_mut(&path) {
                    directory.state = DirectoryState::Loading(id);
                }
                self.active = Some(Active {
                    id,
                    generation,
                    path,
                    cancel,
                });
            }
            Err(TrySendError::Full(request)) => {
                if let Job::Select { path } = request.job {
                    self.pending_selection = Some(path);
                }
                context.request_repaint();
            }
            Err(TrySendError::Disconnected(_)) => {
                self.tx = None;
                self.worker_error = Some(
                    "The folder browser worker stopped. Reopen the application to retry.".into(),
                );
            }
        }
    }

    fn apply_response(&mut self, response: Response) {
        let Some(active) = &self.active else {
            return;
        };
        if active.id != response.id
            || active.generation != response.generation
            || active.path != response.path
        {
            return;
        }
        let cancelled = active.cancel.load(Ordering::Acquire);
        self.active = None;
        if cancelled || response.generation != self.generation.load(Ordering::Acquire) {
            return;
        }
        match response.outcome {
            Outcome::Listing(result) => {
                let Some(directory) = self.directories.get_mut(&response.path) else {
                    return;
                };
                if !matches!(directory.state, DirectoryState::Loading(id) if id == response.id) {
                    return;
                }
                match result {
                    Ok(mut listing) => {
                        let available =
                            MAX_CACHED_ENTRIES - self.cached_entries + directory.entries.len();
                        let limit = available.min(MAX_DIRECTORY_ENTRIES);
                        if listing.entries.len() > limit {
                            listing.entries.truncate(limit);
                            listing.has_more = true;
                        }
                        self.cached_entries =
                            self.cached_entries - directory.entries.len() + listing.entries.len();
                        directory.entries = listing.entries;
                        directory.has_more = listing.has_more;
                        directory.state = DirectoryState::Ready;
                    }
                    Err(error) => directory.state = DirectoryState::Failed(error),
                }
            }
            Outcome::Selection(result) => {
                // A newer click supersedes an in-flight selection.
                if self.pending_selection.is_some() {
                    return;
                }
                match result {
                    Ok(path) => self.selected_file = Some(path),
                    Err(error) => self.notice = Some(error),
                }
            }
        }
    }
}

impl Drop for ProjectTree {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        if let Some(active) = &self.active {
            active.cancel.store(true, Ordering::Release);
        }
        // Never join a potentially blocked filesystem call on the UI thread.
        // Dropping the channels wakes an idle worker; a busy worker exits at its
        // next cancellation check or when delivering its bounded result.
        self.tx.take();
    }
}

enum Action {
    Refresh,
    Dismiss,
    Toggle(PathBuf),
    Retry(PathBuf),
    More(PathBuf),
    Select(PathBuf),
}

enum Row<'a> {
    Entry {
        entry: &'a Entry,
        depth: usize,
        expanded: bool,
    },
    Footer {
        path: &'a PathBuf,
        directory: &'a Directory,
        depth: usize,
    },
}

impl Row<'_> {
    fn depth(&self) -> usize {
        match self {
            Self::Entry { depth, .. } | Self::Footer { depth, .. } => *depth,
        }
    }
}

fn check_current(current: &impl Fn() -> bool) -> Result<(), String> {
    if current() {
        Ok(())
    } else {
        Err("Folder operation cancelled.".into())
    }
}

fn is_link_or_reparse(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

/// Validate only the root and the path below it. Ancestors above an explicitly
/// chosen root can be OS aliases (for example /var on macOS).
fn checked_directory(root: &Path, path: &Path, current: &impl Fn() -> bool) -> Result<(), String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "Folder is outside the project root.".to_string())?;
    let mut candidate = root.to_path_buf();
    check_directory_component(&candidate, current)?;
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err("Invalid project folder path.".into());
        }
        candidate.push(component);
        check_directory_component(&candidate, current)?;
    }
    Ok(())
}

fn check_directory_component(path: &Path, current: &impl Fn() -> bool) -> Result<(), String> {
    check_current(current)?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Cannot inspect {}: {error}", path.display()))?;
    if is_link_or_reparse(&metadata) {
        return Err(format!(
            "{} is a link or reparse point and will not be followed.",
            path.display()
        ));
    }
    if !metadata.is_dir() {
        return Err(format!(
            "{} is no longer a directory. Refresh the project.",
            path.display()
        ));
    }
    Ok(())
}

fn read_directory(
    root: &Path,
    path: &Path,
    requested_limit: usize,
    current: &impl Fn() -> bool,
) -> Result<Listing, String> {
    checked_directory(root, path, current)?;
    check_current(current)?;
    let directory =
        fs::read_dir(path).map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
    let limit = requested_limit.min(MAX_DIRECTORY_ENTRIES);
    let mut entries = Vec::with_capacity(limit);
    let mut has_more = false;
    for item in directory {
        check_current(current)?;
        let item =
            item.map_err(|error| format!("Cannot finish reading {}: {error}", path.display()))?;
        if entries.len() >= limit {
            has_more = true;
            break;
        }
        let path = item.path();
        let kind = match fs::symlink_metadata(&path) {
            Ok(metadata) if is_link_or_reparse(&metadata) => EntryKind::Link,
            Ok(metadata) if metadata.is_dir() => EntryKind::Directory,
            Ok(metadata) if metadata.is_file() => EntryKind::File,
            Ok(_) => EntryKind::Other,
            Err(error) => {
                EntryKind::Unavailable(format!("Cannot inspect {}: {error}", path.display()))
            }
        };
        entries.push(Entry {
            display_name: item.file_name().to_string_lossy().into_owned(),
            path,
            kind,
        });
    }
    check_current(current)?;
    // Sort only the bounded prefix. Never imply that a partial listing contains
    // the alphabetically first entries in the entire directory.
    entries.sort_by(|a, b| {
        let a_folder = matches!(a.kind, EntryKind::Directory);
        let b_folder = matches!(b.kind, EntryKind::Directory);
        b_folder
            .cmp(&a_folder)
            .then_with(|| a.path.file_name().cmp(&b.path.file_name()))
    });
    Ok(Listing { entries, has_more })
}

fn verify_selection(
    root: &Path,
    path: &Path,
    current: &impl Fn() -> bool,
) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "File has no parent folder.".to_string())?;
    checked_directory(root, parent, current)?;
    check_current(current)?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Cannot open {}: {error}", path.display()))?;
    if is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(format!(
            "{} is not a regular, non-link file. Refresh the project.",
            path.display()
        ));
    }
    check_current(current)?;
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn entry(path: PathBuf) -> Entry {
        Entry {
            display_name: path.file_name().unwrap().to_string_lossy().into_owned(),
            path,
            kind: EntryKind::File,
        }
    }

    fn active(tree: &mut ProjectTree, path: &Path, id: u64) -> u64 {
        let generation = tree.generation.load(Ordering::Acquire);
        tree.active = Some(Active {
            id,
            generation,
            path: path.to_path_buf(),
            cancel: Arc::new(AtomicBool::new(false)),
        });
        if let Some(directory) = tree.directories.get_mut(path) {
            directory.state = DirectoryState::Loading(id);
        }
        generation
    }

    #[test]
    fn listing_is_bounded_and_load_more_is_honest() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..PAGE_ENTRIES + 17 {
            fs::write(temp.path().join(format!("file-{index:04}")), b"").unwrap();
        }
        let first = read_directory(temp.path(), temp.path(), PAGE_ENTRIES, &|| true).unwrap();
        assert_eq!(first.entries.len(), PAGE_ENTRIES);
        assert!(first.has_more);
        let next = read_directory(temp.path(), temp.path(), PAGE_ENTRIES * 2, &|| true).unwrap();
        assert_eq!(next.entries.len(), PAGE_ENTRIES + 17);
        assert!(!next.has_more);
        let zero = read_directory(temp.path(), temp.path(), 0, &|| true).unwrap();
        assert!(zero.entries.is_empty());
        assert!(zero.has_more);
    }

    #[test]
    fn listing_does_not_read_descendants_and_sorts_folders_first() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("z-folder")).unwrap();
        fs::write(temp.path().join("z-folder/child.txt"), b"child").unwrap();
        fs::write(temp.path().join("a-file"), b"file").unwrap();
        let listing = read_directory(temp.path(), temp.path(), PAGE_ENTRIES, &|| true).unwrap();
        assert_eq!(listing.entries.len(), 2);
        assert_eq!(listing.entries[0].kind, EntryKind::Directory);
        assert!(
            !listing
                .entries
                .iter()
                .any(|entry| entry.path.ends_with("child.txt"))
        );
    }

    #[test]
    fn real_listing_never_exceeds_hard_per_directory_limit() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..MAX_DIRECTORY_ENTRIES + 3 {
            fs::write(temp.path().join(format!("{index}")), b"").unwrap();
        }
        let checks = AtomicUsize::new(0);
        let listing = read_directory(temp.path(), temp.path(), usize::MAX, &|| {
            checks.fetch_add(1, Ordering::AcqRel);
            true
        })
        .unwrap();
        assert_eq!(listing.entries.len(), MAX_DIRECTORY_ENTRIES);
        assert!(listing.has_more);
        // Root validation, pre-read check, one per enumerated item including
        // the lookahead item, and a final check. No whole-directory traversal.
        assert_eq!(checks.load(Ordering::Acquire), MAX_DIRECTORY_ENTRIES + 4);
    }

    #[test]
    fn cancelled_work_stops_before_and_during_listing() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..50 {
            fs::write(temp.path().join(format!("{index}")), b"").unwrap();
        }
        assert!(read_directory(temp.path(), temp.path(), PAGE_ENTRIES, &|| false).is_err());
        let checks = AtomicUsize::new(0);
        let result = read_directory(temp.path(), temp.path(), PAGE_ENTRIES, &|| {
            checks.fetch_add(1, Ordering::AcqRel) < 7
        });
        assert!(result.unwrap_err().contains("cancelled"));
        assert_eq!(checks.load(Ordering::Acquire), 8);
    }

    #[test]
    fn changing_root_cancels_and_rejects_stale_results() {
        let mut tree = ProjectTree::new();
        let old = PathBuf::from("/old");
        let new = PathBuf::from("/new");
        tree.set_root(old.clone());
        let generation = active(&mut tree, &old, 10);
        let cancel = Arc::clone(&tree.active.as_ref().unwrap().cancel);
        tree.set_root(new.clone());
        assert!(cancel.load(Ordering::Acquire));
        assert_eq!(tree.root(), Some(new.as_path()));
        tree.apply_response(Response {
            id: 10,
            generation,
            path: old.clone(),
            outcome: Outcome::Listing(Ok(Listing {
                entries: vec![entry(old.join("stale"))],
                has_more: false,
            })),
        });
        assert!(tree.active.is_none());
        assert_eq!(tree.cached_entries, 0);
        assert!(!tree.directories.contains_key(&old));
        assert!(tree.directories[&new].entries.is_empty());
        assert!(tree.selected_file.is_none());
    }

    #[test]
    fn request_id_mismatch_does_not_release_or_replace_current_job() {
        let mut tree = ProjectTree::new();
        let root = PathBuf::from("/project");
        tree.set_root(root.clone());
        let generation = active(&mut tree, &root, 12);
        tree.apply_response(Response {
            id: 11,
            generation,
            path: root.clone(),
            outcome: Outcome::Listing(Ok(Listing {
                entries: vec![entry(root.join("wrong"))],
                has_more: false,
            })),
        });
        assert_eq!(tree.active.as_ref().unwrap().id, 12);
        assert_eq!(tree.cached_entries, 0);
    }

    #[test]
    fn result_application_enforces_per_directory_and_total_cache_caps() {
        let mut tree = ProjectTree::new();
        let root = PathBuf::from("/project");
        tree.set_root(root.clone());
        for folder in 0..6 {
            let path = root.join(format!("dir-{folder}"));
            tree.directories.insert(path.clone(), Directory::queued());
            let generation = active(&mut tree, &path, folder + 1);
            tree.apply_response(Response {
                id: folder + 1,
                generation,
                path: path.clone(),
                outcome: Outcome::Listing(Ok(Listing {
                    entries: (0..MAX_DIRECTORY_ENTRIES + 10)
                        .map(|index| entry(path.join(format!("{index}"))))
                        .collect(),
                    has_more: false,
                })),
            });
            assert!(tree.directories[&path].entries.len() <= MAX_DIRECTORY_ENTRIES);
            assert!(tree.directories[&path].has_more);
            assert!(tree.cached_entries <= MAX_CACHED_ENTRIES);
        }
        assert_eq!(tree.cached_entries, MAX_CACHED_ENTRIES);
        tree.collapse(&root.join("dir-0"));
        assert_eq!(
            tree.cached_entries,
            MAX_CACHED_ENTRIES - MAX_DIRECTORY_ENTRIES
        );
    }

    #[test]
    fn permission_error_is_visible_and_retryable() {
        let mut tree = ProjectTree::new();
        let root = PathBuf::from("/project");
        tree.set_root(root.clone());
        let generation = active(&mut tree, &root, 1);
        tree.apply_response(Response {
            id: 1,
            generation,
            path: root.clone(),
            outcome: Outcome::Listing(Err("Permission denied".into())),
        });
        assert!(
            matches!(&tree.directories[&root].state, DirectoryState::Failed(error) if error.contains("Permission"))
        );
        tree.perform(Action::Retry(root.clone()));
        assert!(matches!(
            tree.directories[&root].state,
            DirectoryState::Queued
        ));
        let generation = active(&mut tree, &root, 2);
        tree.apply_response(Response {
            id: 2,
            generation,
            path: root.clone(),
            outcome: Outcome::Listing(Ok(Listing {
                entries: vec![entry(root.join("recovered"))],
                has_more: false,
            })),
        });
        assert!(matches!(
            tree.directories[&root].state,
            DirectoryState::Ready
        ));
        assert_eq!(tree.directories[&root].entries.len(), 1);
    }

    #[test]
    fn real_read_error_can_recover_after_directory_appears() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("later");
        assert!(read_directory(temp.path(), &missing, PAGE_ENTRIES, &|| true).is_err());
        fs::create_dir(&missing).unwrap();
        assert!(read_directory(temp.path(), &missing, PAGE_ENTRIES, &|| true).is_ok());
    }

    #[test]
    fn collapsed_folder_does_not_reappear_when_result_arrives() {
        let mut tree = ProjectTree::new();
        let root = PathBuf::from("/project");
        let child = root.join("child");
        tree.set_root(root);
        tree.perform(Action::Toggle(child.clone()));
        let generation = active(&mut tree, &child, 1);
        tree.collapse(&child);
        tree.apply_response(Response {
            id: 1,
            generation,
            path: child.clone(),
            outcome: Outcome::Listing(Ok(Listing {
                entries: vec![entry(child.join("file"))],
                has_more: false,
            })),
        });
        assert!(!tree.directories.contains_key(&child));
        assert_eq!(tree.cached_entries, 0);
    }

    #[test]
    fn expanded_directory_count_is_bounded() {
        let mut tree = ProjectTree::new();
        let root = PathBuf::from("/project");
        tree.set_root(root.clone());
        for index in 0..MAX_EXPANDED_DIRECTORIES + 20 {
            tree.perform(Action::Toggle(root.join(format!("{index}"))));
        }
        assert_eq!(tree.directories.len(), MAX_EXPANDED_DIRECTORIES);
        assert!(tree.notice.as_deref().unwrap().contains("Too many"));
    }

    #[test]
    fn selection_requires_regular_file_and_stays_inside_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir(&root).unwrap();
        let file = root.join("file.txt");
        fs::write(&file, b"text").unwrap();
        assert_eq!(verify_selection(&root, &file, &|| true).unwrap(), file);
        assert!(verify_selection(&root, &root, &|| true).is_err());
        let outside = temp.path().join("outside.txt");
        fs::write(&outside, b"outside").unwrap();
        assert!(verify_selection(&root, &outside, &|| true).is_err());
        assert!(verify_selection(&root, &root.join("../outside.txt"), &|| true).is_err());
        assert!(verify_selection(&root, &file, &|| false).is_err());
    }

    #[test]
    fn new_click_and_new_root_discard_old_selection() {
        let mut tree = ProjectTree::new();
        let root = PathBuf::from("/project");
        let first = root.join("first");
        let second = root.join("second");
        tree.set_root(root.clone());
        let generation = active(&mut tree, &first, 1);
        tree.perform(Action::Select(second.clone()));
        tree.apply_response(Response {
            id: 1,
            generation,
            path: first.clone(),
            outcome: Outcome::Selection(Ok(first)),
        });
        assert!(tree.selected_file.is_none());
        assert_eq!(tree.pending_selection, Some(second.clone()));
        let generation = active(&mut tree, &second, 2);
        tree.set_root(PathBuf::from("/other"));
        tree.apply_response(Response {
            id: 2,
            generation,
            path: second.clone(),
            outcome: Outcome::Selection(Ok(second)),
        });
        assert!(tree.selected_file.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn links_are_disabled_and_parent_replacement_cannot_bypass_checks() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("target"), b"outside").unwrap();
        symlink(&outside, root.join("linked-folder")).unwrap();
        symlink(outside.join("target"), root.join("linked-file")).unwrap();
        symlink(outside.join("missing"), root.join("broken")).unwrap();
        let listing = read_directory(&root, &root, PAGE_ENTRIES, &|| true).unwrap();
        assert_eq!(listing.entries.len(), 3);
        assert!(
            listing
                .entries
                .iter()
                .all(|entry| entry.kind == EntryKind::Link)
        );
        assert!(
            read_directory(&root, &root.join("linked-folder"), PAGE_ENTRIES, &|| true).is_err()
        );
        assert!(verify_selection(&root, &root.join("linked-file"), &|| true).is_err());
        assert!(verify_selection(&root, &root.join("linked-folder/target"), &|| true).is_err());
        let alias = temp.path().join("root-alias");
        symlink(&root, &alias).unwrap();
        assert!(read_directory(&alias, &alias, PAGE_ENTRIES, &|| true).is_err());

        let child = root.join("child");
        fs::create_dir(&child).unwrap();
        fs::write(child.join("target"), b"inside").unwrap();
        assert!(verify_selection(&root, &child.join("target"), &|| true).is_ok());
        fs::remove_file(child.join("target")).unwrap();
        fs::remove_dir(&child).unwrap();
        symlink(&outside, &child).unwrap();
        assert!(verify_selection(&root, &child.join("target"), &|| true).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn lossy_display_names_do_not_change_raw_path_identity() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join(OsString::from_vec(vec![b'a', 0x80]));
        let b = temp.path().join(OsString::from_vec(vec![b'a', 0x81]));
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        let listing = read_directory(temp.path(), temp.path(), PAGE_ENTRIES, &|| true).unwrap();
        assert_eq!(
            listing.entries[0].display_name,
            listing.entries[1].display_name
        );
        assert_ne!(listing.entries[0].path, listing.entries[1].path);
        assert_eq!(verify_selection(temp.path(), &a, &|| true).unwrap(), a);
        assert_eq!(verify_selection(temp.path(), &b, &|| true).unwrap(), b);
    }

    #[test]
    fn worker_delivers_selection_once_through_render() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("selected.txt");
        fs::write(&file, b"hello").unwrap();
        let mut tree = ProjectTree::new();
        tree.set_root(temp.path().to_path_buf());
        tree.perform(Action::Select(file.clone()));
        let context = egui::Context::default();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut selected = None;
        while selected.is_none() && std::time::Instant::now() < deadline {
            tree.poll(&context);
            context
                .run_ui(egui::RawInput::default(), |ui| {
                    selected = tree.render(ui);
                })
                .drop_without_applying_deltas();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(selected, Some(file));
        context
            .run_ui(egui::RawInput::default(), |ui| {
                assert!(tree.render(ui).is_none());
            })
            .drop_without_applying_deltas();
    }

    #[test]
    fn full_cache_renders_only_a_viewport_of_rows_in_both_themes() {
        let mut tree = ProjectTree::new();
        let root = PathBuf::from("/project");
        tree.set_root(root.clone());
        let root_node = tree.directories.get_mut(&root).unwrap();
        root_node.entries = (0..MAX_DIRECTORY_ENTRIES)
            .map(|index| Entry {
                path: root.join(format!("dir-{index}")),
                display_name: format!("dir-{index}"),
                kind: EntryKind::Directory,
            })
            .collect();
        root_node.state = DirectoryState::Ready;
        for index in 0..3 {
            let path = root.join(format!("dir-{index}"));
            tree.directories.insert(
                path.clone(),
                Directory {
                    entries: (0..MAX_DIRECTORY_ENTRIES)
                        .map(|index| entry(path.join(format!("file-{index}"))))
                        .collect(),
                    has_more: false,
                    desired_limit: MAX_DIRECTORY_ENTRIES,
                    state: DirectoryState::Ready,
                },
            );
        }
        tree.cached_entries = MAX_CACHED_ENTRIES;
        let context = egui::Context::default();
        for visuals in [egui::Visuals::dark(), egui::Visuals::light()] {
            context.set_visuals(visuals);
            let output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(260.0, 480.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    assert!(tree.render(ui).is_none());
                },
            );
            // A non-virtualized tree would paint thousands of labels.
            assert!(output.shapes.len() < 150, "painted {}", output.shapes.len());
            output.drop_without_applying_deltas();
        }
    }
}
