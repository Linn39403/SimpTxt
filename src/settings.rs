//! Small, atomic, user-local preferences. Documents and paths are never stored.
use crate::preferences::{EditorPreferences, MAX_FONT_SIZE, MIN_FONT_SIZE, Theme};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
};

const MAX_SETTINGS_BYTES: u64 = 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Settings {
    pub editor: EditorPreferences,
    pub vim: bool,
}

impl Settings {
    fn encode(self) -> String {
        format!(
            "version=1\ntheme={}\nfont_size={}\nvim={}\n",
            if self.editor.theme == Theme::Light {
                "light"
            } else {
                "dark"
            },
            self.editor.font_size,
            self.vim
        )
    }

    fn decode(text: &str) -> Result<Self, String> {
        let mut result = Self::default();
        let mut seen = 0u8;
        for line in text.lines() {
            let (key, value) = line.split_once('=').ok_or("Invalid settings line")?;
            let bit = match key {
                "version" if value == "1" => 1,
                "theme" => {
                    result.editor.theme = match value {
                        "dark" => Theme::Dark,
                        "light" => Theme::Light,
                        _ => return Err("Invalid settings theme".into()),
                    };
                    2
                }
                "font_size" => {
                    let size: f32 = value.parse().map_err(|_| "Invalid settings font size")?;
                    if !size.is_finite() || !(MIN_FONT_SIZE..=MAX_FONT_SIZE).contains(&size) {
                        return Err("Settings font size is outside the supported range".into());
                    }
                    result.editor.font_size = size;
                    4
                }
                "vim" => {
                    result.vim = value.parse().map_err(|_| "Invalid Vim setting")?;
                    8
                }
                _ => return Err("Unsupported settings version or key; file left unchanged".into()),
            };
            if seen & bit != 0 {
                return Err("Duplicate settings key".into());
            }
            seen |= bit;
        }
        if seen != 15 {
            return Err("Incomplete settings file".into());
        }
        Ok(result)
    }
}

pub(crate) fn default_path() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| Path::new(value).is_absolute())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    base.filter(|path| path.is_absolute())
        .map(|path| path.join("SimpTxt/preferences.conf"))
}

fn require_file(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err("Preferences must be a regular file, not a link or directory".into())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Cannot inspect preferences: {error}")),
    }
}

pub(crate) fn load(path: &Path) -> Result<Settings, String> {
    require_file(path)?;
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Settings::default());
        }
        Err(error) => return Err(format!("Cannot read preferences: {error}")),
    };
    let mut bytes = Vec::new();
    file.take(MAX_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Cannot read preferences: {error}"))?;
    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return Err("Preferences exceed the 4 KiB limit; file left unchanged".into());
    }
    Settings::decode(std::str::from_utf8(&bytes).map_err(|_| "Preferences are not UTF-8")?)
}

fn save(path: &Path, settings: Settings) -> Result<(), String> {
    require_file(path)?;
    let parent = path
        .parent()
        .ok_or("Preferences have no parent directory")?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Cannot create preferences directory: {error}"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("Cannot prepare preferences: {error}"))?;
    temporary
        .write_all(settings.encode().as_bytes())
        .map_err(|error| format!("Cannot write preferences: {error}"))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| format!("Cannot flush preferences: {error}"))?;
    require_file(path)?;
    temporary
        .persist(path)
        .map_err(|error| format!("Cannot replace preferences: {}", error.error))?;
    Ok(())
}

/// One worker and one latest-value slot, independent of the frame rate. A final
/// pending write is joined on exit; no documents or session paths are persisted.
pub(crate) struct Persistence {
    desired: Settings,
    pending: Arc<Mutex<Option<Settings>>>,
    wake: Option<mpsc::SyncSender<()>>,
    results: mpsc::Receiver<(Settings, Result<(), String>)>,
    saving: bool,
    worker: Option<JoinHandle<()>>,
}

impl Persistence {
    pub(crate) fn new(path: PathBuf, initial: Settings) -> Self {
        let pending = Arc::new(Mutex::new(None));
        let job = pending.clone();
        let (wake, receiver) = mpsc::sync_channel(1);
        let (send_result, results) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            while receiver.recv().is_ok() {
                let next = job.lock().expect("preferences slot poisoned").take();
                if let Some(settings) = next {
                    let _ = send_result.send((settings, save(&path, settings)));
                }
            }
        });
        Self {
            desired: initial,
            pending,
            wake: Some(wake),
            results,
            saving: false,
            worker: Some(worker),
        }
    }

    pub(crate) fn update(&mut self, desired: Settings) -> Option<String> {
        if desired != self.desired {
            self.desired = desired;
            self.saving = true;
            *self.pending.lock().expect("preferences slot poisoned") = Some(desired);
            if let Some(wake) = &self.wake {
                let _ = wake.try_send(());
            }
        }
        let mut error = None;
        for (settings, result) in self.results.try_iter() {
            if settings == self.desired {
                self.saving = false;
            }
            if let Err(message) = result {
                error = Some(message);
            }
        }
        error
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.saving
    }
}

impl Drop for Persistence {
    fn drop(&mut self) {
        self.wake.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preference_roundtrip_and_latest_value_flush_never_store_documents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/preferences.conf");
        assert_eq!(load(&path).unwrap(), Settings::default());
        assert!(!path.exists());
        let mut latest = Settings::default();
        {
            let mut persistence = Persistence::new(path.clone(), latest);
            for size in 9..=36 {
                latest.editor.font_size = size as f32;
                latest.editor.theme = Theme::Light;
                latest.vim = true;
                persistence.update(latest);
            }
        }
        assert_eq!(load(&path).unwrap(), latest);
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 4);
        assert!(!text.contains("document"));
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn malformed_oversized_and_newer_settings_are_not_accepted() {
        for text in [
            "",
            "version=2\n",
            "version=1\nversion=1\n",
            "version=1\ntheme=dark\nfont_size=NaN\nvim=false\n",
            "version=1\ntheme=dark\nfont_size=99\nvim=false\n",
        ] {
            assert!(Settings::decode(text).is_err());
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.conf");
        let content = vec![b'x'; 4097];
        fs::write(&path, &content).unwrap();
        assert!(load(&path).unwrap_err().contains("4 KiB"));
        assert_eq!(fs::read(path).unwrap(), content);
    }

    #[test]
    fn failed_preference_save_does_not_replace_existing_nonfile() {
        let dir = tempfile::tempdir().unwrap();
        assert!(save(dir.path(), Settings::default()).is_err());
        assert!(dir.path().is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn linked_preferences_are_not_read_or_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("preferences.conf");
        fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(load(&link).is_err());
        assert!(save(&link, Settings::default()).is_err());
        assert_eq!(fs::read(target).unwrap(), b"keep");
    }
}
