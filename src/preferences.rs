//! Small, document-independent presentation settings shared by every tab.
use eframe::egui::{self, Color32, Event, Key, MouseWheelUnit, Vec2};

pub const DEFAULT_FONT_SIZE: f32 = 15.0;
pub const MIN_FONT_SIZE: f32 = 9.0;
pub const MAX_FONT_SIZE: f32 = 36.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EditorPreferences {
    pub font_size: f32,
    pub theme: Theme,
}

impl Default for EditorPreferences {
    fn default() -> Self {
        Self {
            font_size: DEFAULT_FONT_SIZE,
            theme: Theme::Dark,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub background: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub current_line: Color32,
    pub selection: Color32,
    pub warning: Color32,
    pub comment: Color32,
    pub string: Color32,
    pub number: Color32,
    pub keyword: Color32,
}

impl EditorPreferences {
    pub fn row_height(self) -> f32 {
        self.font_size + 8.0
    }

    pub fn reset_zoom(&mut self) {
        self.font_size = DEFAULT_FONT_SIZE;
    }

    pub fn toggle_theme(&mut self) {
        self.theme = match self.theme {
            Theme::Dark => Theme::Light,
            Theme::Light => Theme::Dark,
        };
    }

    pub fn palette(self) -> Palette {
        let rgb = Color32::from_rgb;
        match self.theme {
            Theme::Dark => Palette {
                background: rgb(20, 25, 33),
                text: rgb(209, 216, 226),
                muted: rgb(145, 158, 174),
                accent: rgb(106, 207, 188),
                current_line: rgb(26, 33, 43),
                selection: rgb(45, 76, 96),
                warning: rgb(244, 200, 100),
                comment: rgb(142, 167, 147),
                string: rgb(218, 201, 138),
                number: rgb(197, 167, 222),
                keyword: rgb(130, 191, 235),
            },
            Theme::Light => Palette {
                background: rgb(250, 251, 253),
                text: rgb(30, 39, 52),
                muted: rgb(87, 101, 120),
                accent: rgb(0, 104, 90),
                current_line: rgb(237, 242, 247),
                selection: rgb(191, 218, 242),
                warning: rgb(140, 74, 0),
                comment: rgb(65, 105, 76),
                string: rgb(128, 73, 15),
                number: rgb(116, 58, 148),
                keyword: rgb(18, 87, 158),
            },
        }
    }

    pub fn apply_theme(self, ctx: &egui::Context) {
        ctx.set_theme(match self.theme {
            Theme::Dark => egui::Theme::Dark,
            Theme::Light => egui::Theme::Light,
        });
        let mut visuals = match self.theme {
            Theme::Dark => egui::Visuals::dark(),
            Theme::Light => egui::Visuals::light(),
        };
        let palette = self.palette();
        visuals.panel_fill = palette.background;
        visuals.selection.bg_fill = palette.selection;
        visuals.selection.stroke.color = palette.accent;
        ctx.set_visuals(visuals);
        // Otherwise egui also applies Ctrl +/- to the entire application.
        ctx.options_mut(|options| options.zoom_with_keyboard = false);
    }

    /// Consume editor zoom before widgets, scrolling, and Vim see the events.
    /// Shortcuts also work while a search field or sidebar owns keyboard focus.
    pub fn handle_zoom(&mut self, ctx: &egui::Context) -> bool {
        ctx.options_mut(|options| options.zoom_with_keyboard = false);
        let old = self.font_size;
        ctx.input_mut(|input| {
            let mut shortcut_text = Vec::new();
            let mut wheel_input = false;
            input.events.retain(|event| {
                match event {
                    Event::Key {
                        key,
                        pressed,
                        modifiers,
                        ..
                    } if (modifiers.ctrl || modifiers.command) && !modifiers.alt => {
                        let delta = match key {
                            Key::Plus | Key::Equals => Some(1.0),
                            Key::Minus => Some(-1.0),
                            Key::Num0 => Some(0.0),
                            _ => None,
                        };
                        if let Some(delta) = delta {
                            if *pressed {
                                shortcut_text.push(*key);
                                if delta == 0.0 {
                                    self.reset_zoom();
                                } else {
                                    self.font_size = (self.font_size + delta)
                                        .clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
                                }
                            }
                            return false;
                        }
                    }
                    Event::MouseWheel {
                        unit,
                        delta,
                        modifiers,
                        ..
                    } if (modifiers.ctrl || modifiers.command) && !modifiers.alt => {
                        let amount = match unit {
                            MouseWheelUnit::Point => delta.y / 24.0,
                            MouseWheelUnit::Line => delta.y,
                            MouseWheelUnit::Page => delta.y * 3.0,
                        };
                        if amount.is_finite() {
                            self.font_size = (self.font_size + amount.clamp(-8.0, 8.0))
                                .clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
                        }
                        wheel_input = true;
                        return false;
                    }
                    _ => {}
                }
                true
            });
            if !shortcut_text.is_empty() {
                // Some integrations report printable shortcut text as well.
                // Consume only a corresponding text event for each key, not
                // unrelated text delivered in the same frame as wheel input.
                input.events.retain(|event| {
                    let Event::Text(text) = event else {
                        return true;
                    };
                    if let Some(index) = shortcut_text.iter().position(|key| match key {
                        Key::Plus | Key::Equals => matches!(text.as_str(), "+" | "="),
                        Key::Minus => text == "-",
                        Key::Num0 => text == "0",
                        _ => false,
                    }) {
                        shortcut_text.remove(index);
                        false
                    } else {
                        true
                    }
                });
            }
            if wheel_input || input.modifiers.ctrl || input.modifiers.command {
                input.smooth_scroll_delta = Vec2::ZERO;
            }
        });
        self.font_size != old
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: Key, shifted: bool) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers {
                ctrl: true,
                command: true,
                shift: shifted,
                ..Default::default()
            },
        }
    }

    fn frame(settings: &mut EditorPreferences, events: Vec<Event>) -> (Vec<Event>, Vec2, f32) {
        let ctx = egui::Context::default();
        settings.apply_theme(&ctx);
        let mut remaining = Vec::new();
        let mut scroll = Vec2::ZERO;
        let mut output = ctx.run_ui(
            egui::RawInput {
                events,
                ..Default::default()
            },
            |ui| {
                settings.handle_zoom(ui.ctx());
                remaining = ui.input(|i| i.events.clone());
                scroll = ui.input(|i| i.smooth_scroll_delta);
            },
        );
        output.textures_delta.clear();
        (remaining, scroll, ctx.zoom_factor())
    }

    #[test]
    fn keyboard_zoom_consumes_shortcut_text_without_global_zoom() {
        let mut settings = EditorPreferences::default();
        for k in [Key::Equals, Key::Plus] {
            let (events, _, ui_zoom) =
                frame(&mut settings, vec![key(k, true), Event::Text("+".into())]);
            assert!(events.is_empty());
            assert_eq!(ui_zoom, 1.0);
        }
        assert_eq!(settings.font_size, DEFAULT_FONT_SIZE + 2.0);
        frame(&mut settings, vec![key(Key::Minus, false)]);
        assert_eq!(settings.font_size, DEFAULT_FONT_SIZE + 1.0);
        frame(&mut settings, vec![key(Key::Num0, false)]);
        assert_eq!(settings.font_size, DEFAULT_FONT_SIZE);
    }

    #[test]
    fn zoom_clamps_repeated_commands_and_retains_unrelated_input() {
        let mut settings = EditorPreferences::default();
        frame(
            &mut settings,
            (0..100).map(|_| key(Key::Plus, false)).collect(),
        );
        assert_eq!(settings.font_size, MAX_FONT_SIZE);
        frame(
            &mut settings,
            (0..100).map(|_| key(Key::Minus, false)).collect(),
        );
        assert_eq!(settings.font_size, MIN_FONT_SIZE);
        let (events, _, _) = frame(&mut settings, vec![Event::Text("ordinary text".into())]);
        assert!(matches!(&events[..], [Event::Text(text)] if text == "ordinary text"));
        let (events, _, _) = frame(
            &mut settings,
            vec![key(Key::Plus, false), Event::Text("-".into())],
        );
        assert!(matches!(&events[..], [Event::Text(text)] if text == "-"));
    }

    #[test]
    fn control_wheel_zooms_both_directions_without_scrolling() {
        let mut settings = EditorPreferences::default();
        for delta in [2.0, -1.0] {
            let (events, scroll, _) = frame(
                &mut settings,
                vec![Event::MouseWheel {
                    unit: MouseWheelUnit::Line,
                    delta: Vec2::new(0.0, delta),
                    phase: egui::TouchPhase::Move,
                    modifiers: egui::Modifiers::CTRL,
                }],
            );
            assert!(events.is_empty());
            assert_eq!(scroll, Vec2::ZERO);
        }
        assert_eq!(settings.font_size, DEFAULT_FONT_SIZE + 1.0);
    }

    #[test]
    fn both_palettes_and_row_geometry_change_without_ui_scaling() {
        let ctx = egui::Context::default();
        let mut settings = EditorPreferences::default();
        let dark = settings.palette();
        settings.toggle_theme();
        settings.apply_theme(&ctx);
        assert!(!ctx.global_style().visuals.dark_mode);
        assert_eq!(ctx.theme(), egui::Theme::Light);
        assert_ne!(dark.background, settings.palette().background);
        assert_ne!(dark.keyword, settings.palette().keyword);
        settings.font_size = 30.0;
        assert_eq!(settings.row_height(), 38.0);
        settings.toggle_theme();
        settings.apply_theme(&ctx);
        assert!(ctx.global_style().visuals.dark_mode);
        assert_eq!(ctx.theme(), egui::Theme::Dark);
        assert_eq!(ctx.zoom_factor(), 1.0);
    }
}
