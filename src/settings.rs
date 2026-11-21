use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn config_dir() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.unwrap_or_else(|| PathBuf::from(".")).join("rustcode")
}

pub fn user_settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn project_settings_path(root: &Path) -> PathBuf {
    root.join(".rustcode").join("settings.json")
}

fn trust_path() -> PathBuf {
    config_dir().join("trusted_folders.json")
}

pub fn load_json<T: for<'a> Deserialize<'a> + Default>(p: &Path) -> T {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_json<T: Serialize>(p: &Path, v: &T) {
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(s) = serde_json::to_string_pretty(v) {
        let _ = std::fs::write(p, s);
    }
}

/// Настройки редактора (пользовательские).
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub font_size: f32,
    pub tab_size: usize,
    pub insert_spaces: bool,
    pub word_wrap: bool,
    pub line_numbers: bool,
    pub theme: String,
    pub auto_save: bool,
    pub trim_trailing_whitespace: bool,
    pub terminal_font_size: f32,
    /// Пусто = shell по умолчанию ($SHELL / powershell)
    pub terminal_shell: String,
    /// Имена или маски вида "*.o", скрываемые в проводнике
    pub explorer_exclude: Vec<String>,
    pub git_poll_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_size: 14.0,
            tab_size: 4,
            insert_spaces: true,
            word_wrap: false,
            line_numbers: true,
            theme: "base16-ocean.dark".into(),
            auto_save: false,
            trim_trailing_whitespace: false,
            terminal_font_size: 13.0,
            terminal_shell: String::new(),
            explorer_exclude: vec![".git".into(), "target".into(), "node_modules".into(), ".DS_Store".into()],
            git_poll_secs: 3,
        }
    }
}

/// Переопределения на уровне проекта: <папка>/.rustcode/settings.json
/// Применяются ТОЛЬКО в доверенной папке.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProjectSettings {
    pub tab_size: Option<usize>,
    pub insert_spaces: Option<bool>,
    pub word_wrap: Option<bool>,
    pub trim_trailing_whitespace: Option<bool>,
    pub terminal_shell: Option<String>,
    pub explorer_exclude: Option<Vec<String>>,
}

impl Settings {
    pub fn merged(&self, p: &ProjectSettings) -> Settings {
        let mut s = self.clone();
        if let Some(v) = p.tab_size {
            s.tab_size = v;
        }
        if let Some(v) = p.insert_spaces {
            s.insert_spaces = v;
        }
        if let Some(v) = p.word_wrap {
            s.word_wrap = v;
        }
        if let Some(v) = p.trim_trailing_whitespace {
            s.trim_trailing_whitespace = v;
        }
        if let Some(v) = &p.terminal_shell {
            s.terminal_shell = v.clone();
        }
        if let Some(v) = &p.explorer_exclude {
            s.explorer_exclude = v.clone();
        }
        s
    }
}

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Список доверенных папок (Workspace Trust).
#[derive(Default, Serialize, Deserialize)]
pub struct TrustStore {
    pub trusted: Vec<PathBuf>,
}

impl TrustStore {
    pub fn load() -> Self {
        load_json(&trust_path())
    }

    /// Папка доверена, если она сама или любой её родитель в списке.
    pub fn is_trusted(&self, p: &Path) -> bool {
        let c = canon(p);
        self.trusted.iter().any(|t| c.starts_with(t))
    }

    pub fn add(&mut self, p: &Path) {
        let c = canon(p);
        if !self.trusted.contains(&c) {
            self.trusted.push(c);
        }
        save_json(&trust_path(), self);
    }
}
