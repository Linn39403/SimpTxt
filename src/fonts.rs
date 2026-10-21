//! Best-effort OS font files, with egui fallback. Does not alter system fonts.
use eframe::egui;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Default)]
pub struct FontCatalog {
    files: BTreeMap<String, PathBuf>,
}

impl FontCatalog {
    pub fn discover() -> Self {
        let mut roots = Vec::new();
        #[cfg(target_os = "windows")]
        {
            if let Some(windir) = std::env::var_os("WINDIR") {
                roots.push(PathBuf::from(windir).join("Fonts"));
            }
            if let Some(local) = std::env::var_os("LOCALAPPDATA") {
                roots.push(PathBuf::from(local).join("Microsoft/Windows/Fonts"));
            }
        }
        #[cfg(target_os = "macos")]
        {
            roots.extend([
                PathBuf::from("/System/Library/Fonts"),
                PathBuf::from("/Library/Fonts"),
            ]);
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            roots.extend([
                PathBuf::from("/usr/share/fonts"),
                PathBuf::from("/usr/local/share/fonts"),
            ]);
        }
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            roots.extend([
                home.join(".fonts"),
                home.join(".local/share/fonts"),
                home.join("Library/Fonts"),
            ]);
        }
        let mut result = Self::default();
        for root in roots {
            result.scan(&root, 0);
        }
        result
    }

    fn scan(&mut self, root: &Path, depth: usize) {
        if depth > 8 || self.files.len() >= 4096 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                self.scan(&path, depth + 1);
            } else if kind.is_file()
                && path
                    .extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|x| x.eq_ignore_ascii_case("ttf") || x.eq_ignore_ascii_case("otf"))
                && let Some(name) = path.file_stem().and_then(|x| x.to_str())
            {
                self.files.entry(name.to_owned()).or_insert(path);
            }
        }
    }

    pub fn names(&self) -> Vec<String> {
        std::iter::once("Default monospace".to_owned())
            .chain(self.files.keys().cloned())
            .collect()
    }

    pub fn install(&self, ctx: &egui::Context, name: &str) -> bool {
        let mut definitions = egui::FontDefinitions::default();
        let mut fallback = definitions
            .families
            .get(&egui::FontFamily::Monospace)
            .cloned()
            .unwrap_or_default();
        if name != "Default monospace" {
            let Some(path) = self.files.get(name) else {
                return false;
            };
            let Ok(meta) = std::fs::metadata(path) else {
                return false;
            };
            if meta.len() > 32 * 1024 * 1024 {
                return false;
            }
            let Ok(bytes) = std::fs::read(path) else {
                return false;
            };
            // Ignore obvious non-font files; detailed parsing is egui's responsibility.
            if !matches!(bytes.get(..4), Some(b"\0\x01\0\0" | b"OTTO" | b"true")) {
                return false;
            }
            definitions.font_data.insert(
                "selected-system-font".to_owned(),
                egui::FontData::from_owned(bytes).into(),
            );
            fallback.insert(0, "selected-system-font".to_owned());
        }
        definitions
            .families
            .insert(egui::FontFamily::Name("editor".into()), fallback);
        ctx.set_fonts(definitions);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_family_and_unknown_font() {
        let catalog = FontCatalog::default();
        let ctx = egui::Context::default();
        assert_eq!(catalog.names(), vec!["Default monospace"]);
        assert!(catalog.install(&ctx, "Default monospace"));
        assert!(!catalog.install(&ctx, "missing-font"));
        let mut output = ctx.run_ui(Default::default(), |ui| {
            ui.label(egui::RichText::new("hello").font(egui::FontId::new(
                14.0,
                egui::FontFamily::Name("editor".into()),
            )));
        });
        output.textures_delta.clear();
    }
    #[test]
    fn discovered_font_can_render_if_present() {
        let catalog = FontCatalog::discover();
        let ctx = egui::Context::default();
        if let Some(name) = catalog.names().get(1) {
            assert!(catalog.install(&ctx, name));
            let mut output = ctx.run_ui(Default::default(), |ui| {
                ui.label(egui::RichText::new("Font sample").font(egui::FontId::new(
                    14.0,
                    egui::FontFamily::Name("editor".into()),
                )));
            });
            output.textures_delta.clear();
        }
    }
}
