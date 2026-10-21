//! Small, versioned application preferences. No document contents are stored here.
use std::{
    env,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

const MAX_SETTINGS_BYTES: u64 = 16 * 1024;
const MAX_FONT_BYTES: usize = 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub font: String,
    pub font_size: f32,
    pub dark: bool,
    pub wrap: bool,
    pub zoom: f32,
    pub syntax: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font: "Default monospace".into(),
            font_size: 15.0,
            dark: false,
            wrap: true,
            zoom: 1.0,
            syntax: true,
        }
    }
}

impl Settings {
    /// Missing, inaccessible, unsupported or malformed preferences use safe defaults.
    pub fn load() -> Self {
        Self::config_path().map(Self::load_from).unwrap_or_default()
    }
    pub fn save(&self) -> io::Result<()> {
        self.save_to(Self::config_path()?)
    }
    pub fn config_path() -> io::Result<PathBuf> {
        #[cfg(target_os = "windows")]
        let base = env::var_os("APPDATA")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("USERPROFILE")
                    .filter(|p| !p.is_empty())
                    .map(|p| PathBuf::from(p).join("AppData").join("Roaming"))
            });
        #[cfg(target_os = "macos")]
        let base = env::var_os("HOME")
            .filter(|p| !p.is_empty())
            .map(|p| PathBuf::from(p).join("Library").join("Application Support"));
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let base = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                env::var_os("HOME")
                    .filter(|p| !p.is_empty())
                    .map(|p| PathBuf::from(p).join(".config"))
            });
        base.filter(|p| p.is_absolute()).map(|p| p.join("rust-notepad-prototype").join("settings.conf"))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Could not locate a configuration directory; settings remain active for this session."))
    }
    /// Explicit-path variant for portable callers and isolated tests.
    pub fn load_from(path: impl AsRef<Path>) -> Self {
        let read = || -> io::Result<String> {
            let file = File::open(path)?;
            if file.metadata()?.len() > MAX_SETTINGS_BYTES {
                return Err(invalid_settings());
            }
            let mut bytes = Vec::new();
            file.take(MAX_SETTINGS_BYTES + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_SETTINGS_BYTES {
                return Err(invalid_settings());
            }
            String::from_utf8(bytes).map_err(|_| invalid_settings())
        };
        read().map(|text| Self::parse(&text)).unwrap_or_default()
    }
    pub fn save_to(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        tmp.write_all(self.serialize().as_bytes())?;
        tmp.as_file().sync_all()?;
        tmp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }
    fn parse(text: &str) -> Self {
        let mut settings = Self::default();
        // Requiring the first nonempty line avoids partially reading future formats.
        let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
        if lines.next() != Some("version=1") {
            return settings;
        }
        for line in lines {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "font_hex" => {
                    if let Some(font) = decode_font(value) {
                        settings.font = font;
                    }
                }
                "font_size" => {
                    if let Some(value) = bounded_float(value, 8.0, 40.0) {
                        settings.font_size = value;
                    }
                }
                "zoom" => {
                    if let Some(value) = bounded_float(value, 0.5, 3.0) {
                        settings.zoom = value;
                    }
                }
                "dark" => {
                    if let Ok(value) = value.parse() {
                        settings.dark = value;
                    }
                }
                "wrap" => {
                    if let Ok(value) = value.parse() {
                        settings.wrap = value;
                    }
                }
                "syntax" => {
                    if let Ok(value) = value.parse() {
                        settings.syntax = value;
                    }
                }
                _ => {}
            }
        }
        settings
    }
    fn serialize(&self) -> String {
        let defaults = Self::default();
        let font = if valid_font(&self.font) {
            &self.font
        } else {
            &defaults.font
        };
        let font_hex: String = font.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        let font_size = if self.font_size.is_finite() && (8.0..=40.0).contains(&self.font_size) {
            self.font_size
        } else {
            defaults.font_size
        };
        let zoom = if self.zoom.is_finite() && (0.5..=3.0).contains(&self.zoom) {
            self.zoom
        } else {
            defaults.zoom
        };
        format!(
            "version=1\nfont_hex={font_hex}\nfont_size={font_size}\ndark={}\nwrap={}\nzoom={zoom}\nsyntax={}\n",
            self.dark, self.wrap, self.syntax
        )
    }
}
fn invalid_settings() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "Invalid preferences file")
}
fn bounded_float(value: &str, min: f32, max: f32) -> Option<f32> {
    value
        .parse::<f32>()
        .ok()
        .filter(|v| v.is_finite() && (min..=max).contains(v))
}
fn valid_font(font: &str) -> bool {
    !font.is_empty() && font.len() <= MAX_FONT_BYTES && !font.contains('\0')
}
fn decode_font(hex: &str) -> Option<String> {
    if hex.len() > MAX_FONT_BYTES * 2 || !hex.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = hex
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some(((high << 4) | low) as u8)
        })
        .collect();
    String::from_utf8(bytes?)
        .ok()
        .filter(|font| valid_font(font))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_and_invalid_files_use_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.conf");
        assert_eq!(Settings::load_from(&path), Settings::default());
        for content in [
            b"garbage".as_slice(),
            b"version=2\ndark=true\n",
            b"\xff\xfe",
        ] {
            fs::write(&path, content).unwrap();
            assert_eq!(Settings::load_from(&path), Settings::default());
        }
    }
    #[test]
    fn malformed_and_out_of_range_fields_keep_defaults() {
        let text = "version=1\nfont_hex=not hex\nfont_size=900\nzoom=NaN\ndark=yes\nwrap=123\nsyntax=truthy\nunknown=value\nno equals sign";
        assert_eq!(Settings::parse(text), Settings::default());
        for value in ["NaN", "inf", "-inf", "0", "-1", "41", "3e38"] {
            assert_eq!(
                Settings::parse(&format!("version=1\nfont_size={value}\n")).font_size,
                15.0
            );
        }
        assert!(Settings::parse("version=1\nfont_size=bad\ndark=true").dark);
    }
    #[test]
    fn settings_roundtrip_unicode_and_delimiters() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("settings.conf");
        let settings = Settings {
            font: "日本語 Font=Mono\nwith\rdelimiters 🦀".into(),
            font_size: 18.5,
            dark: true,
            wrap: false,
            zoom: 1.5,
            syntax: false,
        };
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), settings);
        Settings::default().save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), Settings::default());
    }
    #[test]
    fn huge_files_and_bad_font_encodings_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.conf");
        File::create(&path)
            .unwrap()
            .set_len(MAX_SETTINGS_BYTES + 1)
            .unwrap();
        assert_eq!(Settings::load_from(&path), Settings::default());
        for hex in ["", "0", "ff", "00", "xx", "é"] {
            assert_eq!(
                Settings::parse(&format!("version=1\nfont_hex={hex}")).font,
                Settings::default().font
            );
        }
    }
    #[test]
    fn invalid_in_memory_values_are_safely_serialized() {
        let settings = Settings {
            font_size: f32::NAN,
            zoom: f32::INFINITY,
            font: String::new(),
            ..Settings::default()
        };
        assert_eq!(Settings::parse(&settings.serialize()), Settings::default());
    }
    #[test]
    fn save_errors_are_reported_and_unrelated_files_survive() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("not-a-directory");
        fs::write(&existing, "keep me").unwrap();
        assert!(
            Settings::default()
                .save_to(existing.join("settings.conf"))
                .is_err()
        );
        assert_eq!(fs::read_to_string(existing).unwrap(), "keep me");
    }
}
