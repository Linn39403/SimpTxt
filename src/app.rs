use crate::{fonts::FontCatalog, highlight};
use eframe::egui::{self, Color32, FontFamily, FontId, Key, Modifiers, RichText};
use rust_notepad_prototype::Document;
use rust_notepad_prototype::preferences::Settings;
use std::path::Path;

struct Tab {
    id: usize,
    doc: Document,
    cursor: usize,
}
#[derive(Clone, Copy)]
enum Pending {
    Tab(usize),
    Exit,
}
pub struct App {
    tabs: Vec<Tab>,
    active: usize,
    next_id: usize,
    pub dark: bool,
    wrap: bool,
    zoom: f32,
    font_size: f32,
    font: String,
    fonts: FontCatalog,
    settings: bool,
    find_open: bool,
    replace_open: bool,
    query: String,
    replacement: String,
    match_case: bool,
    search_status: String,
    pending: Option<Pending>,
    error: Option<String>,
    allow_close: bool,
    reveal: Option<(usize, usize)>,
    syntax: bool,
    persist_settings: bool,
    last_settings: Settings,
}
impl App {
    pub fn new(ctx: &egui::Context) -> Self {
        Self::with_settings(ctx, Settings::load(), true)
    }
    #[cfg(any(test, feature = "headless-capture"))]
    pub fn isolated(ctx: &egui::Context) -> Self {
        Self::with_settings(ctx, Settings::default(), false)
    }
    fn with_settings(ctx: &egui::Context, settings: Settings, persist_settings: bool) -> Self {
        let fonts = FontCatalog::discover();
        let mut font = settings.font.clone();
        if !fonts.install(ctx, &font) {
            font = "Default monospace".into();
            fonts.install(ctx, "Default monospace");
        }
        Self {
            tabs: vec![Tab {
                id: 0,
                doc: Document::new(),
                cursor: 0,
            }],
            active: 0,
            next_id: 1,
            dark: settings.dark,
            wrap: settings.wrap,
            zoom: settings.zoom,
            font_size: settings.font_size,
            font,
            fonts,
            settings: false,
            find_open: false,
            replace_open: false,
            query: String::new(),
            replacement: String::new(),
            match_case: false,
            search_status: String::new(),
            pending: None,
            error: None,
            allow_close: false,
            reveal: None,
            syntax: settings.syntax,
            persist_settings,
            last_settings: settings,
        }
    }
    pub fn open_path(&mut self, path: &Path) {
        match Document::open(path) {
            Ok(doc) => {
                if self.tabs.len() == 1
                    && self.tabs[0].doc.text().is_empty()
                    && self.tabs[0].doc.path().is_none()
                {
                    self.tabs.clear();
                }
                self.tabs.push(Tab {
                    id: self.next_id,
                    doc,
                    cursor: 0,
                });
                self.next_id += 1;
                self.active = self.tabs.len() - 1;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }
    fn new_tab(&mut self) {
        self.tabs.push(Tab {
            id: self.next_id,
            doc: Document::new(),
            cursor: 0,
        });
        self.next_id += 1;
        self.active = self.tabs.len() - 1;
    }
    fn save(&mut self, save_as: bool) -> bool {
        let doc = &mut self.tabs[self.active].doc;
        let result = if save_as || doc.path().is_none() {
            let mut dialog = rfd::FileDialog::new().set_title("Save as");
            if let Some(path) = doc.path() {
                dialog =
                    dialog.set_file_name(path.file_name().unwrap_or_default().to_string_lossy());
            }
            let Some(path) = dialog.save_file() else {
                return false;
            };
            let same_destination = std::fs::canonicalize(&path)
                .ok()
                .is_some_and(|resolved| doc.path() == Some(resolved.as_path()));
            if path.exists() && !same_destination {
                let answer = rfd::MessageDialog::new()
                    .set_title("Replace existing file?")
                    .set_description(format!(
                        "{} already exists. Replace its contents?",
                        path.display()
                    ))
                    .set_level(rfd::MessageLevel::Warning)
                    .set_buttons(rfd::MessageButtons::YesNo)
                    .show();
                if answer != rfd::MessageDialogResult::Yes {
                    return false;
                }
            }
            doc.save_as(path)
        } else {
            doc.save()
        };
        match result {
            Ok(()) => true,
            Err(e) => {
                self.error = Some(e.to_string());
                false
            }
        }
    }
    fn open(&mut self) {
        if let Some(path) = rfd::FileDialog::new().set_title("Open").pick_file() {
            self.open_path(&path);
        }
    }
    fn close_tab(&mut self, index: usize) {
        if self.tabs[index].doc.is_dirty() {
            self.active = index;
            self.pending = Some(Pending::Tab(index));
        } else {
            self.remove_tab(index);
        }
    }
    fn remove_tab(&mut self, index: usize) {
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.new_tab();
        }
        self.active = self.active.min(self.tabs.len() - 1);
    }
    fn exit(&mut self, ctx: &egui::Context) {
        if let Some(i) = self.tabs.iter().position(|t| t.doc.is_dirty()) {
            self.active = i;
            self.pending = Some(Pending::Exit);
        } else {
            self.allow_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
    fn resolved(&mut self, pending: Pending, ctx: &egui::Context) {
        self.pending = None;
        match pending {
            Pending::Tab(i) => self.remove_tab(i),
            Pending::Exit => {
                self.remove_tab(self.active);
                self.exit(ctx);
            }
        }
    }
    fn undo(&mut self, ctx: &egui::Context, redo: bool) {
        let tab = &mut self.tabs[self.active];
        if redo {
            tab.doc.redo();
        } else {
            tab.doc.undo();
        }
        if let Some(mut state) = egui::TextEdit::load_state(ctx, egui::Id::new(("editor", tab.id)))
        {
            state.clear_undoer();
            state.store(ctx, egui::Id::new(("editor", tab.id)));
        }
    }
    fn find(&mut self, replace: bool) {
        let tab = &mut self.tabs[self.active];
        let result = if replace {
            tab.doc
                .replace_one(&self.query, &self.replacement, tab.cursor, self.match_case)
        } else {
            tab.doc.find(&self.query, tab.cursor, self.match_case)
        };
        if let Some(range) = result {
            let start = tab.doc.text()[..range.start].chars().count();
            let end = tab.doc.text()[..range.end].chars().count();
            tab.cursor = range.end;
            self.reveal = Some((start, end));
            self.search_status.clear();
        } else {
            self.search_status = "No matches".into();
        }
    }
    fn shortcuts(&mut self, ctx: &egui::Context) {
        let command = Modifiers::COMMAND;
        let take = |mods, key| ctx.input_mut(|i| i.consume_key(mods, key));
        if take(command, Key::N) {
            self.new_tab();
        }
        if take(command, Key::O) {
            self.open();
        }
        if take(
            Modifiers {
                shift: true,
                ..command
            },
            Key::S,
        ) {
            self.save(true);
        } else if take(command, Key::S) {
            self.save(false);
        }
        if take(command, Key::W) {
            self.close_tab(self.active);
        }
        if take(command, Key::F) {
            self.find_open = true;
        }
        if take(command, Key::H) {
            self.find_open = true;
            self.replace_open = true;
        }
        if take(
            Modifiers {
                shift: true,
                ..command
            },
            Key::Z,
        ) || take(command, Key::Y)
        {
            self.undo(ctx, true);
        } else if take(command, Key::Z) {
            self.undo(ctx, false);
        }
        if take(command, Key::Plus) || take(command, Key::Equals) {
            self.zoom = (self.zoom + 0.1).min(3.);
        }
        if take(command, Key::Minus) {
            self.zoom = (self.zoom - 0.1).max(0.5);
        }
        if take(command, Key::Num0) {
            self.zoom = 1.;
        }
        if take(Modifiers::NONE, Key::Escape) {
            self.find_open = false;
            self.settings = false;
        }
    }
    fn current_settings(&self) -> Settings {
        Settings {
            font: self.font.clone(),
            font_size: self.font_size,
            dark: self.dark,
            wrap: self.wrap,
            zoom: self.zoom,
            syntax: self.syntax,
        }
    }
    fn persist_preferences(&mut self) {
        let settings = self.current_settings();
        if self.persist_settings && settings != self.last_settings {
            if let Err(error) = settings.save() {
                self.error = Some(format!("Could not save your settings: {error}"));
            }
            self.last_settings = settings;
        }
    }
    fn settings_page(&mut self, root: &mut egui::Ui, blocked: bool) {
        let ctx = root.ctx().clone();
        let bg = if self.dark {
            Color32::from_rgb(32, 32, 32)
        } else {
            Color32::from_rgb(243, 243, 243)
        };
        let card = if self.dark {
            Color32::from_rgb(43, 43, 43)
        } else {
            Color32::WHITE
        };
        egui::Panel::top("settings_header")
            .frame(
                egui::Frame::NONE
                    .fill(bg)
                    .inner_margin(egui::Margin::symmetric(22, 18)),
            )
            .show(root, |ui| {
                if blocked {
                    ui.disable();
                }
                ui.horizontal(|ui| {
                    if ui
                        .button(RichText::new("<").size(22.))
                        .on_hover_text("Back to editor (Escape)")
                        .clicked()
                    {
                        self.settings = false;
                    }
                    ui.add_space(16.);
                    ui.label(RichText::new("Settings").size(26.).strong());
                });
            });
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(bg)
                    .inner_margin(egui::Margin::symmetric(64, 18)),
            )
            .show(root, |ui| {
                if blocked {
                    ui.disable();
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.set_max_width(900.);
                    ui.label(RichText::new("Appearance").size(17.).strong());
                    ui.add_space(12.);
                    egui::Frame::NONE
                        .fill(card)
                        .corner_radius(7)
                        .inner_margin(18)
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("App theme").size(16.));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        egui::ComboBox::from_id_salt("theme")
                                            .selected_text(if self.dark { "Dark" } else { "Light" })
                                            .width(180.)
                                            .show_ui(ui, |ui| {
                                                ui.selectable_value(&mut self.dark, false, "Light");
                                                ui.selectable_value(&mut self.dark, true, "Dark");
                                            });
                                    },
                                );
                            });
                        });
                    ui.add_space(28.);
                    ui.label(RichText::new("Text formatting").size(17.).strong());
                    ui.add_space(12.);
                    egui::Frame::NONE
                        .fill(card)
                        .corner_radius(7)
                        .inner_margin(18)
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.label(RichText::new("Font").size(16.));
                            ui.add_space(14.);
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.label("Family");
                                    egui::ComboBox::from_id_salt("font")
                                        .selected_text(&self.font)
                                        .width(270.)
                                        .show_ui(ui, |ui| {
                                            for name in self.fonts.names() {
                                                if ui
                                                    .selectable_value(
                                                        &mut self.font,
                                                        name.clone(),
                                                        &name,
                                                    )
                                                    .changed()
                                                    && !self.fonts.install(&ctx, &self.font)
                                                {
                                                    self.font = "Default monospace".into();
                                                    self.fonts.install(&ctx, &self.font);
                                                }
                                            }
                                        });
                                });
                                ui.add_space(22.);
                                ui.vertical(|ui| {
                                    ui.label("Size");
                                    ui.add(
                                        egui::DragValue::new(&mut self.font_size)
                                            .range(8.0..=40.0)
                                            .speed(1.),
                                    );
                                });
                            });
                            ui.add_space(22.);
                            ui.label(
                                RichText::new("The quick brown fox jumps over the lazy dog.").font(
                                    FontId::new(self.font_size, FontFamily::Name("editor".into())),
                                ),
                            );
                            ui.add_space(10.);
                        });
                    ui.add_space(8.);
                    egui::Frame::NONE
                        .fill(card)
                        .corner_radius(7)
                        .inner_margin(18)
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.checkbox(&mut self.wrap, "Word wrap");
                        });
                    ui.add_space(28.);
                    ui.label(RichText::new("Plain-text extras").size(17.).strong());
                    ui.add_space(12.);
                    egui::Frame::NONE
                        .fill(card)
                        .corner_radius(7)
                        .inner_margin(18)
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.checkbox(&mut self.syntax, "Source code colors");
                            ui.add_space(6.);
                            ui.label("Colors are display-only. Files stay plain text.");
                        });
                    ui.add_space(24.);
                    ui.label(RichText::new("Notepad prototype").strong());
                    ui.label(
                        "A simple Rust text editor. Appearance choices are saved on this device.",
                    );
                });
            });
    }
    #[cfg(any(test, feature = "headless-capture"))]
    pub fn show_settings(&mut self) {
        self.settings = true;
    }
    pub fn render(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        ctx.set_visuals(if self.dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
        ctx.style_mut_of(
            if self.dark {
                egui::Theme::Dark
            } else {
                egui::Theme::Light
            },
            |s| {
                s.spacing.button_padding = egui::vec2(11., 6.);
                s.visuals.widgets.inactive.bg_fill = Color32::TRANSPARENT;
                s.visuals.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
                s.visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;
                s.spacing.interact_size.y = 28.;
                s.text_styles
                    .insert(egui::TextStyle::Button, FontId::proportional(14.));
                s.text_styles
                    .insert(egui::TextStyle::Body, FontId::proportional(14.));
            },
        );
        if !self.allow_close && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.exit(&ctx);
        }
        if self.pending.is_none() && self.error.is_none() {
            self.shortcuts(&ctx);
        }
        let panel = if self.dark {
            Color32::from_rgb(32, 32, 32)
        } else {
            Color32::from_rgb(243, 243, 243)
        };
        let editor_bg = if self.dark {
            Color32::from_rgb(39, 39, 39)
        } else {
            Color32::WHITE
        };
        let blocked = self.pending.is_some() || self.error.is_some();
        root.set_style(ctx.style_of(if self.dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        }));
        if !self.settings {
            egui::Panel::top("tabs")
                .frame(egui::Frame::NONE.fill(panel).inner_margin(egui::Margin {
                    left: 12,
                    right: 12,
                    top: 8,
                    bottom: 0,
                }))
                .show(root, |ui| {
                    if blocked {
                        ui.disable();
                    }
                    ui.horizontal(|ui| {
                        let mut close = None;
                        for (i, tab) in self.tabs.iter().enumerate() {
                            let name = tab
                                .doc
                                .path()
                                .and_then(|p| p.file_name())
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "Untitled".into());
                            let fill = if i == self.active {
                                editor_bg
                            } else {
                                Color32::TRANSPARENT
                            };
                            egui::Frame::NONE
                                .fill(fill)
                                .corner_radius(egui::CornerRadius {
                                    nw: 8,
                                    ne: 8,
                                    sw: 0,
                                    se: 0,
                                })
                                .inner_margin(egui::Margin::symmetric(12, 2))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        if ui
                                            .selectable_label(
                                                false,
                                                format!(
                                                    "{}{}",
                                                    name,
                                                    if tab.doc.is_dirty() { "  •" } else { "" }
                                                ),
                                            )
                                            .clicked()
                                        {
                                            self.active = i;
                                        }
                                        if ui
                                            .add(
                                                egui::Button::new(RichText::new("×").size(17.))
                                                    .frame(false),
                                            )
                                            .on_hover_text("Close tab")
                                            .clicked()
                                        {
                                            close = Some(i);
                                        }
                                    });
                                });
                        }
                        if ui
                            .button(RichText::new("+").size(20.))
                            .on_hover_text("New tab (Ctrl+N)")
                            .clicked()
                        {
                            self.new_tab();
                        }
                        if let Some(i) = close {
                            self.close_tab(i);
                        }
                    });
                });
            egui::Panel::top("menu")
                .frame(
                    egui::Frame::NONE
                        .fill(panel)
                        .inner_margin(egui::Margin::symmetric(12, 6)),
                )
                .show(root, |ui| {
                    if blocked {
                        ui.disable();
                    }
                    ui.horizontal(|ui| {
                        ui.menu_button("File", |ui| {
                            if ui.button("New tab                    Ctrl+N").clicked() {
                                self.new_tab();
                                ui.close();
                            }
                            if ui.button("Open…                      Ctrl+O").clicked() {
                                self.open();
                                ui.close();
                            }
                            if ui.button("Save                         Ctrl+S").clicked() {
                                self.save(false);
                                ui.close();
                            }
                            if ui.button("Save as…           Ctrl+Shift+S").clicked() {
                                self.save(true);
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Close tab                   Ctrl+W").clicked() {
                                self.close_tab(self.active);
                                ui.close();
                            }
                            if ui.button("Exit").clicked() {
                                self.exit(&ctx);
                                ui.close();
                            }
                        });
                        ui.menu_button("Edit", |ui| {
                            if ui
                                .add_enabled(
                                    self.tabs[self.active].doc.can_undo(),
                                    egui::Button::new("Undo     Ctrl+Z"),
                                )
                                .clicked()
                            {
                                self.undo(&ctx, false);
                                ui.close();
                            }
                            if ui
                                .add_enabled(
                                    self.tabs[self.active].doc.can_redo(),
                                    egui::Button::new("Redo     Ctrl+Y"),
                                )
                                .clicked()
                            {
                                self.undo(&ctx, true);
                                ui.close();
                            }
                            ui.separator();
                            for (label, command) in [
                                (
                                    "Cut                    Ctrl+X",
                                    egui::ViewportCommand::RequestCut,
                                ),
                                (
                                    "Copy                  Ctrl+C",
                                    egui::ViewportCommand::RequestCopy,
                                ),
                                (
                                    "Paste                  Ctrl+V",
                                    egui::ViewportCommand::RequestPaste,
                                ),
                            ] {
                                if ui.button(label).clicked() {
                                    ctx.memory_mut(|m| {
                                        m.request_focus(egui::Id::new((
                                            "editor",
                                            self.tabs[self.active].id,
                                        )))
                                    });
                                    ctx.send_viewport_cmd(command);
                                    ui.close();
                                }
                            }
                            if ui.button("Find…                 Ctrl+F").clicked() {
                                self.find_open = true;
                                ui.close();
                            }
                            if ui.button("Replace…            Ctrl+H").clicked() {
                                self.find_open = true;
                                self.replace_open = true;
                                ui.close();
                            }
                            if ui.button("Select all            Ctrl+A").clicked() {
                                let n = self.tabs[self.active].doc.text().chars().count();
                                self.reveal = Some((0, n));
                                ui.close();
                            }
                        });
                        ui.menu_button("View", |ui| {
                            ui.checkbox(&mut self.wrap, "Word wrap");
                            ui.checkbox(&mut self.syntax, "Source code colors");
                            ui.separator();
                            if ui.button("Zoom in       Ctrl++").clicked() {
                                self.zoom = (self.zoom + 0.1).min(3.);
                            }
                            if ui.button("Zoom out      Ctrl+-").clicked() {
                                self.zoom = (self.zoom - 0.1).max(0.5);
                            }
                            if ui.button("Restore zoom  Ctrl+0").clicked() {
                                self.zoom = 1.;
                            }
                            ui.separator();
                            ui.checkbox(&mut self.dark, "Dark theme");
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .button(RichText::new("⚙").size(19.))
                                .on_hover_text("Settings")
                                .clicked()
                            {
                                self.settings = !self.settings;
                            }
                        });
                    });
                });
            egui::Panel::bottom("status")
                .frame(
                    egui::Frame::NONE
                        .fill(panel)
                        .inner_margin(egui::Margin::symmetric(18, 7)),
                )
                .show(root, |ui| {
                    let tab = &self.tabs[self.active];
                    let (line, col) = tab.doc.line_col(tab.cursor.min(tab.doc.text().len()));
                    ui.horizontal(|ui| {
                        ui.label(format!("Ln {line}, Col {col}"));
                        ui.add_space(25.);
                        ui.label(format!("{} characters", tab.doc.char_count()));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(if tab.doc.has_bom() {
                                "UTF-8 BOM"
                            } else {
                                "UTF-8"
                            });
                            ui.separator();
                            ui.label(tab.doc.line_ending_label());
                            ui.separator();
                            ui.label(format!("{}%", (self.zoom * 100.).round() as u32));
                        });
                    });
                });
            if self.find_open {
                egui::Panel::top("find").show(root, |ui| {
                    if blocked {
                        ui.disable();
                    }
                    ui.horizontal(|ui| {
                        ui.label("Find");
                        ui.add(egui::TextEdit::singleline(&mut self.query).desired_width(220.));
                        if ui.button("Find next").clicked() {
                            self.find(false);
                        }
                        ui.checkbox(&mut self.match_case, "Match case");
                        ui.checkbox(&mut self.replace_open, "Replace");
                        if ui.button("×").clicked() {
                            self.find_open = false;
                        }
                        ui.label(&self.search_status);
                    });
                    if self.replace_open {
                        ui.horizontal(|ui| {
                            ui.label("With");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.replacement)
                                    .desired_width(220.),
                            );
                            if ui.button("Replace next").clicked() {
                                self.find(true);
                            }
                            if ui.button("Replace all").clicked() {
                                let n = self.tabs[self.active].doc.replace_all(
                                    &self.query,
                                    &self.replacement,
                                    self.match_case,
                                );
                                self.search_status = format!("{n} replacements");
                            }
                        });
                    }
                });
            }
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::NONE
                        .fill(editor_bg)
                        .inner_margin(egui::Margin::symmetric(18, 14)),
                )
                .show(root, |ui| {
                    if blocked {
                        ui.disable();
                    }
                    let tab = &mut self.tabs[self.active];
                    let mut text = tab.doc.text().to_owned();
                    let id = egui::Id::new(("editor", tab.id));
                    if let Some((a, b)) = self.reveal.take() {
                        let mut state = egui::TextEdit::load_state(&ctx, id).unwrap_or_default();
                        state
                            .cursor
                            .set_char_range(Some(egui::text::CCursorRange::two(
                                egui::text::CCursor::new(a),
                                egui::text::CCursor::new(b),
                            )));
                        state.store(&ctx, id);
                        ctx.memory_mut(|m| m.request_focus(id));
                    }
                    let font = FontId::new(
                        self.font_size * self.zoom,
                        FontFamily::Name("editor".into()),
                    );
                    let syntax =
                        self.syntax && tab.doc.path().is_some_and(highlight::supports_path);
                    let dark = self.dark;
                    let wrap = self.wrap;
                    let mut layouter =
                        |ui: &egui::Ui, buffer: &dyn egui::TextBuffer, width: f32| {
                            ui.fonts_mut(|f| {
                                f.layout_job(highlight::layout(
                                    buffer.as_str(),
                                    &font,
                                    dark,
                                    syntax,
                                    if wrap { width } else { f32::INFINITY },
                                ))
                            })
                        };
                    egui::ScrollArea::both()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            let output = egui::TextEdit::multiline(&mut text)
                                .id(id)
                                .font(font.clone())
                                .frame(egui::Frame::NONE)
                                .desired_width(if wrap {
                                    ui.available_width()
                                } else {
                                    f32::INFINITY
                                })
                                .desired_rows(30)
                                .layouter(&mut layouter)
                                .show(ui);
                            if output.response.changed() {
                                tab.doc.set_text(text);
                            }
                            if let Some(range) = output.cursor_range {
                                tab.cursor = tab
                                    .doc
                                    .text()
                                    .char_indices()
                                    .nth(range.primary.index.into())
                                    .map_or(tab.doc.text().len(), |(b, _)| b);
                            }
                        });
                });
        }
        if self.settings {
            self.settings_page(root, blocked);
        }
        self.persist_preferences();
        if let Some(pending) = self.pending {
            egui::Modal::new(egui::Id::new("unsaved")).show(&ctx, |ui| {
                ui.heading("Save changes?");
                ui.label("This tab has unsaved changes.");
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() && self.save(false) {
                        self.resolved(pending, &ctx);
                    }
                    if ui.button("Don't save").clicked() {
                        self.resolved(pending, &ctx);
                    }
                    if ui.button("Cancel").clicked() {
                        self.pending = None;
                    }
                });
            });
        }
        if let Some(error) = self.error.clone() {
            egui::Modal::new(egui::Id::new("error")).show(&ctx, |ui| {
                ui.heading("Couldn't complete that");
                ui.label(error);
                if ui.button("OK").clicked() {
                    self.error = None;
                }
            });
        }
    }
}
impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _: &mut eframe::Frame) {
        self.render(root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(app: &mut App, ctx: &egui::Context, events: Vec<egui::Event>) {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1100., 720.),
                )),
                events,
                focused: true,
                ..Default::default()
            },
            |ui| app.render(ui),
        );
        output.textures_delta.clear();
    }
    #[test]
    fn dirty_tab_close_requires_explicit_resolution() {
        let ctx = egui::Context::default();
        let mut app = App::isolated(&ctx);
        app.tabs[0].doc.set_text("keep me".into());
        app.close_tab(0);
        assert!(matches!(app.pending, Some(Pending::Tab(0))));
        assert_eq!(app.tabs[0].doc.text(), "keep me");
        app.pending = None;
        assert_eq!(app.tabs.len(), 1);
    }
    #[test]
    fn exit_prompts_for_every_dirty_tab() {
        let ctx = egui::Context::default();
        let mut app = App::isolated(&ctx);
        app.tabs[0].doc.set_text("first".into());
        app.new_tab();
        app.tabs[1].doc.set_text("second".into());
        app.exit(&ctx);
        assert_eq!(app.active, 0);
        app.resolved(Pending::Exit, &ctx);
        assert!(app.pending.is_some());
        assert_eq!(app.tabs[0].doc.text(), "second");
        assert!(!app.allow_close);
    }
    #[test]
    fn real_editor_input_and_undo() {
        let ctx = egui::Context::default();
        let mut app = App::isolated(&ctx);
        frame(&mut app, &ctx, vec![]);
        ctx.memory_mut(|m| m.request_focus(egui::Id::new(("editor", 0usize))));
        frame(&mut app, &ctx, vec![egui::Event::Text("Hello 世界".into())]);
        assert_eq!(app.tabs[0].doc.text(), "Hello 世界");
        app.undo(&ctx, false);
        assert_eq!(app.tabs[0].doc.text(), "");
        app.undo(&ctx, true);
        assert_eq!(app.tabs[0].doc.text(), "Hello 世界");
    }
}

#[cfg(test)]
#[path = "../tests/support/ui_safety.rs"]
mod independent_safety_tests;

#[cfg(test)]
mod preferences_ui_tests {
    use super::*;
    #[test]
    fn restored_preferences_apply_without_persisting_test_state() {
        let ctx = egui::Context::default();
        let settings = Settings {
            dark: true,
            wrap: false,
            font_size: 22.,
            zoom: 1.4,
            ..Default::default()
        };
        let app = App::with_settings(&ctx, settings.clone(), false);
        assert_eq!(app.current_settings(), settings);
        assert!(!app.persist_settings);
    }
    #[test]
    fn settings_page_preserves_document_and_back_state() {
        let ctx = egui::Context::default();
        let mut app = App::isolated(&ctx);
        app.tabs[0].doc.set_text("unsaved text".into());
        app.show_settings();
        let mut result = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200., 800.),
                )),
                ..Default::default()
            },
            |ui| app.render(ui),
        );
        result.textures_delta.clear();
        assert_eq!(app.tabs[0].doc.text(), "unsaved text");
        assert!(app.tabs[0].doc.is_dirty());
        app.settings = false;
        assert_eq!(app.tabs.len(), 1);
    }
}
