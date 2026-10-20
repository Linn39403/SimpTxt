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
const DISCLOSURE_WIDTH: f32 = 14.0;
const ICON_WIDTH: f32 = 24.0;
const NAME_GAP: f32 = 6.0;

/// Deliberately use ordinary ASCII badges rather than a platform icon font.
/// Classification affects decoration only: the original path still identifies
/// every file, including filenames that cannot be represented as UTF-8.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileIconKind {
    C,
    Cpp,
    Header,
    Rust,
    Python,
    Json,
    Toml,
    CMake,
    Generic,
}

impl FileIconKind {
    fn for_path(path: &Path) -> Self {
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("CMakeLists.txt"))
        {
            return Self::CMake;
        }
        let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
            return Self::Generic;
        };
        // Uppercase .C is conventionally C++, unlike lowercase .c.
        if extension == "C" {
            return Self::Cpp;
        }
        match extension.to_ascii_lowercase().as_str() {
            "c" => Self::C,
            "cc" | "cpp" | "cxx" | "c++" => Self::Cpp,
            "h" | "hh" | "hpp" | "hxx" | "h++" => Self::Header,
            "rs" => Self::Rust,
            "py" | "pyi" | "pyw" => Self::Python,
            "json" | "jsonc" => Self::Json,
            "toml" => Self::Toml,
            "cmake" => Self::CMake,
            _ => Self::Generic,
        }
    }

    fn badge(self) -> &'static str {
        match self {
            Self::C => "C",
            Self::Cpp => "C++",
            Self::Header => "H",
            Self::Rust => "RS",
            Self::Python => "PY",
            Self::Json => "{}",
            Self::Toml => "TM",
            Self::CMake => "CM",
            Self::Generic => "",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::C => "C source",
            Self::Cpp => "C++ source",
            Self::Header => "C/C++ header",
            Self::Rust => "Rust source",
            Self::Python => "Python source",
            Self::Json => "JSON file",
            Self::Toml => "TOML configuration",
            Self::CMake => "CMake configuration",
            Self::Generic => "File",
        }
    }

    fn color(self, visuals: &egui::Visuals) -> egui::Color32 {
        let (dark, light) = match self {
            Self::C | Self::Cpp => ([133, 181, 247], [54, 107, 184]),
            Self::Header => ([112, 205, 205], [24, 116, 122]),
            Self::Rust => ([226, 162, 127], [168, 80, 47]),
            Self::Python => ([230, 199, 103], [143, 106, 18]),
            Self::Json => ([155, 203, 139], [65, 111, 52]),
            Self::Toml => ([193, 157, 223], [129, 79, 155]),
            Self::CMake => ([132, 189, 189], [45, 113, 113]),
            Self::Generic => return visuals.text_color(),
        };
        let [red, green, blue] = if visuals.dark_mode { dark } else { light };
        egui::Color32::from_rgb(red, green, blue)
    }
}

fn row_indent(depth: usize) -> f32 {
    // Preserve space for the icon and name in a narrow sidebar.
    (depth as f32 * 14.0).min(112.0)
}

fn root_label(root: &Path) -> String {
    root.file_name()
        .filter(|name| !name.is_empty())
        .unwrap_or(root.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn paint_disclosure(
    painter: &egui::Painter,
    center: egui::Pos2,
    expanded: bool,
    color: egui::Color32,
) {
    let points = if expanded {
        [(-4.0, -2.5), (4.0, -2.5), (0.0, 2.5)]
    } else {
        [(-2.5, -4.0), (2.5, 0.0), (-2.5, 4.0)]
    };
    painter.add(egui::Shape::convex_polygon(
        points
            .into_iter()
            .map(|(x, y)| center + egui::vec2(x, y))
            .collect(),
        color,
        egui::Stroke::NONE,
    ));
}

fn paint_folder(painter: &egui::Painter, rect: egui::Rect, expanded: bool, dark: bool) {
    let color = if dark {
        egui::Color32::from_rgb(224, 178, 86)
    } else {
        egui::Color32::from_rgb(147, 95, 25)
    };
    let origin = rect.center() - egui::vec2(9.0, 8.0);
    let stroke = egui::Stroke::new(1.25, color);
    // The tab and body are independent vector rectangles, so the folder never
    // depends on a missing Unicode glyph or unsupported concave filled path.
    painter.rect(
        egui::Rect::from_min_size(origin, egui::vec2(8.0, 6.0)),
        1.5,
        color.gamma_multiply(0.22),
        stroke,
        egui::StrokeKind::Inside,
    );
    painter.rect(
        egui::Rect::from_min_size(origin + egui::vec2(0.0, 3.0), egui::vec2(18.0, 13.0)),
        1.5,
        color.gamma_multiply(0.22),
        stroke,
        egui::StrokeKind::Inside,
    );
    if expanded {
        // A visibly tilted front flap distinguishes the open folder silhouette.
        painter.add(egui::Shape::convex_polygon(
            [(1.0, 7.0), (21.0, 7.0), (17.0, 16.0), (-2.0, 16.0)]
                .into_iter()
                .map(|(x, y)| origin + egui::vec2(x, y))
                .collect(),
            color.gamma_multiply(0.35),
            stroke,
        ));
    }
}

fn paint_file(painter: &egui::Painter, rect: egui::Rect, kind: FileIconKind, color: egui::Color32) {
    let origin = rect.center() - egui::vec2(9.0, 10.0);
    let stroke = egui::Stroke::new(1.0, color);
    painter.add(egui::Shape::convex_polygon(
        [
            (1.0, 0.0),
            (11.0, 0.0),
            (17.0, 6.0),
            (17.0, 20.0),
            (1.0, 20.0),
        ]
        .into_iter()
        .map(|(x, y)| origin + egui::vec2(x, y))
        .collect(),
        color.gamma_multiply(0.06),
        stroke,
    ));
    painter.line(
        [(11.0, 0.0), (11.0, 6.0), (17.0, 6.0)]
            .into_iter()
            .map(|(x, y)| origin + egui::vec2(x, y))
            .collect(),
        stroke,
    );
    if kind == FileIconKind::Generic {
        for y in [10.0, 14.0] {
            painter.line_segment(
                [origin + egui::vec2(5.0, y), origin + egui::vec2(13.0, y)],
                stroke,
            );
        }
    } else {
        painter.text(
            origin + egui::vec2(9.0, 13.5),
            egui::Align2::CENTER_CENTER,
            kind.badge(),
            egui::FontId::monospace(if kind == FileIconKind::Cpp { 7.5 } else { 9.0 }),
            color,
        );
    }
}

fn render_entry_row(
    ui: &mut egui::Ui,
    entry: &Entry,
    expanded: bool,
    depth: usize,
) -> egui::Response {
    let enabled = matches!(entry.kind, EntryKind::Directory | EntryKind::File);
    let is_folder = entry.kind == EntryKind::Directory;
    let kind = FileIconKind::for_path(&entry.path);
    let description = match &entry.kind {
        EntryKind::Directory if expanded => "Collapse folder",
        EntryKind::Directory => "Expand folder",
        EntryKind::File => kind.description(),
        EntryKind::Link => "Link (not followed)",
        EntryKind::Other => "Special file (cannot open)",
        EntryKind::Unavailable(_) => "Unavailable file",
    };
    let tooltip = match &entry.kind {
        EntryKind::Link => format!(
            "{}\nLinks and reparse points are not followed. Open the intended target explicitly.",
            entry.path.display()
        ),
        EntryKind::Other => format!(
            "{}\nOnly regular files can be opened.",
            entry.path.display()
        ),
        EntryKind::Unavailable(error) => format!("{}\n{error}", entry.path.display()),
        _ => format!("{description}\n{}", entry.path.display()),
    };
    ui.push_id(&entry.path, |ui| {
        ui.add_enabled_ui(enabled, |ui| {
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), ROW_HEIGHT),
                egui::Sense::click(),
            );
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    if is_folder {
                        egui::WidgetType::CollapsingHeader
                    } else {
                        egui::WidgetType::Button
                    },
                    ui.is_enabled(),
                    format!("{description}: {}", entry.display_name),
                )
            });
            if ui.is_rect_visible(rect) {
                let visuals = ui.style().interact(&response);
                if response.hovered()
                    || response.has_focus()
                    || response.is_pointer_button_down_on()
                {
                    ui.painter().rect(
                        rect,
                        3.0,
                        visuals.weak_bg_fill,
                        visuals.bg_stroke,
                        egui::StrokeKind::Inside,
                    );
                }
                let left = rect.left() + row_indent(depth);
                let icon_rect = egui::Rect::from_min_size(
                    egui::pos2(left + DISCLOSURE_WIDTH, rect.top()),
                    egui::vec2(ICON_WIDTH, ROW_HEIGHT),
                );
                if is_folder {
                    paint_disclosure(
                        ui.painter(),
                        egui::pos2(left + DISCLOSURE_WIDTH / 2.0, rect.center().y),
                        expanded,
                        visuals.fg_stroke.color,
                    );
                    paint_folder(ui.painter(), icon_rect, expanded, ui.visuals().dark_mode);
                } else {
                    paint_file(ui.painter(), icon_rect, kind, kind.color(ui.visuals()));
                }
                let text_left = icon_rect.right() + NAME_GAP;
                let mut job = egui::text::LayoutJob::simple_singleline(
                    entry.display_name.clone(),
                    egui::TextStyle::Body.resolve(ui.style()),
                    visuals.text_color(),
                );
                job.wrap = egui::text::TextWrapping::truncate_at_width(
                    (rect.right() - text_left - 4.0).max(0.0),
                );
                let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
                let text_top = rect.center().y - galley.size().y / 2.0;
                ui.painter().galley(
                    egui::pos2(text_left, text_top),
                    galley,
                    visuals.text_color(),
                );
            }
            response.on_hover_text(tooltip)
        })
        .inner
    })
    .inner
}

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

    /// Headless captures must wait for the same lazy listings as the desktop
    /// UI. A failed listing is settled: its visible error must remain capturable.
    pub(crate) fn is_settled(&self) -> bool {
        self.active.is_none()
            && self.pending_selection.is_none()
            && self.directories.values().all(|directory| {
                !matches!(
                    directory.state,
                    DirectoryState::Queued | DirectoryState::Loading(_)
                )
            })
    }

    /// Queue only the parents of a known file for deterministic headless
    /// captures. Listing still happens on the normal bounded worker; this does
    /// not recursively scan, select the file, or expand any sibling folders.
    pub(crate) fn reveal_file(&mut self, path: &Path) {
        let Some(root) = self.root.as_ref() else {
            return;
        };
        let Some(relative_parent) = path
            .parent()
            .and_then(|parent| parent.strip_prefix(root).ok())
        else {
            return;
        };
        if self.cached_entries >= MAX_CACHED_ENTRIES {
            return;
        }
        let mut directory_path = root.clone();
        let mut missing = Vec::new();
        for component in relative_parent.components() {
            let std::path::Component::Normal(name) = component else {
                // No lexical parent traversal, even if a later component
                // would happen to return inside the chosen root.
                return;
            };
            directory_path.push(name);
            if !self.directories.contains_key(&directory_path) {
                if self.directories.len() + missing.len() >= MAX_EXPANDED_DIRECTORIES {
                    return;
                }
                missing.push(directory_path.clone());
            }
        }
        for parent in missing {
            self.directories.insert(parent, Directory::queued());
        }
    }

    /// Replace only the browser's root and cache. Existing editor tabs are not
    /// owned by this component and must be left alone by the caller.
    pub fn set_root(&mut self, root: PathBuf) {
        self.clear_root();
        self.directories.insert(root.clone(), Directory::queued());
        self.root = Some(root);
    }

    /// Close the browser without changing any file or editor tab. Invalidate
    /// queued and completed selections as well as any listing still in flight.
    pub fn clear_root(&mut self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        if let Some(active) = &self.active {
            active.cancel.store(true, Ordering::Release);
        }
        // Keep the old active slot until its response arrives: repeated root
        // changes or close/reopen must not build a queue of stale requests.
        self.directories.clear();
        self.root = None;
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
        ui.add(
            egui::Label::new(egui::RichText::new(root_label(&root)).strong())
                .truncate()
                .show_tooltip_when_elided(false),
        )
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
                    match row {
                        Row::Entry { entry, expanded, depth } => {
                            if render_entry_row(ui, entry, *expanded, *depth).clicked() {
                                action = match entry.kind {
                                    EntryKind::Directory => Some(Action::Toggle(entry.path.clone())),
                                    EntryKind::File => Some(Action::Select(entry.path.clone())),
                                    _ => None,
                                };
                            }
                        }
                        Row::Footer { path, directory, .. } => {
                            ui.horizontal(|ui| {
                                ui.add_space(
                                    row_indent(row.depth())
                                        + DISCLOSURE_WIDTH
                                        + ICON_WIDTH
                                        + NAME_GAP,
                                );
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
                            });
                        }
                    }
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
    fn closing_root_cancels_listing_and_clears_all_browser_state() {
        let mut tree = ProjectTree::new();
        let root = PathBuf::from("/project");
        let child = root.join("child");
        let file = root.join("selected.txt");
        tree.set_root(root.clone());
        tree.directories
            .insert(child, ready_directory(vec![entry(file.clone())]));
        tree.cached_entries = 1;
        tree.pending_selection = Some(file.clone());
        tree.selected_file = Some(file);
        tree.notice = Some("Old folder notice".into());
        let generation = active(&mut tree, &root, 11);
        let cancel = Arc::clone(&tree.active.as_ref().unwrap().cancel);

        tree.clear_root();

        assert!(tree.root().is_none());
        assert!(tree.directories.is_empty());
        assert_eq!(tree.cached_entries, 0);
        assert!(tree.pending_selection.is_none());
        assert!(tree.selected_file.is_none());
        assert!(tree.notice.is_none());
        assert!(cancel.load(Ordering::Acquire));
        assert_ne!(tree.generation.load(Ordering::Acquire), generation);
        // Keep the slot occupied until the real worker's acknowledgment arrives.
        assert_eq!(tree.active.as_ref().unwrap().id, 11);

        tree.apply_response(Response {
            id: 11,
            generation,
            path: root.clone(),
            outcome: Outcome::Listing(Ok(Listing {
                entries: vec![entry(root.join("late.txt"))],
                has_more: false,
            })),
        });
        assert!(tree.is_settled());
        assert!(tree.root().is_none());
        assert!(tree.directories.is_empty());
        assert_eq!(tree.cached_entries, 0);
        assert!(tree.selected_file.is_none());
    }

    #[test]
    fn closing_root_rejects_late_selection_success_and_error() {
        let mut tree = ProjectTree::new();
        let root = PathBuf::from("/project");
        let file = root.join("late.txt");
        for result in [Ok(file.clone()), Err("Late selection failed".into())] {
            tree.set_root(root.clone());
            let generation = active(&mut tree, &file, 17);
            tree.clear_root();
            tree.apply_response(Response {
                id: 17,
                generation,
                path: file.clone(),
                outcome: Outcome::Selection(result),
            });
            assert!(tree.is_settled());
            assert!(tree.root().is_none());
            assert!(tree.selected_file.is_none());
            assert!(tree.pending_selection.is_none());
            assert!(tree.notice.is_none());
        }
    }

    #[test]
    fn close_and_reopen_keeps_one_worker_request_and_rejects_old_generation() {
        let mut tree = ProjectTree::new();
        // A controlled channel makes the response race deterministic without
        // sleeps or relying on a filesystem operation to stay slow.
        let (tx, requests) = mpsc::sync_channel(1);
        let (responses, rx) = mpsc::sync_channel(1);
        tree.tx = Some(tx);
        tree.rx = rx;
        let root = PathBuf::from("/project");
        let context = egui::Context::default();
        tree.set_root(root.clone());
        tree.dispatch(&context);
        let stale = requests.try_recv().unwrap();
        assert!(matches!(stale.job, Job::List { .. }));

        // Reopening the exact same path still has to discard the old result.
        // Repeated close/reopen cannot enqueue work behind the outstanding job.
        for _ in 0..4 {
            tree.clear_root();
            tree.set_root(root.clone());
            tree.dispatch(&context);
            assert!(matches!(
                requests.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
            assert_eq!(tree.active.as_ref().unwrap().id, stale.id);
        }
        assert!(stale.cancel.load(Ordering::Acquire));
        responses
            .send(Response {
                id: stale.id,
                generation: stale.generation,
                path: root.clone(),
                outcome: Outcome::Listing(Ok(Listing {
                    entries: vec![entry(root.join("stale.txt"))],
                    has_more: false,
                })),
            })
            .unwrap();
        tree.poll(&context);
        let fresh = requests.try_recv().unwrap();
        assert_ne!(fresh.id, stale.id);
        assert_ne!(fresh.generation, stale.generation);
        assert!(!fresh.cancel.load(Ordering::Acquire));
        assert!(tree.directories[&root].entries.is_empty());

        responses
            .send(Response {
                id: fresh.id,
                generation: fresh.generation,
                path: root.clone(),
                outcome: Outcome::Listing(Ok(Listing {
                    entries: vec![entry(root.join("fresh.txt"))],
                    has_more: false,
                })),
            })
            .unwrap();
        tree.poll(&context);
        assert!(tree.is_settled());
        assert_eq!(tree.root(), Some(root.as_path()));
        assert_eq!(tree.cached_entries, 1);
        assert_eq!(
            tree.directories[&root].entries[0].path,
            root.join("fresh.txt")
        );
        assert!(matches!(
            requests.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn closing_real_worker_and_reopening_preserves_files_and_selection() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let file = root.join("keep.txt");
        let nested = root.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(&file, b"keep this content\n").unwrap();
        fs::write(nested.join("also-keep.txt"), b"nested content\n").unwrap();
        let mut tree = ProjectTree::new();
        let context = egui::Context::default();
        tree.set_root(root.clone());
        tree.perform(Action::Select(file.clone()));
        tree.dispatch(&context);
        tree.clear_root();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !tree.is_settled() && std::time::Instant::now() < deadline {
            tree.poll(&context);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(tree.is_settled());
        assert!(tree.root().is_none());
        assert!(tree.selected_file.is_none());

        tree.set_root(root.clone());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !tree.is_settled() && std::time::Instant::now() < deadline {
            tree.poll(&context);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(tree.is_settled());
        assert_eq!(tree.directories[&root].entries.len(), 2);
        tree.perform(Action::Select(file.clone()));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while tree.selected_file.is_none() && std::time::Instant::now() < deadline {
            tree.poll(&context);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(tree.selected_file.take(), Some(file.clone()));
        tree.clear_root();
        tree.clear_root(); // Closing an already closed folder is harmless.
        assert!(tree.is_settled());
        assert_eq!(fs::read(&file).unwrap(), b"keep this content\n");
        assert_eq!(
            fs::read(nested.join("also-keep.txt")).unwrap(),
            b"nested content\n"
        );
        assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
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
        // Unix paths can contain invalid UTF-8 in memory even when the host
        // filesystem (such as APFS on macOS) rejects those filenames.
        let root = PathBuf::from("/project");
        let a = entry(root.join(OsString::from_vec(vec![b'a', 0x80])));
        let b = entry(root.join(OsString::from_vec(vec![b'a', 0x81])));
        assert_eq!(a.display_name, b.display_name);
        assert_ne!(a.path, b.path);

        let mut tree = ProjectTree::new();
        tree.set_root(root);
        for item in [a, b] {
            tree.perform(Action::Select(item.path.clone()));
            assert_eq!(tree.pending_selection, Some(item.path));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn invalid_utf8_filenames_preserve_listing_and_selection_paths() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        // Linux CI supports invalid UTF-8 filenames, so also exercise the
        // actual filesystem listing and selection without ignoring I/O errors.
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join(OsString::from_vec(vec![b'a', 0x80]));
        let b = temp.path().join(OsString::from_vec(vec![b'a', 0x81]));
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        let listing = read_directory(temp.path(), temp.path(), PAGE_ENTRIES, &|| true).unwrap();
        assert_eq!(listing.entries.len(), 2);
        assert_eq!(
            listing.entries[0].display_name,
            listing.entries[1].display_name
        );
        assert_eq!(listing.entries[0].path, a);
        assert_eq!(listing.entries[1].path, b);
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
    fn source_file_icons_are_deterministic_and_ascii_only() {
        let cases = [
            ("main.c", FileIconKind::C),
            ("main.C", FileIconKind::Cpp),
            ("buffer_layout.cpp", FileIconKind::Cpp),
            ("camera.CC", FileIconKind::Cpp),
            ("main.cxx", FileIconKind::Cpp),
            ("camera.h", FileIconKind::Header),
            ("camera.HPP", FileIconKind::Header),
            ("main.rs", FileIconKind::Rust),
            ("script.PY", FileIconKind::Python),
            ("types.pyi", FileIconKind::Python),
            ("settings.json", FileIconKind::Json),
            ("settings.JSONC", FileIconKind::Json),
            ("Cargo.toml", FileIconKind::Toml),
            ("CMakeLists.txt", FileIconKind::CMake),
            ("cmakelists.TXT", FileIconKind::CMake),
            ("FindCamera.cmake", FileIconKind::CMake),
            ("README.md", FileIconKind::Generic),
            (".gitignore", FileIconKind::Generic),
            ("no-extension", FileIconKind::Generic),
            ("almost.cpp.bak", FileIconKind::Generic),
        ];
        for (name, expected) in cases {
            let kind = FileIconKind::for_path(&PathBuf::from("project").join(name));
            assert_eq!(kind, expected, "{name}");
            assert!(kind.badge().is_ascii());
            assert!(kind.description().is_ascii());
        }
    }

    #[test]
    fn root_label_uses_basename_but_preserves_filesystem_roots() {
        assert_eq!(
            root_label(Path::new("/home/example/Documents/workspace/smart_camera")),
            "smart_camera"
        );
        assert_eq!(root_label(Path::new("project")), "project");
        #[cfg(unix)]
        assert_eq!(root_label(Path::new("/")), "/");
        #[cfg(windows)]
        assert_eq!(root_label(Path::new("C:\\")), "C:\\");
    }

    #[test]
    fn capture_reveal_queues_only_parent_folders_inside_root() {
        let mut tree = ProjectTree::new();
        let root = PathBuf::from("/project");
        tree.reveal_file(&root.join("no-root/file.rs"));
        assert!(tree.directories.is_empty());
        tree.set_root(root.clone());
        tree.reveal_file(&root.join("src/camera/file.cpp"));
        assert_eq!(tree.directories.len(), 3);
        for parent in [root.clone(), root.join("src"), root.join("src/camera")] {
            assert!(matches!(
                tree.directories[&parent].state,
                DirectoryState::Queued
            ));
            assert!(tree.directories[&parent].entries.is_empty());
        }
        tree.reveal_file(&root.join("src/camera/file.cpp"));
        tree.reveal_file(&root.join("root-file.cpp"));
        tree.reveal_file(Path::new("/outside/src/file.rs"));
        tree.reveal_file(&root.join("../outside/file.rs"));
        tree.reveal_file(&root.join("src/../../outside/file.rs"));
        assert_eq!(tree.directories.len(), 3);
        assert_eq!(tree.cached_entries, 0);
        assert!(tree.pending_selection.is_none());
    }

    #[test]
    fn capture_readiness_waits_for_all_work_but_not_failed_listings() {
        let mut tree = ProjectTree::new();
        assert!(tree.is_settled());
        let root = PathBuf::from("/project");
        tree.set_root(root.clone());
        assert!(!tree.is_settled());
        tree.directories.get_mut(&root).unwrap().state = DirectoryState::Loading(1);
        assert!(!tree.is_settled());
        tree.directories.get_mut(&root).unwrap().state =
            DirectoryState::Failed("Permission denied".into());
        assert!(tree.is_settled());
        tree.directories.get_mut(&root).unwrap().state = DirectoryState::Ready;
        assert!(tree.is_settled());
        tree.perform(Action::Select(root.join("file.rs")));
        assert!(!tree.is_settled());
        tree.pending_selection = None;
        active(&mut tree, &root.join("file.rs"), 2);
        assert!(!tree.is_settled());
        tree.active = None;
        assert!(tree.is_settled());
        tree.reveal_file(&root.join("src/camera/file.cpp"));
        assert!(!tree.is_settled());
    }

    #[test]
    fn capture_reveal_obeys_cache_and_expansion_bounds_without_partial_changes() {
        let mut tree = ProjectTree::new();
        let root = PathBuf::from("/project");
        tree.set_root(root.clone());
        tree.cached_entries = MAX_CACHED_ENTRIES;
        tree.reveal_file(&root.join("src/file.rs"));
        assert_eq!(tree.directories.len(), 1);
        tree.cached_entries = 0;
        let mut deep_file = root.clone();
        for index in 0..MAX_EXPANDED_DIRECTORIES {
            deep_file.push(format!("nested-{index}"));
        }
        deep_file.push("file.rs");
        tree.reveal_file(&deep_file);
        assert_eq!(tree.directories.len(), 1);
        for index in 0..MAX_EXPANDED_DIRECTORIES - 2 {
            tree.directories
                .insert(root.join(format!("dir-{index}")), Directory::queued());
        }
        tree.reveal_file(&root.join("src/deeper/file.rs"));
        assert_eq!(tree.directories.len(), MAX_EXPANDED_DIRECTORIES - 1);
        tree.reveal_file(&root.join("src/file.rs"));
        assert_eq!(tree.directories.len(), MAX_EXPANDED_DIRECTORIES);
    }

    fn directory_entry(path: PathBuf) -> Entry {
        Entry {
            kind: EntryKind::Directory,
            ..entry(path)
        }
    }

    fn ready_directory(entries: Vec<Entry>) -> Directory {
        Directory {
            entries,
            has_more: false,
            desired_limit: PAGE_ENTRIES,
            state: DirectoryState::Ready,
        }
    }

    fn flatten_shapes<'a>(shape: &'a egui::Shape, shapes: &mut Vec<&'a egui::Shape>) {
        if let egui::Shape::Vec(children) = shape {
            for child in children {
                flatten_shapes(child, shapes);
            }
        } else {
            shapes.push(shape);
        }
    }

    fn rendered_shapes(
        tree: &mut ProjectTree,
        context: &egui::Context,
    ) -> Vec<egui::epaint::ClippedShape> {
        let output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(260.0, 620.0),
                )),
                ..Default::default()
            },
            |ui| {
                assert!(tree.render(ui).is_none());
            },
        );
        let shapes = output.shapes.clone();
        output.drop_without_applying_deltas();
        shapes
    }

    #[test]
    fn nested_folders_render_vector_disclosures_and_source_badges_in_both_themes() {
        for visuals in [egui::Visuals::dark(), egui::Visuals::light()] {
            let root = PathBuf::from("/long/home/path/project");
            let src = root.join("src");
            let nested = src.join("nested");
            let mut tree = ProjectTree::new();
            tree.set_root(root.clone());
            tree.directories.insert(
                root.clone(),
                ready_directory(vec![
                    directory_entry(src.clone()),
                    directory_entry(root.join("docs")),
                    entry(root.join("main.c")),
                    entry(root.join("camera.cpp")),
                    entry(root.join("camera.h")),
                    entry(root.join("script.py")),
                    entry(root.join("settings.json")),
                    entry(root.join("Cargo.toml")),
                    entry(root.join("CMakeLists.txt")),
                    entry(root.join("README.md")),
                ]),
            );
            tree.directories.insert(
                src.clone(),
                ready_directory(vec![
                    directory_entry(nested.clone()),
                    entry(src.join("main.rs")),
                ]),
            );
            tree.cached_entries = 12;
            let context = egui::Context::default();
            context.set_visuals(visuals);
            // Warm up layout so the test inspects the same settled rendering as
            // the desktop sidebar rather than egui's sizing/discard pass.
            rendered_shapes(&mut tree, &context);
            let output = rendered_shapes(&mut tree, &context);
            let mut shapes = Vec::new();
            for clipped in &output {
                flatten_shapes(&clipped.shape, &mut shapes);
            }
            let text: Vec<_> = shapes
                .iter()
                .filter_map(|shape| match shape {
                    egui::Shape::Text(text) => Some(text),
                    _ => None,
                })
                .collect();
            // In particular, disclosure markers are not text glyphs. All
            // labels in this ASCII fixture must render without icon fonts.
            assert!(text.iter().all(|text| text.galley.text().is_ascii()));
            for label in [
                "project", "src", "nested", "docs", "C", "C++", "H", "RS", "PY", "{}", "TM", "CM",
            ] {
                assert!(
                    text.iter().any(|text| text.galley.text() == label),
                    "missing label or badge {label}"
                );
            }
            assert!(
                !text
                    .iter()
                    .any(|text| text.galley.text().contains("/long/home"))
            );
            let triangles: Vec<_> = shapes
                .iter()
                .filter_map(|shape| match shape {
                    egui::Shape::Path(path) if path.closed && path.points.len() == 3 => Some(path),
                    _ => None,
                })
                .collect();
            assert_eq!(triangles.len(), 3, "one painted triangle per folder");
            assert_eq!(triangles[0].points[0].y, triangles[0].points[1].y);
            assert!(triangles[0].points[2].y > triangles[0].points[0].y);
            assert_eq!(triangles[1].points[0].x, triangles[1].points[2].x);
            assert!(triangles[1].points[1].x > triangles[1].points[0].x);
            // The expanded folder has its own vector front flap.
            assert_eq!(
                shapes
                    .iter()
                    .filter(|shape| matches!(
                        shape, egui::Shape::Path(path) if path.closed && path.points.len() == 4
                    ))
                    .count(),
                1
            );
            let text_x = |label| {
                text.iter()
                    .find(|text| text.galley.text() == label)
                    .unwrap()
                    .pos
                    .x
            };
            assert_eq!(text_x("src"), text_x("camera.cpp"));
            assert_eq!(text_x("nested"), text_x("main.rs"));
            assert_eq!(text_x("nested") - text_x("src"), 14.0);

            // Expand the nested folder without any filesystem reads, then
            // verify its child and open flap appear in the real row renderer.
            tree.perform(Action::Toggle(nested.clone()));
            tree.directories.insert(
                nested.clone(),
                ready_directory(vec![entry(nested.join("deep.rs"))]),
            );
            tree.cached_entries += 1;
            let output = rendered_shapes(&mut tree, &context);
            let mut shapes = Vec::new();
            for clipped in &output {
                flatten_shapes(&clipped.shape, &mut shapes);
            }
            assert!(shapes.iter().any(|shape| matches!(
                shape, egui::Shape::Text(text) if text.galley.text() == "deep.rs"
            )));
            assert_eq!(
                shapes
                    .iter()
                    .filter(|shape| matches!(
                        shape, egui::Shape::Path(path) if path.closed && path.points.len() == 4
                    ))
                    .count(),
                2
            );
            tree.perform(Action::Toggle(src));
            let output = rendered_shapes(&mut tree, &context);
            let mut shapes = Vec::new();
            for clipped in &output {
                flatten_shapes(&clipped.shape, &mut shapes);
            }
            assert!(!shapes.iter().any(|shape| matches!(
                shape, egui::Shape::Text(text) if matches!(text.galley.text(), "nested" | "deep.rs" | "main.rs")
            )));
            assert!(!shapes.iter().any(|shape| matches!(
                shape, egui::Shape::Path(path) if path.closed && path.points.len() == 4
            )));
        }
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
            // Each visible row paints several small vector shapes. A
            // non-virtualized tree would paint tens of thousands, not <200.
            assert!(output.shapes.len() < 200, "painted {}", output.shapes.len());
            output.drop_without_applying_deltas();
        }
    }
}
