/* K1sh-M1sh Members - License MIT */
/* © 2026, K1sh-M1sh Members */

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod git;
mod highlight;
mod settings;
mod terminal;
mod i18n;
use i18n::{Language, Loc};

use eframe::egui::text::{CCursor, CCursorRange, LayoutJob};
use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, FontId, Key, Layout, Modifiers, RichText, Sense, TextFormat,
};
use settings::{ProjectSettings, Settings, TrustStore, SessionData};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

fn ext_of(p: &Path) -> String {
    p.extension().and_then(|e| e.to_str()).unwrap_or("txt").to_string()
}

fn name_of(p: &Path) -> String {
    p.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string()
}

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

fn byte_idx(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map(|(b, _)| b).unwrap_or(s.len())
}

fn trim_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for l in s.split_inclusive('\n') {
        let (body, nl) = if let Some(b) = l.strip_suffix("\r\n") {
            (b, "\r\n")
        } else if let Some(b) = l.strip_suffix('\n') {
            (b, "\n")
        } else {
            (l, "")
        };
        out.push_str(body.trim_end());
        out.push_str(nl);
    }
    out
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn default_shell(setting: &str) -> String {
    if !setting.trim().is_empty() {
        return setting.trim().to_string();
    }
    if cfg!(windows) {
        "powershell.exe".into()
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    }
}

fn is_dark(theme: &str) -> bool {
    !(theme.contains("light") || theme.contains("Light") || theme.contains("Inspired"))
}

fn badge_char(f: &git::GitFile) -> char {
    if f.untracked() {
        'U'
    } else if f.y != ' ' && f.y != '?' {
        f.y
    } else {
        f.x
    }
}

fn badge_color(c: char) -> Color32 {
    match c {
        'M' => Color32::from_rgb(226, 192, 141),
        'A' | 'U' => Color32::from_rgb(115, 201, 145),
        'D' => Color32::from_rgb(199, 78, 57),
        'R' => Color32::from_rgb(115, 160, 230),
        '•' => Color32::from_rgb(150, 150, 150),
        _ => Color32::from_rgb(230, 120, 80),
    }
}

fn cursor_range(ctx: &egui::Context, id: egui::Id) -> Option<(usize, usize)> {
    let st = egui::TextEdit::load_state(ctx, id)?;
    let r = st.cursor.char_range()?;
    let (a, b) = (r.primary.index, r.secondary.index);
    Some((a.min(b), a.max(b)))
}

fn replace_range(ctx: &egui::Context, id: egui::Id, text: &mut String, a: usize, b: usize, ins: &str) {
    let ba = byte_idx(text, a);
    let bb = byte_idx(text, b);
    text.replace_range(ba..bb, ins);
    if let Some(mut st) = egui::TextEdit::load_state(ctx, id) {
        let c = a + ins.chars().count();
        st.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(c))));
        st.store(ctx, id);
    }
}

struct Tab {
    uid: u64,
    path: Option<PathBuf>,
    title: String,
    text: String,
    ext: String,
    dirty: bool,
    readonly: bool,
    last_edit: Instant,
    hl: highlight::HlCache,
    gkey: u64,
    galley: Option<Arc<egui::Galley>>,
    pending_cursor: Option<usize>,
}

impl Tab {
    fn new(uid: u64, path: Option<PathBuf>, title: String, text: String, ext: String, readonly: bool) -> Self {
        Self {
            uid,
            path,
            title,
            text,
            ext,
            dirty: false,
            readonly,
            last_edit: Instant::now(),
            hl: Default::default(),
            gkey: 0,
            galley: None,
            pending_cursor: None,
        }
    }
}

enum Msg {
    Status(git::Status),
    GitDone { label: String, result: Result<String, String> },
    OpenText { title: String, text: String, ext: String },
    AutosaveDone { uid: u64, result: Result<(), String> },
}

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Explorer,
    Git,
}

struct Entry {
    path: PathBuf,
    name: String,
    is_dir: bool,
}

enum TreeAct {
    None,
    Open(PathBuf),
    New { dir: PathBuf, is_dir: bool },
}

enum GitAct {
    Op { label: String, args: Vec<String>, top: PathBuf },
    Diff { title: String, top: PathBuf, path: String, staged: bool },
    Log { top: PathBuf },
    OpenFile(PathBuf),
    Discard(git::GitFile, PathBuf),
    Refresh,
}

struct App {
    user: Settings,
    project: ProjectSettings,
    eff: Settings,
    loc: Loc,
    trust: TrustStore,
    trusted: bool,
    trust_prompt: bool,
    trust_parent: bool,
    show_settings: bool,
    settings_tab: u8,
    applied_theme: String,
    dark: bool,

    root: Option<PathBuf>,
    tabs: Vec<Tab>,
    active: usize,
    next_uid: u64,
    confirm_close: Option<usize>,
    confirm_exit: bool,
    new_entry: Option<(PathBuf, bool, String)>,
    status: String,

    ss: SyntaxSet,
    themes: ThemeSet,

    show_side: bool,
    side: Side,
    dir_cache: HashMap<PathBuf, (Instant, Arc<Vec<Entry>>)>,

    git: Option<git::Status>,
    git_busy: bool,
    git_dirty: bool,
    last_poll: Instant,
    badges: HashMap<PathBuf, char>,
    git_msg: String,
    commit_msg: String,
    new_branch: String,
    confirm_discard: Option<(git::GitFile, PathBuf)>,

    show_terminal: bool,
    terminals: Vec<terminal::Terminal>,
    term_active: usize,
    term_error: Option<String>,
    term_focused: bool,
    want_new_term: bool,
    term_counter: usize,

    show_about: bool,
    show_github: bool,
    show_issues: bool,
    show_plugins: bool,
    show_output: bool,
    output_log: String,

    all_suggestions: Vec<String>,
    autocomplete_open: bool,
    autocomplete_idx: usize,
    autocomplete_suggestions: Vec<String>,
    autocomplete_start_char: usize,

    show_find: bool,
    focus_find: bool,
    find_text: String,
    replace_text: String,
    show_goto: bool,
    focus_goto: bool,
    goto_line: usize,
    scroll_to_char: Option<usize>,

    session: SessionData,
    restoring_cursors: bool,

    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    md_cache: egui_commonmark::CommonMarkCache,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let user: Settings = settings::load_json(&settings::user_settings_path());
        let loc = Loc::load(user.language);
        let themes = ThemeSet::load_defaults();
        let mut sb = SyntaxSet::load_defaults_newlines().into_builder();
        let extra = settings::config_dir().join("syntaxes");
        if extra.is_dir() {
            let _ = sb.add_from_folder(&extra, true);
        }

        egui_extras::install_image_loaders(&cc.egui_ctx);

        let (tx, rx) = channel();
        let mut app = Self {
            eff: user.clone(),
            status: loc.ready.clone(),
            loc,
            user,
            project: ProjectSettings::default(),
            trust: TrustStore::load(),
            trusted: false,
            trust_prompt: false,
            trust_parent: false,
            show_settings: false,
            settings_tab: 0,
            applied_theme: String::new(),
            dark: true,
            root: None,
            tabs: vec![],
            active: 0,
            next_uid: 1,
            confirm_close: None,
            confirm_exit: false,
            new_entry: None,
            ss: sb.build(),
            themes,
            show_side: true,
            side: Side::Explorer,
            dir_cache: HashMap::new(),
            git: None,
            git_busy: false,
            git_dirty: true,
            last_poll: Instant::now(),
            badges: HashMap::new(),
            git_msg: String::new(),
            commit_msg: String::new(),
            new_branch: String::new(),
            confirm_discard: None,
            show_terminal: false,
            terminals: vec![],
            term_active: 0,
            term_error: None,
            term_focused: false,
            want_new_term: false,
            term_counter: 0,

            show_about: false,
            show_github: false,
            show_issues: false,
            show_plugins: false,
            show_output: false,
            output_log: String::new(),

            all_suggestions: Vec::new(),
            autocomplete_open: false,
            autocomplete_idx: 0,
            autocomplete_suggestions: Vec::new(),
            autocomplete_start_char: 0,

            show_find: false,
            focus_find: false,
            find_text: String::new(),
            replace_text: String::new(),
            show_goto: false,
            focus_goto: false,
            goto_line: 1,
            scroll_to_char: None,

            session: SessionData::load(),
            restoring_cursors: true,
            tx,
            rx,
            md_cache: egui_commonmark::CommonMarkCache::default(),
        };

        if let Some(root) = app.session.root.clone() {
            if root.exists() {
                app.set_root(root);
                let tabs_to_open = app.session.tabs.clone();
                for tab_path in tabs_to_open {
                    app.open_file(tab_path);
                }
                if app.session.active < app.tabs.len() {
                    app.active = app.session.active;
                }
            }
        }
        app

    }

    fn recompute(&mut self) {
        self.eff = if self.trusted { self.user.merged(&self.project) } else { self.user.clone() };
        self.dir_cache.clear();
    }

    fn load_project(&mut self) {
        self.project = match (&self.root, self.trusted) {
            (Some(r), true) => settings::load_json(&settings::project_settings_path(r)),
            _ => ProjectSettings::default(),
        };
    }

    fn set_root(&mut self, p: PathBuf) {
        self.terminals.clear();
        self.term_error = None;
        self.git = None;
        self.badges.clear();
        self.dir_cache.clear();
        self.git_msg.clear();
        self.trusted = self.trust.is_trusted(&p);
        self.trust_prompt = !self.trusted;
        self.root = Some(p);
        self.load_project();
        self.recompute();
        self.git_dirty = true;
        self.show_side = true;
        self.side = Side::Explorer;
    }

    fn apply_theme(&mut self, ctx: &egui::Context) {
        let mut style = (*ctx.style()).clone();
        style.visuals.widgets.noninteractive.rounding = egui::Rounding::same(6.0);
        style.visuals.widgets.inactive.rounding = egui::Rounding::same(6.0);
        style.visuals.widgets.hovered.rounding = egui::Rounding::same(6.0);
        style.visuals.widgets.active.rounding = egui::Rounding::same(6.0);
        style.visuals.window_rounding = egui::Rounding::same(10.0);
        style.animation_time = 0.15;
        ctx.set_style(style);

        if self.applied_theme != self.eff.theme {
            self.dark = is_dark(&self.eff.theme);
            ctx.set_visuals(if self.dark { egui::Visuals::dark() } else { egui::Visuals::light() });
            self.applied_theme = self.eff.theme.clone();
        }
    }

    fn open_file(&mut self, path: PathBuf) {
        let c = canon(&path);
        if let Some(i) = self.tabs.iter().position(|t| !t.readonly && t.path.as_ref().map(|p| canon(p)) == Some(c.clone())) {
            self.active = i;
            return;
        }
        if let Ok(md) = std::fs::metadata(&path) {
            if md.len() > 20 * 1024 * 1024 {
                self.status = self.loc.big_file_error.clone();
                return;
            }
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let uid = self.next_uid;
                self.next_uid += 1;
                self.status = format!("{} {}", &self.loc.open_file, path.display());
                let readonly = !self.trusted;

                let mut tab = Tab::new(uid, Some(path.clone()), name_of(&path), text, ext_of(&path), readonly);

                // Запоминаем, куда нужно поставить курсор при отрисовке
                if let Some(&char_idx) = self.session.cursors.get(&path.to_string_lossy().to_string()) {
                    tab.pending_cursor = Some(char_idx);
                }

                self.tabs.push(tab);
                self.active = self.tabs.len() - 1;
                self.update_suggestions();
            }
            Err(e) => self.status = format!("{} {e}", &self.loc.err_to_open),
        }
    }

    fn open_virtual(&mut self, title: String, text: String, ext: String) {
        if let Some(i) = self.tabs.iter().position(|t| t.readonly && t.title == title) {
            let t = &mut self.tabs[i];
            t.text = text;
            t.gkey = 0;
            self.active = i;
            return;
        }
        let uid = self.next_uid;
        self.next_uid += 1;
        self.tabs.push(Tab::new(uid, None, title, text, ext, true));
        self.active = self.tabs.len() - 1;
    }

    fn save_all_tabs(&mut self) {
        for i in 0..self.tabs.len() {
            if self.tabs[i].dirty && !self.tabs[i].readonly {
                self.save_tab(i);
            }
        }
        self.status = self.loc.save_all.clone(); // Или любой другой статус
    }

    fn save_tab(&mut self, idx: usize) {
        if !self.trusted {
            self.status = self.loc.cannot_be_save.clone();
            return;
        }
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        if tab.readonly {
            return;
        }
        if tab.path.is_none() {
            match rfd::FileDialog::new().save_file() {
                Some(p) => {
                    tab.ext = ext_of(&p);
                    tab.title = name_of(&p);
                    tab.path = Some(p);
                }
                None => return,
            }
        }
        if self.eff.trim_trailing_whitespace {
            tab.text = trim_ws(&tab.text);
        }
        let p = tab.path.clone().unwrap();
        match std::fs::write(&p, &tab.text) {
            Ok(_) => {
                tab.dirty = false;
                self.status = format!("{} {}", &self.loc.save, p.display());
                self.git_dirty = true;
                self.update_suggestions();
            }
            Err(e) => self.status = format!("{} {e}", &self.loc.err_save),
        }
    }

    fn close_tab(&mut self, i: usize) {
        if i >= self.tabs.len() { return; }
        self.tabs.remove(i);
        if self.tabs.is_empty() {
            self.active = 0;
        } else {
            if i < self.active { self.active -= 1; }
            self.active = self.active.min(self.tabs.len() - 1);
        }
    }

    fn request_close(&mut self, i: usize) {
        match self.tabs.get(i) {
            Some(t) if t.dirty && !t.readonly => self.confirm_close = Some(i),
            Some(_) => self.close_tab(i),
            None => {}
        }
    }

    fn autosave(&mut self) {
        if !self.eff.auto_save || !self.trusted { return; }

        let idxs: Vec<usize> = self.tabs.iter().enumerate()
            .filter(|(_, t)| t.dirty && !t.readonly && t.path.is_some() && t.last_edit.elapsed() > Duration::from_millis(1200))
            .map(|(i, _)| i).collect();

        for i in idxs {
            let tab = &mut self.tabs[i];
            let Some(path) = &tab.path else { continue };
            let path = path.clone();
            let text = tab.text.clone();
            let uid = tab.uid;
            let tx = self.tx.clone();

            // Снимаем флаг dirty сразу, чтобы не запускать поток каждую секунду.
            // Если произойдет ошибка, мы вернем его обратно в pump.
            tab.dirty = false;

            thread::spawn(move || {
                let res = std::fs::write(&path, text).map_err(|e| e.to_string());
                let _ = tx.send(Msg::AutosaveDone { uid, result: res });
            });
        }
    }

    fn create_entry(&mut self, dir: PathBuf, is_dir: bool, name: &str) {
        if !self.trusted {
            self.status = self.loc.err_create_a_file.clone();
            return;
        }
        let name = name.trim();
        if name.is_empty() || Path::new(name).is_absolute() || name.contains("..") {
            self.status = self.loc.uncorrect_name.clone();
            return;
        }
        let p = dir.join(name);
        let res = if is_dir {
            std::fs::create_dir_all(&p)
        } else {
            p.parent().map(std::fs::create_dir_all).transpose()
                .and_then(|_| std::fs::OpenOptions::new().write(true).create_new(true).open(&p).map(|_| ()))
        };
        match res {
            Ok(_) => {
                self.dir_cache.clear();
                self.git_dirty = true;
                self.update_suggestions();
                if !is_dir { self.open_file(p); }
            }
            Err(e) => self.status = format!("{} {e}", &self.loc.err_to_create),
        }
    }

    fn update_suggestions(&mut self) {
        self.all_suggestions.clear();
        let mut seen = std::collections::HashSet::new();

        for tab in &self.tabs {
            for word in tab.text.split(|c: char| !c.is_alphanumeric() && c != '_') {
                if word.len() > 2 && !seen.contains(word) {
                    seen.insert(word.to_string());
                    self.all_suggestions.push(word.to_string());
                }
            }
        }

        if let Some(root) = &self.root {
            if let Ok(entries) = std::fs::read_dir(root) {
                for entry in entries.flatten() {
                    if let Some(name) = entry.file_name().to_str() {
                        let word = name.split('.').next().unwrap_or(name);
                        if word.len() > 2 && !seen.contains(word) {
                            seen.insert(word.to_string());
                            self.all_suggestions.push(word.to_string());
                        }
                    }
                }
            }
        }
        self.all_suggestions.sort();
        self.all_suggestions.truncate(200);
    }

    fn run_command(&mut self, cmd_str: &str, is_build: bool) {
        if !self.trusted {
            self.status = self.loc.err_to_start.clone();
            return;
        }
        let Some(root) = &self.root else { return };

        let cmd = if cmd_str.is_empty() {
            let ext = self.tabs.get(self.active).map(|t| t.ext.as_str()).unwrap_or("");
            if ext == "rs" { if is_build { "cargo build".into() } else { "cargo run".into() } }
            else if ext == "py" { "python".into() }
            else if ext == "go" { "go run .".into() }
            else if ext == "c" || ext == "cpp" { "gcc main.c -o main && ./main".into() }
            else {
                self.status = self.loc.err_unknown_type.clone();
                return;
            }
        } else {
            cmd_str.to_string()
        };

        self.output_log.clear();
        self.output_log.push_str(&format!("> {}\n", cmd));
        self.show_output = true;

        let output = if cfg!(windows) {
            std::process::Command::new("cmd").args(&["/C", &cmd]).current_dir(root).output()
        } else {
            std::process::Command::new("sh").args(&["-c", &cmd]).current_dir(root).output()
        };

        match output {
            Ok(out) => {
                self.output_log.push_str(&String::from_utf8_lossy(&out.stdout));
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !stderr.is_empty() {
                    self.output_log.push_str("\n--- STDERR ---\n");
                    self.output_log.push_str(&stderr);
                }
                self.status = if out.status.success() { self.loc.successfully_start.clone() } else { self.loc.unsuccessfully_start.clone() };
            }
            Err(e) => {
                self.output_log.push_str(&format!("{}: {}\n", &self.loc.unsuccessfully_start, e));
                self.status = self.loc.unsuccessfully_start.clone();
            }
        }
    }

    fn bg<F: FnOnce(Sender<Msg>) + Send + 'static>(&self, ctx: &egui::Context, f: F) {
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            f(tx);
            ctx.request_repaint();
        });
    }

    fn pump(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Status(s) => {
                    self.git_busy = false;
                    self.set_git(s);
                }
                Msg::GitDone { label, result } => {
                    match result {
                        Ok(s) => {
                            if label == "commit" { self.commit_msg.clear(); }
                            self.git_msg = format!("✔ {label}\n{}", s.trim());
                            self.status = format!("{}", &self.loc.git_ready);
                        }
                        Err(e) => {
                            self.git_msg = format!("✖ {label}\n{}", e.trim());
                            self.status = format!("{}", &self.loc.git_error);
                        }
                    }
                }
                Msg::AutosaveDone { uid, result } => {
                    if let Err(e) = result {
                        // Если не удалось сохранить — возвращаем пометку "грязный", чтобы попробовать снова
                        if let Some(tab) = self.tabs.iter_mut().find(|t| t.uid == uid) {
                            tab.dirty = true;
                        }
                        self.status = format!("{} {e}", &self.loc.error_autosave);
                    } else {
                        self.status = format!("{}", &self.loc.save);
                    }
                }
                Msg::OpenText { title, text, ext } => self.open_virtual(title, text, ext),
            }
        }
    }

    fn set_git(&mut self, s: git::Status) {
        self.badges.clear();
        if let (git::Status::Repo(st), Some(root)) = (&s, &self.root) {
            for f in &st.files {
                let Some(rel) = f.path.strip_prefix(st.prefix.as_str()) else { continue };
                let p = root.join(rel.trim_end_matches('/'));
                let mut a = p.parent();
                while let Some(d) = a {
                    if d == root.as_path() { break; }
                    self.badges.entry(d.to_path_buf()).or_insert('•');
                    a = d.parent();
                }
                self.badges.insert(p, badge_char(f));
            }
        }
        self.git = Some(s);
    }

    fn poll_git(&mut self, ctx: &egui::Context) {
        if !self.trusted || self.git_busy { return; }
        let Some(root) = self.root.clone() else { return };
        let mult = if matches!(self.git, Some(git::Status::Repo(_))) { 1 } else { 5 };
        let due = self.last_poll.elapsed() >= Duration::from_secs(self.eff.git_poll_secs.max(1) * mult);
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        if !(self.git_dirty || (due && focused)) { return; }
        self.git_busy = true;
        self.git_dirty = false;
        self.last_poll = Instant::now();
        self.bg(ctx, move |tx| {
            let _ = tx.send(Msg::Status(git::status(&root)));
        });
    }

    fn run_git_act(&mut self, ctx: &egui::Context, a: GitAct) {
        let Some(root) = self.root.clone() else { return };
        match a {
            GitAct::Refresh => self.git_dirty = true,
            GitAct::OpenFile(p) => self.open_file(p),
            GitAct::Discard(f, top) => self.confirm_discard = Some((f, top)),
            GitAct::Op { label, args, top } => {
                self.git_busy = true;
                self.bg(ctx, move |tx| {
                    let a: Vec<&str> = args.iter().map(String::as_str).collect();
                    let r = git::run(&top, &a);
                    let _ = tx.send(Msg::GitDone { label, result: r });
                    let _ = tx.send(Msg::Status(git::status(&root)));
                });
            }
            GitAct::Diff { title, top, path, staged } => {
                self.bg(ctx, move |tx| {
                    let mut a = vec!["diff"];
                    if staged { a.push("--cached"); }
                    a.push("--"); a.push(&path);
                    let text = git::run(&top, &a).unwrap_or_else(|e| e);
                    let text = if text.trim().is_empty() { "(no changes)".to_string() } else { text };
                    let _ = tx.send(Msg::OpenText { title, text, ext: "diff".into() });
                });
            }
            GitAct::Log { top } => {
                self.bg(ctx, move |tx| {
                    let text = git::run(&top, &["log", "--oneline", "--decorate", "-n", "80"]).unwrap_or_else(|e| e);
                    let _ = tx.send(Msg::OpenText { title: "git log".into(), text, ext: "txt".into() });
                });
            }
        }
    }

    fn new_terminal(&mut self, ctx: &egui::Context) {
        let cwd = self.root.clone().unwrap_or_else(home_dir);
        let shell = default_shell(&self.eff.terminal_shell);
        self.term_counter += 1;
        let title = format!("{} {}", name_of(Path::new(&shell)), self.term_counter);
        match terminal::Terminal::spawn(ctx, &shell, &cwd, title) {
            Ok(t) => {
                self.terminals.push(t);
                self.term_active = self.terminals.len() - 1;
                self.term_error = None;
            }
            Err(e) => self.term_error = Some(format!("{} {e}", &self.loc.err_start_terminal)),
        }
    }

    fn excluded(&self, name: &str) -> bool {
        self.eff.explorer_exclude.iter().any(|pat| {
            if let Some(suf) = pat.strip_prefix('*') { name.ends_with(suf) } else { name == pat }
        })
    }

    fn list_dir(&mut self, dir: &Path) -> Arc<Vec<Entry>> {
        if let Some((t, v)) = self.dir_cache.get(dir) {
            if t.elapsed() < Duration::from_secs(3) { return v.clone(); }
        }
        let mut v = vec![];
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if self.excluded(&name) { continue; }
                let path = e.path();
                let is_dir = path.is_dir();
                v.push(Entry { path, name, is_dir });
            }
        }
        v.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        let v = Arc::new(v);
        self.dir_cache.insert(dir.to_path_buf(), (Instant::now(), v.clone()));
        v
    }

    fn tree(&mut self, ui: &mut egui::Ui, dir: &Path, act: &mut TreeAct) {
        let entries = self.list_dir(dir);
        for e in entries.iter() {
            let badge = self.badges.get(&e.path).copied();
            let color = badge.map(badge_color);
            if e.is_dir {
                let mut t = RichText::new(&e.name);
                if let Some(c) = color { t = t.color(c); }

                let r = egui::CollapsingHeader::new(t)
                    .id_salt(&e.path)
                    .icon(|ui, _open, response| {
                        let rect = egui::Rect::from_center_size(
                            response.rect.center(),
                            vec2(16.0, 16.0),
                        );
                        egui::Image::new(egui::include_image!("../images/folder.png"))
                            .paint_at(ui, rect);
                    })
                    .show(ui, |ui| self.tree(ui, &e.path, act));

                r.header_response.context_menu(|ui| {
                    if ui.button(&self.loc.new_file).clicked() {
                        *act = TreeAct::New { dir: e.path.clone(), is_dir: false };
                        ui.close_menu();
                    }
                    if ui.button(&self.loc.new_folder).clicked() {
                        *act = TreeAct::New { dir: e.path.clone(), is_dir: true };
                        ui.close_menu();
                    }
                });
            } else {
                let sel = self.tabs.get(self.active).and_then(|t| t.path.as_ref()) == Some(&e.path);
                ui.horizontal(|ui| {
                    ui.add(egui::Image::new(egui::include_image!("../images/file.png")).max_size(vec2(16.0, 16.0)));
                    let mut t = RichText::new(format!(" {}", e.name));
                    if let Some(c) = color { t = t.color(c); }
                    if ui.selectable_label(sel, t).clicked() {
                        *act = TreeAct::Open(e.path.clone());
                    }
                    if let Some(b) = badge {
                        ui.label(RichText::new(b.to_string()).small().color(badge_color(b)));
                    }
                });
            }
        }
    }

    fn explorer_ui(&mut self, ui: &mut egui::Ui) {
        let Some(root) = self.root.clone() else {
            ui.label(&self.loc.folder_not_open);
            if ui.button(&self.loc.open_folder).clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_folder() { self.set_root(p); }
            }
            return;
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(name_of(&root).to_uppercase()).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button(&self.loc.update).clicked() {
                    self.dir_cache.clear();
                    self.git_dirty = true;
                }
                if self.trusted {
                    if ui.small_button("+📁").on_hover_text(&self.loc.new_folder).clicked() {
                        self.new_entry = Some((root.clone(), true, String::new()));
                    }
                    if ui.small_button("+📄").on_hover_text(&self.loc.new_file).clicked() {
                        self.new_entry = Some((root.clone(), false, String::new()));
                    }
                }
            });
        });
        ui.separator();
        let mut act = TreeAct::None;
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            self.tree(ui, &root, &mut act);
        });
        match act {
            TreeAct::Open(p) => self.open_file(p),
            TreeAct::New { dir, is_dir } => self.new_entry = Some((dir, is_dir, String::new())),
            TreeAct::None => {}
        }
    }

    fn git_ui(&mut self, ui: &mut egui::Ui) {
        if !self.trusted {
            ui.label(&self.loc.git_turn_off);
            if ui.button(&self.loc.trust_folder).clicked() { self.trust_prompt = true; }
            return;
        }
        let Some(root) = self.root.clone() else {
            ui.label(&self.loc.open_a_folder);
            return;
        };
        let ctx = ui.ctx().clone();
        let mut acts: Vec<GitAct> = vec![];
        match self.git.clone() {
            None => { ui.label(&self.loc.loading); }
            Some(git::Status::NotRepo) => {
                ui.label(&self.loc.not_git_repo);
                if ui.button(&self.loc.init_repo).clicked() {
                    acts.push(GitAct::Op { label: "git init".into(), args: vec!["init".into()], top: root.clone() });
                }
            }
            Some(git::Status::Error(e)) => { ui.colored_label(Color32::LIGHT_RED, e); }
            Some(git::Status::Repo(st)) => {
                let top = st.top.clone();
                let op = |label: &str, a: &[&str]| GitAct::Op {
                    label: label.to_string(),
                    args: a.iter().map(|s| s.to_string()).collect(),
                    top: top.clone(),
                };
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("branch")
                        .selected_text(format!("{} {}", &self.loc.branch, st.branch))
                        .show_ui(ui, |ui| {
                            for b in &st.branches {
                                if ui.selectable_label(*b == st.branch, b).clicked() {
                                    acts.push(op(&format!("checkout {b}"), &["checkout", b]));
                                }
                            }
                        });
                    if ui.button(&self.loc.update).clicked() { acts.push(GitAct::Refresh); }
                });
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.new_branch).hint_text(&self.loc.new_branch).desired_width(110.0));
                    if ui.add_enabled(!self.new_branch.trim().is_empty(), egui::Button::new(&self.loc.create)).clicked() {
                        let n = self.new_branch.trim().to_string();
                        acts.push(op(&format!("checkout -b {n}"), &["checkout", "-b", &n]));
                        self.new_branch.clear();
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("Fetch").clicked() { acts.push(op("fetch", &["fetch", "--all", "--prune"])); }
                    if ui.button(format!("Pull {}", st.behind)).clicked() { acts.push(op("pull", &["pull"])); }
                    if ui.button(format!("Push {}", st.ahead)).clicked() {
                        if st.upstream.is_empty() { acts.push(op("push", &["push", "-u", "origin", &st.branch])); }
                        else { acts.push(op("push", &["push"])); }
                    }
                });
                ui.separator();
                ui.add(egui::TextEdit::multiline(&mut self.commit_msg).hint_text(&self.loc.commit_msg).desired_rows(3).desired_width(f32::INFINITY));
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.commit_msg.trim().is_empty(), egui::Button::new("✔ Commit")).clicked() {
                        let m = self.commit_msg.clone();
                        acts.push(op("commit", &["commit", "-m", &m]));
                    }
                    if ui.button("Stage all").clicked() { acts.push(op("add -A", &["add", "-A"])); }
                    if ui.button(&self.loc.history).clicked() { acts.push(GitAct::Log { top: top.clone() }); }
                });
                ui.separator();

                let staged: Vec<&git::GitFile> = st.files.iter().filter(|f| f.staged()).collect();
                let changes: Vec<&git::GitFile> = st.files.iter().filter(|f| f.unstaged()).collect();
                egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(ui.available_height() - 90.0).show(ui, |ui| {
                    egui::CollapsingHeader::new(format!("{} ({})", &self.loc.in_index, staged.len())).default_open(true).show(ui, |ui| {
                        for f in &staged {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(f.x.to_string()).color(badge_color(f.x)).monospace());
                                if ui.selectable_label(false, &f.path).clicked() {
                                    acts.push(GitAct::Diff { title: format!("Δ {} (index)", f.path), top: top.clone(), path: f.path.clone(), staged: true });
                                }
                                if ui.small_button("-").on_hover_text(&self.loc.remove_from_index).clicked() {
                                    acts.push(op("unstage", &["restore", "--staged", "--", &f.path]));
                                }
                            });
                        }
                    });
                    egui::CollapsingHeader::new(format!("{} ({})", &self.loc.changes, changes.len())).default_open(true).show(ui, |ui| {
                        for f in &changes {
                            ui.horizontal(|ui| {
                                let b = badge_char(f);
                                ui.label(RichText::new(b.to_string()).color(badge_color(b)).monospace());
                                if ui.selectable_label(false, &f.path).clicked() {
                                    if f.untracked() { acts.push(GitAct::OpenFile(top.join(&f.path))); }
                                    else { acts.push(GitAct::Diff { title: format!("Δ {}", f.path), top: top.clone(), path: f.path.clone(), staged: false }); }
                                }
                                if ui.small_button("+").on_hover_text(&self.loc.in_index).clicked() { acts.push(op("add", &["add", "--", &f.path])); }
                                if ui.small_button("<-").on_hover_text(&self.loc.cancel_changes).clicked() { acts.push(GitAct::Discard((*f).clone(), top.clone())); }
                            });
                        }
                    });
                });
                if !self.git_msg.is_empty() {
                    ui.separator();
                    egui::ScrollArea::vertical().max_height(70.0).id_salt("gitmsg").show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(&self.git_msg).monospace().small()).selectable(true));
                    });
                }
            }
        }
        for a in acts { self.run_git_act(&ctx, a); }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Backtick)) {
            self.show_terminal = !self.show_terminal;
        }
        if self.term_focused { return; }

        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F1)) { self.show_about = true; }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F8)) { self.show_github = true; }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F2)) { self.show_settings = true; }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F5)) {
            let run_cmd = self.project.run_command.clone().unwrap_or_default();
            self.run_command(&run_cmd, false);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F7)) {
            let build_cmd = self.project.build_command.clone().unwrap_or_default();
            self.run_command(&build_cmd, true);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::S)) {
            self.save_all_tabs();
        }

        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::F)) {
            self.show_find = true;
            self.focus_find = true;
            if let Some(tab) = self.tabs.get(self.active) {
                let id = egui::Id::new(("editor", tab.uid));
                if let Some((a, b)) = cursor_range(ctx, id) {
                    if a != b {
                        let slice = &tab.text[byte_idx(&tab.text, a)..byte_idx(&tab.text, b)];
                        if !slice.contains('\n') {
                            self.find_text = slice.to_string();
                        }
                    }
                }
            }
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::G)) {
            self.show_goto = true;
            self.focus_goto = true;
        }

        let cs = Modifiers::COMMAND.plus(Modifiers::SHIFT);
        if ctx.input_mut(|i| i.consume_key(cs, Key::G)) { self.show_side = true; self.side = Side::Git; }
        if ctx.input_mut(|i| i.consume_key(cs, Key::E)) { self.show_side = true; self.side = Side::Explorer; }
        if ctx.input_mut(|i| i.consume_key(cs, Key::O)) {
            if let Some(p) = rfd::FileDialog::new().pick_folder() { self.set_root(p); }
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::S)) { self.save_tab(self.active); }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::O)) {
            if let Some(p) = rfd::FileDialog::new().pick_file() { self.open_file(p); }
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::W)) { self.request_close(self.active); }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::B)) { self.show_side = !self.show_side; }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Comma)) { self.show_settings = true; }
    }

    fn ui_title_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("title_bar")
            .frame(egui::Frame::none().inner_margin(egui::Margin::symmetric(8.0, 4.0)).fill(ctx.style().visuals.widgets.noninteractive.bg_fill))
            .show(ctx, |ui| {
                let title_bar_rect = ui.max_rect();
                let response = ui.interact(title_bar_rect, egui::Id::new("title_bar_drag"), Sense::click_and_drag());

                if response.drag_started() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if response.double_clicked() {
                    let is_max = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!is_max));
                }

                ui.horizontal(|ui| {
                    ui.add(egui::Image::new(egui::include_image!("../images/icon.png")).max_size(vec2(16.0, 16.0)));

                    let project_name = self.root.as_ref().map(|p| name_of(p)).unwrap_or_else(|| self.loc.unnamed.clone());
                    let title = if !self.trusted && self.root.is_some() {
                        format!("RustCode || {} || {}", project_name, &self.loc.limited_mode)
                    } else if self.root.is_some() {
                        format!("RustCode || {}", project_name)
                    } else {
                        "RustCode".to_string()
                    };

                    ui.label(RichText::new(title).strong());

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("✖").clicked() { ctx.send_viewport_cmd(egui::ViewportCommand::Close); }
                        if ui.button("🗖").clicked() {
                            let is_max = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!is_max));
                        }
                        if ui.button("🗕").clicked() { ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true)); }
                    });
                });
            });
    }

    fn ui_menu(&mut self, ctx: &egui::Context) {
        let loc = self.loc.clone();
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button(&loc.file, |ui| {
                    if ui.button(&self.loc.new_file).clicked() {
                        let uid = self.next_uid;
                        self.next_uid += 1;
                        self.tabs.push(Tab::new(uid, None, "untitled".into(), String::new(), "txt".into(), !self.trusted));
                        self.active = self.tabs.len() - 1;
                        ui.close_menu();
                    }
                    if ui.button(&self.loc.open_file_button_an_file).clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_file() { self.open_file(p); }
                        ui.close_menu();
                    }
                    if ui.button(&self.loc.open_folder).clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() { self.set_root(p); }
                        ui.close_menu();
                    }
                    if ui.button(&self.loc.save_button_an_file).clicked() { self.save_tab(self.active); ui.close_menu(); }
                    if ui.button(&self.loc.save_all).clicked() { self.save_all_tabs(); ui.close_menu(); }
                    ui.separator();
                    if ui.button(&self.loc.btn_settings).clicked() { self.show_settings = true; ui.close_menu(); }
                    if ui.button(&self.loc.btn_store).clicked() { self.show_plugins = true; ui.close_menu(); }
                });
                ui.menu_button(&self.loc.view, |ui| {
                    if ui.button(&loc.sidebar).clicked() { self.show_side = !self.show_side; ui.close_menu(); }
                    if ui.button(&self.loc.terminal).clicked() { self.show_terminal = !self.show_terminal; ui.close_menu(); }
                    if ui.button(&self.loc.output).clicked() { self.show_output = !self.show_output; ui.close_menu(); }
                });
                ui.menu_button(&loc.start_bar, |ui| {
                    if ui.button(&self.loc.start_btn).clicked() {
                        let run_cmd = self.project.run_command.clone().unwrap_or_default();
                        self.run_command(&run_cmd, false);
                        ui.close_menu();
                    }
                    if ui.button(&self.loc.compil).clicked() {
                        let build_cmd = self.project.build_command.clone().unwrap_or_default();
                        self.run_command(&build_cmd, true);
                        ui.close_menu();
                    }
                });
                ui.menu_button(&self.loc.reference, |ui| {
                    if ui.button(&self.loc.about).clicked() { self.show_about = true; ui.close_menu(); }
                    if ui.button("GitHub (F8)").clicked() { self.show_github = true; ui.close_menu(); }
                    if ui.button(&self.loc.report_a_bug).clicked() { self.show_issues = true; ui.close_menu(); }
                });
            });
        });
    }

    fn ui_status(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if !self.trusted && self.root.is_some() {
                    if ui.button(RichText::new(&self.loc.limited_mode).color(Color32::from_rgb(230, 160, 60))).clicked() {
                        self.trust_prompt = true;
                    }
                }
                if let Some(git::Status::Repo(s)) = &self.git {
                    let t = format!("git: {} {} {}", s.branch, s.behind, s.ahead);
                    if ui.button(t).clicked() { self.show_side = true; self.side = Side::Git; }
                }
                ui.label(&self.status);

                if self.show_output {
                    let errors = self.output_log.lines().filter(|l| l.contains("error")).count();
                    let warnings = self.output_log.lines().filter(|l| l.contains("warning")).count();

                    if errors > 0 {
                        ui.add(
                            egui::Image::new(egui::include_image!("../images/error.png"))
                                .fit_to_exact_size(egui::vec2(16.0, 16.0)),
                        );
                        ui.colored_label(Color32::LIGHT_RED, format!("{errors} err"));
                    }

                    if warnings > 0 {
                        ui.add(
                            egui::Image::new(egui::include_image!("../images/warning.png"))
                                .fit_to_exact_size(egui::vec2(16.0, 16.0)),
                        );
                        ui.colored_label(Color32::YELLOW, format!("{warnings} warn"));
                    }

                    if errors == 0 && warnings == 0 {
                        ui.add(
                            egui::Image::new(egui::include_image!("../images/done.png"))
                                .fit_to_exact_size(egui::vec2(16.0, 16.0)),
                        );
                        ui.colored_label(Color32::GREEN, &self.loc.well_done);
                    }
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("⚙").clicked() { self.show_settings = true; }
                    let ind = if self.eff.insert_spaces { format!("{} {}", self.loc.spaces, self.eff.tab_size) } else { "Tab".to_string() };
                    ui.label(format!("{ind}   UTF-8"));
                });
            });
        });
    }

    fn ui_terminal(&mut self, ctx: &egui::Context) {
        if self.show_output {
            egui::TopBottomPanel::bottom("output_panel")
                .resizable(true)
                .default_height(150.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&self.loc.output_text).small().strong());
                        if ui.small_button(&self.loc.clearning_btn).clicked() { self.output_log.clear(); }
                        if ui.small_button(&self.loc.hide_btn).clicked() { self.show_output = false; }
                    });
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(&self.output_log).monospace().small()).selectable(true));
                    });
                });
        }

        if !self.show_terminal {
            self.term_focused = false;
            return;
        }
        if self.trusted && self.terminals.is_empty() && self.term_error.is_none() {
            self.new_terminal(ctx);
        }
        egui::TopBottomPanel::bottom("terminal")
            .resizable(true)
            .default_height(260.0)
            .min_height(100.0)
            .show(ctx, |ui| {
                let mut close = None;
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&self.loc.terminal_text).small().strong());
                    for (i, t) in self.terminals.iter().enumerate() {
                        if ui.selectable_label(i == self.term_active, &t.title).clicked() { self.term_active = i; }
                        if ui.small_button("×").clicked() { close = Some(i); }
                    }
                    if self.trusted && ui.small_button("+").clicked() { self.want_new_term = true; }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button(&self.loc.hide_btn).clicked() { self.show_terminal = false; }
                    });
                });
                if let Some(i) = close {
                    self.terminals.remove(i);
                    self.term_active = self.term_active.min(self.terminals.len().saturating_sub(1));
                    self.term_error = None;
                }
                if !self.trusted {
                    ui.label(&self.loc.err_trust_terminal);
                    if ui.button(&self.loc.trust_folder).clicked() { self.trust_prompt = true; }
                    self.term_focused = false;
                } else if let Some(e) = self.term_error.clone() {
                    ui.colored_label(Color32::LIGHT_RED, e);
                    self.term_focused = false;
                } else if let Some(t) = self.terminals.get_mut(self.term_active) {
                    let fs = self.eff.terminal_font_size;
                    self.term_focused = t.ui(ui, fs);
                } else {
                    self.term_focused = false;
                }
            });
        if self.want_new_term {
            self.want_new_term = false;
            if self.trusted {
                self.term_error = None;
                self.new_terminal(ctx);
            }
        }
    }

    fn ui_sidebar(&mut self, ctx: &egui::Context) {
        if !self.show_side { return; }
        egui::SidePanel::left("side").resizable(true).default_width(270.0).show(ctx, |ui| {
            let n = match &self.git {
                Some(git::Status::Repo(s)) => s.files.len(),
                _ => 0,
            };
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.side, Side::Explorer, &self.loc.file);
                let t = if n > 0 { format!("Git ({n})") } else { "Git".to_string() };
                ui.selectable_value(&mut self.side, Side::Git, t);
            });
            ui.separator();
            match self.side {
                Side::Explorer => self.explorer_ui(ui),
                Side::Git => self.git_ui(ui),
            }
        });
    }

    fn ui_central(&mut self, ctx: &egui::Context) {
        let bg = self.themes.themes.get(&self.eff.theme)
            .and_then(|t| t.settings.background)
            .map(|c| Color32::from_rgb(c.r, c.g, c.b))
            .unwrap_or(ctx.style().visuals.panel_fill);
        let frame = egui::Frame::none().fill(bg).inner_margin(egui::Margin::same(4.0));
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            let mut close = None;
            let mut activate = None;

            egui::ScrollArea::horizontal().id_salt("tabs_scroll").show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (i, t) in self.tabs.iter().enumerate() {
                        let label = if t.dirty { format!("{} !", t.title) } else { t.title.clone() };
                        if ui.selectable_label(i == self.active, label).clicked() { activate = Some(i); }
                        if ui.small_button("×").clicked() { close = Some(i); }
                        ui.add_space(4.0);
                    }
                });
            });
            if let Some(i) = activate { self.active = i; }
            if let Some(i) = close { self.request_close(i); }
            ui.separator();
            if self.tabs.is_empty() {
                self.welcome_ui(ui);
            } else {
                self.active = self.active.min(self.tabs.len() - 1);
                self.editor_ui(ui);
            }
        });
    }

    fn welcome_ui(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(60.0);
            ui.heading("RustCode");
            ui.label(&self.loc.welcome_cap);
            ui.add_space(16.0);
            if ui.button(&self.loc.open_folder).clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_folder() { self.set_root(p); }
            }
            if ui.button(&self.loc.open_file).clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_file() { self.open_file(p); }
            }
            ui.add_space(16.0);
            ui.label(&self.loc.welcome_text);
        });
    }

    fn editor_ui(&mut self, ui: &mut egui::Ui) {
        let eff = self.eff.clone();
        let theme: &Theme = self.themes.themes.get(&eff.theme)
            .or_else(|| self.themes.themes.get("base16-ocean.dark"))
            .unwrap_or_else(|| self.themes.themes.values().next().unwrap());
        let fg = theme.settings.foreground.map(|c| Color32::from_rgb(c.r, c.g, c.b)).unwrap_or(Color32::LIGHT_GRAY);
        let ss = &self.ss;
        let tab = &mut self.tabs[self.active];

        let font = FontId::monospace(eff.font_size);
        let (cw, rh) = ui.fonts(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));

        let Tab { uid, text, dirty, readonly, last_edit, hl, gkey, galley, ext, path, pending_cursor, .. } = tab;
        let id = egui::Id::new(("editor", *uid));
        let ctx = ui.ctx().clone();

        // Восстанавливаем курсор
        if let Some(pos) = pending_cursor.take() {
            if let Some(mut st) = egui::TextEdit::load_state(&ctx, id) {
                st.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(pos))));
                st.store(&ctx, id);
            } else {
                let mut st = egui::TextEdit::load_state(&ctx, id).unwrap_or_default();
                st.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(pos))));
                st.store(&ctx, id);
            }
        }

        // Сохраняем позицию курсора
        if let Some((a, _)) = cursor_range(&ctx, id) {
            if let Some(p) = path {
                self.session.cursors.insert(p.to_string_lossy().to_string(), a);
            }
        }

        // --- Обработка автодополнения ---
        if self.autocomplete_open {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
                self.autocomplete_idx = (self.autocomplete_idx + 1) % self.autocomplete_suggestions.len();
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                if self.autocomplete_idx == 0 { self.autocomplete_idx = self.autocomplete_suggestions.len() - 1; }
                else { self.autocomplete_idx -= 1; }
            }
            let accept = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter) || i.consume_key(Modifiers::NONE, Key::Tab));
            if accept {
                if let Some(sug) = self.autocomplete_suggestions.get(self.autocomplete_idx) {
                    if let Some((a, _b)) = cursor_range(&ctx, id) {
                        replace_range(&ctx, id, text, self.autocomplete_start_char, a, sug);
                        *dirty = true;
                        *last_edit = Instant::now();
                    }
                }
                self.autocomplete_open = false;
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                self.autocomplete_open = false;
            }
        }

        // Автозакрытие скобок
        if !*readonly && ctx.memory(|m| m.has_focus(id)) {
            if let Some((a, b)) = cursor_range(&ctx, id) {
                let mut consume_event = false;
                let mut insert_text = None;
                let mut move_right = false;

                ctx.input(|i| {
                    for ev in &i.events {
                        if let egui::Event::Text(t) = ev {
                            let open = match t.as_str() { "(" => Some(")"), "{" => Some("}"), "[" => Some("]"), "<" => Some(">"), _ => None };
                            if let Some(close) = open {
                                insert_text = Some(format!("{}{}", t, close));
                                consume_event = true;
                            } else if let Some(next_c) = text.chars().nth(a) {
                                if (t == ")" && next_c == ')') || (t == "}" && next_c == '}') ||
                                   (t == "]" && next_c == ']') || (t == ">" && next_c == '>') {
                                    move_right = true;
                                    consume_event = true;
                                }
                            }
                        }
                    }
                });

                if consume_event {
                    ctx.input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Text(_))));
                    if let Some(ins) = insert_text {
                        replace_range(&ctx, id, text, a, b, &ins);
                        let mut st = egui::TextEdit::load_state(&ctx, id).unwrap_or_default();
                        st.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(a + 1))));
                        st.store(&ctx, id);
                        *dirty = true;
                    } else if move_right {
                        let mut st = egui::TextEdit::load_state(&ctx, id).unwrap_or_default();
                        st.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(a + 1))));
                        st.store(&ctx, id);
                    }
                }
            }
        }

        // Tab, Shift+Tab и Enter
        if !*readonly && ctx.memory(|m| m.has_focus(id)) && !self.autocomplete_open {
            if ctx.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::Tab)) {
                if let Some((a, b)) = cursor_range(&ctx, id) {
                    let indent_str = if eff.insert_spaces { " ".repeat(eff.tab_size.max(1)) } else { "\t".to_string() };
                    let indent_chars = indent_str.chars().count();

                    let ba = byte_idx(text, a);
                    let line_start_byte = text[..ba].rfind('\n').map(|i| i + 1).unwrap_or(0);
                    let bb = byte_idx(text, b);

                    let block = &text[line_start_byte..bb];
                    let mut new_block = String::with_capacity(block.len());
                    let mut removed_first = 0;
                    let mut total_removed = 0;

                    for (idx, line) in block.split_inclusive('\n').enumerate() {
                        if let Some(stripped) = line.strip_prefix(&indent_str) {
                            new_block.push_str(stripped);
                            if idx == 0 { removed_first = indent_chars; }
                            total_removed += indent_chars;
                        } else if line.starts_with('\t') || line.starts_with(' ') {
                            let space_count = line.chars().take(indent_chars).take_while(|c| *c == ' ' || *c == '\t').count();
                            let skip_bytes = byte_idx(line, space_count);
                            new_block.push_str(&line[skip_bytes..]);
                            if idx == 0 { removed_first = space_count; }
                            total_removed += space_count;
                        } else {
                            new_block.push_str(line);
                        }
                    }

                    if total_removed > 0 {
                        text.replace_range(line_start_byte..bb, &new_block);
                        if let Some(mut st) = egui::TextEdit::load_state(&ctx, id) {
                            let new_a = a.saturating_sub(removed_first);
                            let new_b = b.saturating_sub(total_removed);
                            if a == b {
                                st.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(new_a))));
                            } else {
                                st.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(new_a), CCursor::new(new_b))));
                            }
                            st.store(&ctx, id);
                        }
                        *dirty = true;
                        *last_edit = Instant::now();
                    }
                }
            }
            else if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab)) {
                if let Some((a, b)) = cursor_range(&ctx, id) {
                    let indent_str = if eff.insert_spaces { " ".repeat(eff.tab_size.max(1)) } else { "\t".to_string() };
                    let indent_chars = indent_str.chars().count();

                    if a != b {
                        let ba = byte_idx(text, a);
                        let line_start_byte = text[..ba].rfind('\n').map(|i| i + 1).unwrap_or(0);
                        let bb = byte_idx(text, b);

                        let block = &text[line_start_byte..bb];
                        let mut new_block = String::with_capacity(block.len() + 32);
                        let mut lines_count = 0;

                        for line in block.split_inclusive('\n') {
                            new_block.push_str(&indent_str);
                            new_block.push_str(line);
                            lines_count += 1;
                        }

                        text.replace_range(line_start_byte..bb, &new_block);

                        if let Some(mut st) = egui::TextEdit::load_state(&ctx, id) {
                            let new_a = a + indent_chars;
                            let new_b = b + indent_chars * lines_count;
                            st.cursor.set_char_range(Some(CCursorRange::two(
                                CCursor::new(new_a),
                                CCursor::new(new_b),
                            )));
                            st.store(&ctx, id);
                        }
                        *dirty = true;
                        *last_edit = Instant::now();
                    } else if eff.insert_spaces {
                        replace_range(&ctx, id, text, a, b, &indent_str);
                        *dirty = true;
                        *last_edit = Instant::now();
                    }
                }
            }
            else if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                if let Some((a, b)) = cursor_range(&ctx, id) {
                    let before = &text[..byte_idx(text, a)];
                    let line = before.rsplit('\n').next().unwrap_or("");
                    let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
                    let opens = line.trim_end().ends_with(|c: char| matches!(c, '{' | '(' | '[' | ':'));
                    let extra = if opens { if eff.insert_spaces { " ".repeat(eff.tab_size.max(1)) } else { "\t".to_string() } } else { String::new() };
                    let ins = format!("\n{indent}{extra}");
                    replace_range(&ctx, id, text, a, b, &ins);
                    *dirty = true;
                    *last_edit = Instant::now();
                }
            }
        }

        let mut layouter = |ui: &egui::Ui, t: &str, wrap: f32| -> Arc<egui::Galley> {
            let wrap = if eff.word_wrap { wrap } else { f32::INFINITY };
            let mut h = DefaultHasher::new();
            t.hash(&mut h);
            wrap.to_bits().hash(&mut h);
            eff.font_size.to_bits().hash(&mut h);
            eff.theme.hash(&mut h);
            ext.hash(&mut h);
            ui.ctx().pixels_per_point().to_bits().hash(&mut h);
            let k = h.finish();
            if *gkey == k {
                if let Some(g) = galley.as_ref() { return g.clone(); }
            }
            let mut job = if t.len() > 1_500_000 {
                let mut j = LayoutJob::default();
                j.append(t, 0.0, TextFormat { font_id: font.clone(), color: fg, ..Default::default() });
                j
            } else {
                hl.layout(ss, theme, &eff.theme, ext.as_str(), t, &font)
            };
            job.wrap.max_width = wrap;
            let g = ui.fonts(|f| f.layout_job(job));
            *gkey = k;
            *galley = Some(g.clone());
            g
        };

        let lines = text.bytes().filter(|b| *b == b'\n').count() + 1;
        let gw = lines.to_string().len().max(2) as f32 * cw + 14.0;
        let view_h = ui.available_height();
        let rows = ((view_h / rh).ceil() as usize).max(4);
        let mut changed = false;
        let ro_flag = *readonly;

        if *ext == "md" {
            // --- РЕЖИМ MARKDOWN (РАЗДЕЛЕННЫЙ ЭКРАН) ---
            ui.columns(2, |cols| {
                // ЛЕВАЯ КОЛОНКА: Редактор
                {
                    let mut ro: &str = text.as_str();
                    let buf: &mut dyn egui::TextBuffer = if ro_flag { &mut ro } else { text };

                    let sa = if eff.word_wrap { egui::ScrollArea::vertical() } else { egui::ScrollArea::both() };
                    sa.id_salt("md_editor_scroll")
                      .auto_shrink([false, false]).show(&mut cols[0], |ui| {
                        ui.horizontal_top(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            let gutter = if eff.line_numbers { Some(ui.allocate_exact_size(vec2(gw, 1.0), Sense::hover()).0) } else { None };
                            let width = if eff.word_wrap { ui.available_width() } else { f32::INFINITY };
                            let out = egui::TextEdit::multiline(buf)
                                .id(id)
                                .frame(false)
                                .lock_focus(true)
                                .desired_width(width)
                                .desired_rows(rows)
                                .layouter(&mut layouter)
                                .show(ui);
                            if out.response.changed() { changed = true; }

                            // Полосы отступов
                            if eff.indent_guides {
                                let painter = ui.painter();
                                let clip = ui.clip_rect();
                                let guide_color = fg.gamma_multiply(0.15);
                                let stroke = egui::Stroke::new(1.0_f32, guide_color);
                                for row in &out.galley.rows {
                                    let y_min = out.galley_pos.y + row.rect.min.y;
                                    let y_max = out.galley_pos.y + row.rect.max.y;
                                    if y_max < clip.min.y || y_min > clip.max.y { continue; }

                                    let mut has_text = false;
                                    for glyph in &row.glyphs {
                                        if glyph.chr != ' ' && glyph.chr != '\t' { has_text = true; break; }
                                    }
                                    if !has_text { continue; }

                                    let mut leading_spaces = 0;
                                    for glyph in &row.glyphs {
                                        if glyph.chr == ' ' { leading_spaces += 1; }
                                        else if glyph.chr == '\t' { leading_spaces += eff.tab_size; }
                                        else { break; }
                                    }
                                    let levels = leading_spaces / eff.tab_size.max(1);
                                    for l in 1..=levels {
                                        let x = out.galley_pos.x + (l * eff.tab_size) as f32 * cw;
                                        painter.vline(x, y_min..=y_max, stroke);
                                    }
                                }
                            }

                            // Номера строк
                            if let Some(gr) = gutter {
                                let painter = ui.painter();
                                let clip = ui.clip_rect();
                                let col = fg.gamma_multiply(0.45);
                                let mut n = 1usize;
                                let mut newline = true;
                                for row in &out.galley.rows {
                                    let y = out.galley_pos.y + row.rect.min.y;
                                    if newline {
                                        if y + rh >= clip.min.y && y <= clip.max.y {
                                            painter.text(pos2(gr.max.x - 6.0, y), Align2::RIGHT_TOP, n.to_string(), font.clone(), col);
                                        }
                                        n += 1;
                                    }
                                    newline = row.ends_with_newline;
                                    if y > clip.max.y { break; }
                                }
                            }
                        });
                    });
                }

                // ПРАВАЯ КОЛОНКА: Превью
                egui::ScrollArea::vertical()
                    .id_salt("md_preview_scroll")
                    .show(&mut cols[1], |ui| {
                        let text_view = text.as_str();
                        egui_commonmark::CommonMarkViewer::new()
                            .show(ui, &mut self.md_cache, text_view);
                    });
            });
        } else {
            // --- ОБЫЧНЫЙ РЕЖИМ ---
            let mut ro: &str = text.as_str();
            let buf: &mut dyn egui::TextBuffer = if ro_flag { &mut ro } else { text };

            let sa = if eff.word_wrap { egui::ScrollArea::vertical() } else { egui::ScrollArea::both() };
            sa.id_salt("normal_editor_scroll")
              .auto_shrink([false, false]).show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let gutter = if eff.line_numbers { Some(ui.allocate_exact_size(vec2(gw, 1.0), Sense::hover()).0) } else { None };
                    let width = if eff.word_wrap { ui.available_width() } else { f32::INFINITY };
                    let out = egui::TextEdit::multiline(buf)
                        .id(id)
                        .frame(false)
                        .lock_focus(true)
                        .desired_width(width)
                        .desired_rows(rows)
                        .layouter(&mut layouter)
                        .show(ui);
                    if out.response.changed() { changed = true; }

                    // Полосы отступов
                    if eff.indent_guides {
                        let painter = ui.painter();
                        let clip = ui.clip_rect();
                        let guide_color = fg.gamma_multiply(0.15);
                        let stroke = egui::Stroke::new(1.0_f32, guide_color);
                        for row in &out.galley.rows {
                            let y_min = out.galley_pos.y + row.rect.min.y;
                            let y_max = out.galley_pos.y + row.rect.max.y;
                            if y_max < clip.min.y || y_min > clip.max.y { continue; }

                            let mut has_text = false;
                            for glyph in &row.glyphs {
                                if glyph.chr != ' ' && glyph.chr != '\t' { has_text = true; break; }
                            }
                            if !has_text { continue; }

                            let mut leading_spaces = 0;
                            for glyph in &row.glyphs {
                                if glyph.chr == ' ' { leading_spaces += 1; }
                                else if glyph.chr == '\t' { leading_spaces += eff.tab_size; }
                                else { break; }
                            }
                            let levels = leading_spaces / eff.tab_size.max(1);
                            for l in 1..=levels {
                                let x = out.galley_pos.x + (l * eff.tab_size) as f32 * cw;
                                painter.vline(x, y_min..=y_max, stroke);
                            }
                        }
                    }

                    // Прокрутка
                    if let Some(target_char) = self.scroll_to_char.take() {
                        let cur = out.galley.from_ccursor(CCursor::new(target_char));
                        let cursor_rect = out.galley.pos_from_cursor(&cur);
                        let target_rect = cursor_rect
                            .translate(out.galley_pos.to_vec2())
                            .expand2(vec2(20.0, 80.0));
                        ui.scroll_to_rect(target_rect, Some(Align::Center));
                    }

                    // Номера строк
                    if let Some(gr) = gutter {
                        let painter = ui.painter();
                        let clip = ui.clip_rect();
                        let col = fg.gamma_multiply(0.45);
                        let mut n = 1usize;
                        let mut newline = true;
                        for row in &out.galley.rows {
                            let y = out.galley_pos.y + row.rect.min.y;
                            if newline {
                                if y + rh >= clip.min.y && y <= clip.max.y {
                                    painter.text(pos2(gr.max.x - 6.0, y), Align2::RIGHT_TOP, n.to_string(), font.clone(), col);
                                }
                                n += 1;
                            }
                            newline = row.ends_with_newline;
                            if y > clip.max.y { break; }
                        }
                    }
                });
            });
        }

        if changed {
            *dirty = true;
            *last_edit = Instant::now();

            if let Some((a, _)) = cursor_range(&ctx, id) {
                let byte_a = byte_idx(text, a);
                let text_before = &text[..byte_a];
                let byte_start = text_before
                    .char_indices()
                    .rev()
                    .find(|&(_, c)| !c.is_alphanumeric() && c != '_')
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);

                let current_word = &text[byte_start..byte_a];
                if current_word.len() >= 3 {
                    self.autocomplete_suggestions = self.all_suggestions.iter()
                        .filter(|s| s.starts_with(current_word) && s.as_str() != current_word)
                        .cloned().collect();
                    if !self.autocomplete_suggestions.is_empty() {
                        self.autocomplete_open = true;
                        self.autocomplete_idx = 0;
                        self.autocomplete_start_char = text[..byte_start].chars().count();
                    } else {
                        self.autocomplete_open = false;
                    }
                } else {
                    self.autocomplete_open = false;
                }
            }
        }

        // Всплывающее окно автодополнения (упрощенно, без привязки к out, чтобы не ломать)
        if self.autocomplete_open && !self.autocomplete_suggestions.is_empty() {
            if let Some((a, _)) = cursor_range(&ctx, id) {
                let mut selected_sug = None;
                egui::Area::new("autocomplete_popup".into())
                    .fixed_pos(ctx.memory(|m| m.data.get_temp::<egui::Pos2>(id.with("cursor_pos")).unwrap_or(egui::Pos2::ZERO)))
                    .order(egui::Order::Foreground)
                    .show(&ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.set_max_width(200.0);
                            ui.set_max_height(150.0);
                            egui::ScrollArea::vertical().max_height(150.0).show(ui, |ui| {
                                for (i, sug) in self.autocomplete_suggestions.iter().enumerate() {
                                    if ui.selectable_label(i == self.autocomplete_idx, sug).clicked() {
                                        selected_sug = Some(sug.clone());
                                    }
                                }
                            });
                        });
                    });

                if let Some(sug) = selected_sug {
                    replace_range(&ctx, id, text, self.autocomplete_start_char, a, &sug);
                    *dirty = true;
                    *last_edit = Instant::now();
                    self.autocomplete_open = false;
                }
            }
        }

        if self.show_find && !self.find_text.is_empty() {
            // Рамка поиска оставлена только для обычного режима (для MD превью поиск не подсвечивается)
            // чтобы не усложнять код
        }
    }

    fn ui_dialogs(&mut self, ctx: &egui::Context) {
        self.dialog_trust(ctx);
        self.dialog_settings(ctx);

        if self.show_about {
            egui::Window::new(&self.loc.about).open(&mut self.show_about).show(ctx, |ui| {
                ui.label(RichText::new("RustCode v0.4.0").strong());
                ui.label(&self.loc.about_text_1);
                ui.label(&self.loc.about_text_2);
                ui.label(&self.loc.about_text_3);
            });
        }

        if self.confirm_exit {
            let (mut save_all, mut discard_all, mut cancel) = (false, false, false);
            egui::Window::new(&self.loc.unsaved)
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(&self.loc.unsaved_text);
                    ui.horizontal(|ui| {
                        save_all = ui.button(&self.loc.save_unsaved_btn).clicked();
                        discard_all = ui.button(&self.loc.unsave_unsaved_btn).clicked();
                        cancel = ui.button(&self.loc.cancel_btn).clicked();
                    });
                });

            if save_all {
                for i in 0..self.tabs.len() {
                    if self.tabs[i].dirty && !self.tabs[i].readonly {
                        self.save_tab(i);
                    }
                }
                // Закрываемся только если все файлы успешно сохранились
                if !self.tabs.iter().any(|t| t.dirty && !t.readonly) {
                    self.confirm_exit = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            } else if discard_all {
                // Сбрасываем флаги dirty, чтобы при следующем кадре close_requested() пропустил выход
                for t in &mut self.tabs {
                    t.dirty = false;
                }
                self.confirm_exit = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else if cancel {
                self.confirm_exit = false;
            }
        }

        if self.show_github {
            self.show_github = false;

            let url = "https://github.com/K1sh-M1sh-Studio/RustCode";
            if let Err(_e) = open::that(url) {
                eprintln!("Не удалось открыть сайт GitHub");
            }
        }

        if self.show_issues {
            self.show_issues = false;
            let url = "https://github.com/K1sh-M1sh-Studio/RustCode/issues";
            if let Err(_e) = open::that(url) {
                eprintln!("Не удалось загрузить страницу!");
            }
        }

        if self.show_plugins {
            egui::Window::new(&self.loc.store_text).open(&mut self.show_plugins).show(ctx, |ui| {
                ui.label(RichText::new(&self.loc.appear_leter).strong());
            });
        }

        if self.show_find {
            egui::Window::new(&self.loc.search_replace).open(&mut self.show_find).show(ctx, |ui| {
                let mut trigger_find = false;

                ui.horizontal(|ui| {
                    ui.label(&self.loc.search);
                    let r = ui.text_edit_singleline(&mut self.find_text);
                    if self.focus_find {
                        r.request_focus();
                        self.focus_find = false;
                    }
                    // Поиск по нажатию Enter в поле "Найти"
                    if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        trigger_find = true;
                        r.request_focus(); // Возвращаем фокус обратно в поле, чтобы можно было жать Enter снова
                    }
                });

                ui.horizontal(|ui| {
                    ui.label(&self.loc.replace);
                    ui.text_edit_singleline(&mut self.replace_text);
                });

                ui.horizontal(|ui| {
                    if ui.button(&self.loc.find_below).clicked() {
                        trigger_find = true;
                    }

                    if trigger_find && !self.find_text.is_empty() {
                        if let Some(tab) = self.tabs.get(self.active) {
                            let id = egui::Id::new(("editor", tab.uid));
                            let start_char = cursor_range(ctx, id).map(|(_, end)| end).unwrap_or(0);
                            let start_byte = byte_idx(&tab.text, start_char);

                            let found_byte = tab.text[start_byte..]
                                .find(&self.find_text)
                                .map(|pos| start_byte + pos)
                                .or_else(|| tab.text.find(&self.find_text));

                            if let Some(byte_pos) = found_byte {
                                let sel_start = tab.text[..byte_pos].chars().count();
                                let sel_end = sel_start + self.find_text.chars().count();

                                if let Some(mut st) = egui::TextEdit::load_state(ctx, id) {
                                    st.cursor.set_char_range(Some(CCursorRange::two(
                                        CCursor::new(sel_end),
                                        CCursor::new(sel_start),
                                    )));
                                    st.store(ctx, id);
                                }
                                // Даем команду редактору прокрутиться к этой позиции
                                self.scroll_to_char = Some(sel_start);
                                self.status = self.loc.found.clone();
                            } else {
                                self.status = self.loc.nothing_found.clone();
                            }
                        }
                    }

                    if ui.button(&self.loc.replace_btn).clicked() && !self.find_text.is_empty() {
                        if let Some(tab) = self.tabs.get_mut(self.active) {
                            let id = egui::Id::new(("editor", tab.uid));
                            if let Some((a, b)) = cursor_range(ctx, id) {
                                let ba = byte_idx(&tab.text, a);
                                let bb = byte_idx(&tab.text, b);
                                if &tab.text[ba..bb] == self.find_text {
                                    replace_range(ctx, id, &mut tab.text, a, b, &self.replace_text);
                                    tab.dirty = true;
                                    tab.last_edit = Instant::now();
                                }
                            }
                        }
                    }

                    if ui.button(&self.loc.replace_all).clicked() && !self.find_text.is_empty() {
                        if let Some(tab) = self.tabs.get_mut(self.active) {
                            let count = tab.text.matches(&self.find_text).count();
                            if count > 0 {
                                tab.text = tab.text.replace(&self.find_text, &self.replace_text);
                                tab.dirty = true;
                                tab.last_edit = Instant::now();
                                self.status = format!("{} {count}", &self.loc.replaced);
                            }
                        }
                    }
                });
            });
        }

        if self.show_goto {
            let mut open = self.show_goto;
            let mut should_goto = false;
            egui::Window::new(&self.loc.go_to_line).open(&mut open).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(&self.loc.line);
                    let r = ui.add(egui::DragValue::new(&mut self.goto_line).range(1..=usize::MAX));
                    if self.focus_goto {
                        r.request_focus();
                        self.focus_goto = false;
                    }
                    if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        should_goto = true;
                    }
                });
                if ui.button(&self.loc.go_btn).clicked() {
                    should_goto = true;
                }
            });
            if should_goto {
                if let Some(tab) = self.tabs.get(self.active) {
                    let id = egui::Id::new(("editor", tab.uid));
                    let mut count = 1;
                    let mut byte_pos = 0;
                    for (b, c) in tab.text.char_indices() {
                        if c == '\n' { count += 1; }
                        if count == self.goto_line {
                            byte_pos = if b == 0 { 0 } else { b + 1 };
                            break;
                        }
                    }
                    let char_idx = tab.text[..byte_pos].chars().count();
                    if let Some(mut st) = egui::TextEdit::load_state(ctx, id) {
                        st.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(char_idx))));
                        st.store(ctx, id);
                        // Возвращаем фокус в редактор после перехода к строке
                        ctx.memory_mut(|m| m.request_focus(id));
                    }
                }
                self.show_goto = false;
            } else if !open {
                self.show_goto = false;
            }
        }

        if let Some(i) = self.confirm_close {
            let (mut save, mut discard, mut cancel) = (false, false, false);
            let title = self.tabs.get(i).map(|t| t.title.clone()).unwrap_or_default();
            egui::Window::new(&self.loc.unsaved).collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.label(format!("{} «{title}»?", &self.loc.unsaved_confirm));
                ui.horizontal(|ui| {
                    save = ui.button(&self.loc.save).clicked();
                    discard = ui.button(&self.loc.unsave_confirm).clicked();
                    cancel = ui.button(&self.loc.cancel_btn).clicked();
                });
            });
            if save { self.save_tab(i); if !self.tabs.get(i).map(|t| t.dirty).unwrap_or(false) { self.close_tab(i); } self.confirm_close = None; }
            else if discard { self.close_tab(i); self.confirm_close = None; }
            else if cancel { self.confirm_close = None; }
        }

        if let Some((f, top)) = self.confirm_discard.clone() {
            let (mut yes, mut no) = (false, false);
            egui::Window::new(&self.loc.cancel_changes).collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.label(format!("{} «{}»", &self.loc.unsaved_scripts_name, f.path));
                ui.horizontal(|ui| { yes = ui.button(&self.loc.btn_yes).clicked(); no = ui.button(&self.loc.cancel).clicked(); });
            });
            if yes {
                let args: Vec<String> = if f.untracked() { vec!["clean".into(), "-f".into(), "--".into(), f.path.clone()] }
                else { vec!["restore".into(), "--".into(), f.path.clone()] };
                self.confirm_discard = None;
                self.run_git_act(ctx, GitAct::Op { label: "discard".into(), args, top });
            } else if no { self.confirm_discard = None; }
        }

        let (mut create, mut cancel) = (false, false);
        if let Some((dir, is_dir, name)) = self.new_entry.as_mut() {
            let title = if *is_dir { &self.loc.new_folder } else { &self.loc.new_file };
            egui::Window::new(title).collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.label(dir.display().to_string());
                let r = ui.text_edit_singleline(name);
                r.request_focus();
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) { create = true; }
                ui.horizontal(|ui| {
                    if ui.button(&self.loc.create).clicked() { create = true; }
                    if ui.button(&self.loc.cancel_btn).clicked() { cancel = true; }
                });
            });
        }
        if create { if let Some((dir, is_dir, name)) = self.new_entry.take() { self.create_entry(dir, is_dir, &name); } }
        else if cancel { self.new_entry = None; }
    }

    fn dialog_trust(&mut self, ctx: &egui::Context) {
        if !self.trust_prompt { return; }
        let Some(root) = self.root.clone() else { self.trust_prompt = false; return; };
        let (mut yes, mut no) = (false, false);
        egui::Window::new(&self.loc.trust_folder_text).collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.label(RichText::new(root.display().to_string()).monospace());
            ui.label(&self.loc.editing);
            ui.checkbox(&mut self.trust_parent, &self.loc.checkbox_subfolders);
            ui.horizontal(|ui| {
                yes = ui.button(&self.loc.to_trust).clicked();
                no = ui.button(&self.loc.limited_mod_btn).clicked();
            });
        });
        if yes {
            let target = if self.trust_parent { root.parent().unwrap_or(&root).to_path_buf() } else { root };
            self.trust.add(&target);
            self.trusted = true;
            self.trust_prompt = false;
            self.load_project();
            self.recompute();
            self.git_dirty = true;
            for tab in &mut self.tabs { tab.readonly = false; }
        } else if no { self.trust_prompt = false; }
    }

    fn dialog_settings(&mut self, ctx: &egui::Context) {
        if !self.show_settings { return; }
        let loc = self.loc.clone();
        let mut open = true;
        let mut changed = false;
        let mut pchanged = false;
        let theme_names: Vec<String> = self.themes.themes.keys().cloned().collect();
        let has_project = self.root.is_some();

        egui::Window::new(&self.loc.settings_window).open(&mut open).default_width(460.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.settings_tab, 0, &self.loc.user);
                ui.add_enabled_ui(has_project, |ui| { ui.selectable_value(&mut self.settings_tab, 1, &self.loc.project); });
            });
            ui.separator();
            if self.settings_tab == 1 && has_project {
                let p = &mut self.project;
                pchanged |= opt_row(ui, &loc.settings_tab_size, &mut p.tab_size, 4usize, |ui, v| ui.add(egui::Slider::new(v, 1..=8)).changed());
                pchanged |= opt_row(ui, &loc.settings_insert_spaces, &mut p.insert_spaces, true, |ui, v| ui.checkbox(v, "").changed());
                pchanged |= opt_row(ui, &loc.settings_word_wrap, &mut p.word_wrap, false, |ui, v| ui.checkbox(v, "").changed());
                pchanged |= opt_row(ui, &loc.terminal_shell, &mut p.terminal_shell, String::new(), |ui, v| ui.text_edit_singleline(v).changed());

                ui.label(&loc.build_command);
                let mut build_cmd = p.build_command.clone().unwrap_or_default();
                if ui.text_edit_singleline(&mut build_cmd).changed() { p.build_command = Some(build_cmd); pchanged = true; }

                ui.label(&loc.run_command);
                let mut run_cmd = p.run_command.clone().unwrap_or_default();
                if ui.text_edit_singleline(&mut run_cmd).changed() { p.run_command = Some(run_cmd); pchanged = true; }

                if let Some(r) = &self.root {
                    ui.separator();
                    if self.trusted {
                        if ui.button(&loc.revoke_trust).clicked() {
                            self.trust.remove(r);
                            self.trusted = false;
                            for tab in &mut self.tabs { tab.readonly = true; }
                        }
                    } else {
                        if ui.button(&loc.trust_project).clicked() {
                            self.trust.add(r);
                            self.trusted = true;
                            for tab in &mut self.tabs { tab.readonly = false; }
                        }
                    }
                }
            } else {
                let s = &mut self.user;
                changed |= ui.add(egui::Slider::new(&mut s.font_size, 9.0..=32.0).text(&loc.settings_font_size)).changed();
                changed |= ui.add(egui::Slider::new(&mut s.tab_size, 1..=8).text(&loc.settings_tab_size)).changed();
                changed |= ui.checkbox(&mut s.insert_spaces, &loc.settings_insert_spaces).changed();
                changed |= ui.checkbox(&mut s.word_wrap, &loc.settings_word_wrap).changed();
                changed |= ui.checkbox(&mut s.line_numbers, &loc.settings_line_numbers).changed();
                changed |= ui.checkbox(&mut s.indent_guides, &loc.settings_indent_guides).changed();
                /*egui::ComboBox::from_label("Тема").selected_text(s.theme.clone()).show_ui(ui, |ui| {
                    for n in &theme_names { if ui.selectable_value(&mut s.theme, n.clone(), n).changed() { changed = true; } } Раньше здесь был мат
                }); */
                ui.push_id("settings_theme_combo", |ui| {
                    egui::ComboBox::from_label(&self.loc.settings_theme)
                        .selected_text(s.theme.clone())
                        .show_ui(ui, |ui| {
                            for n in &theme_names {
                                if ui.selectable_value(&mut s.theme, n.clone(), n).changed() {
                                    changed = true;
                                }
                            }
                        });
                });

                ui.push_id("settings_lang_combo", |ui| {
                    egui::ComboBox::from_label(&self.loc.settings_language)
                        .selected_text(s.language.label())
                        .show_ui(ui, |ui| {
                            if ui.selectable_value(&mut s.language, Language::Ru, Language::Ru.label()).changed() {
                                self.loc = Loc::load(Language::Ru);
                                changed = true;
                            }
                            if ui.selectable_value(&mut s.language, Language::En, Language::En.label()).changed() {
                                self.loc = Loc::load(Language::En);
                                changed = true;
                            }
                        });
                });
            }
        });
        self.show_settings = open;
        if changed || pchanged {
            self.recompute();
            settings::save_json(&settings::user_settings_path(), &self.user);
            if pchanged {
                if let Some(r) = &self.root { settings::save_json(&settings::project_settings_path(r), &self.project); }
            }
        }
    }
}

fn opt_row<T: Clone>(ui: &mut egui::Ui, label: &str, opt: &mut Option<T>, default: T, mut edit: impl FnMut(&mut egui::Ui, &mut T) -> bool) -> bool {
    let mut ch = false;
    ui.horizontal(|ui| {
        let mut on = opt.is_some();
        if ui.checkbox(&mut on, label).changed() {
            *opt = if on { Some(default.clone()) } else { None };
            ch = true;
        }
        if let Some(v) = opt.as_mut() { if edit(ui, v) { ch = true; } }
    });
    ch
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump();
        self.apply_theme(ctx);
        self.shortcuts(ctx);
        self.autosave();
        self.poll_git(ctx);

        if ctx.input(|i| i.viewport().close_requested()) {
            let has_dirty = self.tabs.iter().any(|t| t.dirty && !t.readonly);
            if has_dirty {
                // Отменяем закрытие окна и открываем диалог
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.confirm_exit = true;
            } else {
                // Если всё сохранено — сохраняем сессию и позволяем окну закрыться
                self.session.root = self.root.clone();
                self.session.tabs = self.tabs.iter().filter_map(|t| t.path.clone()).collect();
                self.session.active = self.active;
                self.session.save();
            }
        }

        self.ui_title_bar(ctx);
        self.ui_menu(ctx);
        self.ui_status(ctx);
        self.ui_terminal(ctx);
        self.ui_sidebar(ctx);
        self.ui_central(ctx);
        self.ui_dialogs(ctx);

        ctx.request_repaint_after(Duration::from_secs(1));

        if self.restoring_cursors {
            self.restoring_cursors = false;
        }
    }
}

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_decorations(false)
            .with_transparent(true)
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../images/icon.png"))
                    .unwrap_or_default(),
            ),
        persist_window: true,
        ..Default::default()
    };
    eframe::run_native(
        "RustCode",
        opts,
        Box::new(|cc| {
            let mut app = App::new(cc);
            if let Some(arg) = std::env::args().nth(1) {
                let p = PathBuf::from(arg);
                if p.is_dir() { app.set_root(p); }
                else if p.is_file() { app.open_file(p); }
            }
            Ok(Box::new(app))
        }),
    )
}