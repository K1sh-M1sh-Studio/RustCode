#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod git;
mod highlight;
mod settings;
mod terminal;

use eframe::egui::text::{CCursor, CCursorRange, LayoutJob};
use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, FontId, Key, Layout, Modifiers, RichText, Sense, TextFormat,
};
use settings::{ProjectSettings, Settings, TrustStore};
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

// ───────────────────────── вспомогательное ─────────────────────────

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

// Позиция курсора / правка текста в TextEdit
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

// ───────────────────────── структуры ─────────────────────────

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
        }
    }
}

enum Msg {
    Status(git::Status),
    GitDone { label: String, result: Result<String, String> },
    OpenText { title: String, text: String, ext: String },
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
    // настройки и доверие
    user: Settings,
    project: ProjectSettings,
    eff: Settings,
    trust: TrustStore,
    trusted: bool,
    trust_prompt: bool,
    trust_parent: bool,
    show_settings: bool,
    settings_tab: u8,
    applied_theme: String,
    dark: bool,

    // рабочая область
    root: Option<PathBuf>,
    tabs: Vec<Tab>,
    active: usize,
    next_uid: u64,
    confirm_close: Option<usize>,
    new_entry: Option<(PathBuf, bool, String)>,
    status: String,

    // подсветка
    ss: SyntaxSet,
    themes: ThemeSet,

    // боковая панель
    show_side: bool,
    side: Side,
    dir_cache: HashMap<PathBuf, (Instant, Arc<Vec<Entry>>)>,

    // git
    git: Option<git::Status>,
    git_busy: bool,
    git_dirty: bool,
    last_poll: Instant,
    badges: HashMap<PathBuf, char>,
    git_msg: String,
    commit_msg: String,
    new_branch: String,
    confirm_discard: Option<(git::GitFile, PathBuf)>,

    // терминал
    show_terminal: bool,
    terminals: Vec<terminal::Terminal>,
    term_active: usize,
    term_error: Option<String>,
    term_focused: bool,
    want_new_term: bool,
    term_counter: usize,

    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl App {
    fn new() -> Self {
        let user: Settings = settings::load_json(&settings::user_settings_path());
        let themes = ThemeSet::load_defaults();
        let mut sb = SyntaxSet::load_defaults_newlines().into_builder();
        let extra = settings::config_dir().join("syntaxes");
        if extra.is_dir() {
            let _ = sb.add_from_folder(&extra, true); // свои .sublime-syntax
        }
        let (tx, rx) = channel();
        Self {
            eff: user.clone(),
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
            new_entry: None,
            status: "Готово".into(),
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
            tx,
            rx,
        }
    }

    // ───────── настройки / доверие ─────────

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
        if self.applied_theme != self.eff.theme {
            self.dark = is_dark(&self.eff.theme);
            ctx.set_visuals(if self.dark { egui::Visuals::dark() } else { egui::Visuals::light() });
            self.applied_theme = self.eff.theme.clone();
        }
    }

    // ───────── файлы ─────────

    fn open_file(&mut self, path: PathBuf) {
        let c = canon(&path);
        if let Some(i) = self
            .tabs
            .iter()
            .position(|t| !t.readonly && t.path.as_ref().map(|p| canon(p)) == Some(c.clone()))
        {
            self.active = i;
            return;
        }
        if let Ok(md) = std::fs::metadata(&path) {
            if md.len() > 20 * 1024 * 1024 {
                self.status = "Файл больше 20 МБ!".into();
                return;
            }
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let uid = self.next_uid;
                self.next_uid += 1;
                self.status = format!("Открыт {}", path.display());
                self.tabs.push(Tab::new(uid, Some(path.clone()), name_of(&path), text, ext_of(&path), false));
                self.active = self.tabs.len() - 1;
            }
            Err(e) => self.status = format!("Не удалось открыть (не UTF-8 или нет доступа): {e}"),
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

    fn save_tab(&mut self, idx: usize) {
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
                self.status = format!("Сохранено {}", p.display());
                self.git_dirty = true;
            }
            Err(e) => self.status = format!("Ошибка записи: {e}"),
        }
    }

    fn close_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        self.tabs.remove(i);
        if self.tabs.is_empty() {
            self.active = 0;
        } else {
            if i < self.active {
                self.active -= 1;
            }
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
        if !self.eff.auto_save {
            return;
        }
        let idxs: Vec<usize> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                t.dirty && !t.readonly && t.path.is_some() && t.last_edit.elapsed() > Duration::from_millis(1200)
            })
            .map(|(i, _)| i)
            .collect();
        for i in idxs {
            self.save_tab(i);
        }
    }

    fn create_entry(&mut self, dir: PathBuf, is_dir: bool, name: &str) {
        let name = name.trim();
        if name.is_empty() || Path::new(name).is_absolute() || name.contains("..") {
            self.status = "Некорректное имя".into();
            return;
        }
        let p = dir.join(name);
        let res = if is_dir {
            std::fs::create_dir_all(&p)
        } else {
            p.parent()
                .map(std::fs::create_dir_all)
                .transpose()
                .and_then(|_| std::fs::OpenOptions::new().write(true).create_new(true).open(&p).map(|_| ()))
        };
        match res {
            Ok(_) => {
                self.dir_cache.clear();
                self.git_dirty = true;
                if !is_dir {
                    self.open_file(p);
                }
            }
            Err(e) => self.status = format!("Не удалось создать: {e}"),
        }
    }

    // ───────── фоновые задачи / git ─────────

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
                            if label == "commit" {
                                self.commit_msg.clear();
                            }
                            self.git_msg = format!("✔ {label}\n{}", s.trim());
                            self.status = format!("git: {label} — готово");
                        }
                        Err(e) => {
                            self.git_msg = format!("✖ {label}\n{}", e.trim());
                            self.status = format!("git: {label} — ошибка");
                        }
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
                    if d == root.as_path() {
                        break;
                    }
                    self.badges.entry(d.to_path_buf()).or_insert('•');
                    a = d.parent();
                }
                self.badges.insert(p, badge_char(f));
            }
        }
        self.git = Some(s);
    }

    fn poll_git(&mut self, ctx: &egui::Context) {
        if !self.trusted || self.git_busy {
            return;
        }
        let Some(root) = self.root.clone() else { return };
        let mult = if matches!(self.git, Some(git::Status::Repo(_))) { 1 } else { 5 };
        let due = self.last_poll.elapsed() >= Duration::from_secs(self.eff.git_poll_secs.max(1) * mult);
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        if !(self.git_dirty || (due && focused)) {
            return;
        }
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
                    if staged {
                        a.push("--cached");
                    }
                    a.push("--");
                    a.push(&path);
                    let text = git::run(&top, &a).unwrap_or_else(|e| e);
                    let text = if text.trim().is_empty() { "(нет изменений)".to_string() } else { text };
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

    // ───────── терминал ─────────

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
            Err(e) => self.term_error = Some(format!("Не удалось запустить терминал: {e}")),
        }
    }

    // ───────── проводник ─────────

    fn excluded(&self, name: &str) -> bool {
        self.eff.explorer_exclude.iter().any(|pat| {
            if let Some(suf) = pat.strip_prefix('*') {
                name.ends_with(suf)
            } else {
                name == pat
            }
        })
    }

    /// Листинг папки с кэшем (3 с) — диск не читается каждый кадр.
    fn list_dir(&mut self, dir: &Path) -> Arc<Vec<Entry>> {
        if let Some((t, v)) = self.dir_cache.get(dir) {
            if t.elapsed() < Duration::from_secs(3) {
                return v.clone();
            }
        }
        let mut v = vec![];
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if self.excluded(&name) {
                    continue;
                }
                let path = e.path();
                let is_dir = path.is_dir();
                v.push(Entry { path, name, is_dir });
            }
        }
        v.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
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
                let mut t = RichText::new(format!("📁 {}", e.name));
                if let Some(c) = color {
                    t = t.color(c);
                }
                let r = egui::CollapsingHeader::new(t)
                    .id_salt(&e.path)
                    .show(ui, |ui| self.tree(ui, &e.path, act));
                r.header_response.context_menu(|ui| {
                    if ui.button("Новый файл").clicked() {
                        *act = TreeAct::New { dir: e.path.clone(), is_dir: false };
                        ui.close_menu();
                    }
                    if ui.button("Новая папка").clicked() {
                        *act = TreeAct::New { dir: e.path.clone(), is_dir: true };
                        ui.close_menu();
                    }
                });
            } else {
                let sel = self.tabs.get(self.active).and_then(|t| t.path.as_ref()) == Some(&e.path);
                ui.horizontal(|ui| {
                    let mut t = RichText::new(&e.name);
                    if let Some(c) = color {
                        t = t.color(c);
                    }
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
            ui.label("Папка не открыта");
            if ui.button("Открыть папку…").clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                    self.set_root(p);
                }
            }
            return;
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(name_of(&root).to_uppercase()).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("Обновить").clicked() {
                    self.dir_cache.clear();
                    self.git_dirty = true;
                }
                if ui.small_button("📁+").on_hover_text("Новая папка").clicked() {
                    self.new_entry = Some((root.clone(), true, String::new()));
                }
                if ui.small_button("📄+").on_hover_text("Новый файл").clicked() {
                    self.new_entry = Some((root.clone(), false, String::new()));
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

    // ───────── панель Git ─────────

    fn git_ui(&mut self, ui: &mut egui::Ui) {
        if !self.trusted {
            ui.label("🛡 Git отключён в ограниченном режиме.");
            if ui.button("Доверять папке…").clicked() {
                self.trust_prompt = true;
            }
            return;
        }
        let Some(root) = self.root.clone() else {
            ui.label("Откройте папку");
            return;
        };
        let ctx = ui.ctx().clone();
        let mut acts: Vec<GitAct> = vec![];
        match self.git.clone() {
            None => {
                ui.label("Загрузка…");
            }
            Some(git::Status::NotRepo) => {
                ui.label("Эта папка не является git-репозиторием.");
                if ui.button("Инициализировать репозиторий").clicked() {
                    acts.push(GitAct::Op { label: "git init".into(), args: vec!["init".into()], top: root.clone() });
                }
            }
            Some(git::Status::Error(e)) => {
                ui.colored_label(Color32::LIGHT_RED, e);
            }
            Some(git::Status::Repo(st)) => {
                let top = st.top.clone();
                let op = |label: &str, a: &[&str]| GitAct::Op {
                    label: label.to_string(),
                    args: a.iter().map(|s| s.to_string()).collect(),
                    top: top.clone(),
                };
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("branch")
                        .selected_text(format!("ветка: {}", st.branch))
                        .show_ui(ui, |ui| {
                            for b in &st.branches {
                                if ui.selectable_label(*b == st.branch, b).clicked() {
                                    acts.push(op(&format!("checkout {b}"), &["checkout", b]));
                                }
                            }
                        });
                    if ui.button("Обновить").clicked() {
                        acts.push(GitAct::Refresh);
                    }
                });
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_branch)
                            .hint_text("новая ветка")
                            .desired_width(110.0),
                    );
                    if ui
                        .add_enabled(!self.new_branch.trim().is_empty(), egui::Button::new("Создать"))
                        .clicked()
                    {
                        let n = self.new_branch.trim().to_string();
                        acts.push(op(&format!("checkout -b {n}"), &["checkout", "-b", &n]));
                        self.new_branch.clear();
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("Fetch").clicked() {
                        acts.push(op("fetch", &["fetch", "--all", "--prune"]));
                    }
                    if ui.button(format!("Pull ↓{}", st.behind)).clicked() {
                        acts.push(op("pull", &["pull"]));
                    }
                    if ui.button(format!("Push ↑{}", st.ahead)).clicked() {
                        if st.upstream.is_empty() {
                            acts.push(op("push", &["push", "-u", "origin", &st.branch]));
                        } else {
                            acts.push(op("push", &["push"]));
                        }
                    }
                });
                ui.separator();
                ui.add(
                    egui::TextEdit::multiline(&mut self.commit_msg)
                        .hint_text("Сообщение коммита")
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
                );
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!self.commit_msg.trim().is_empty(), egui::Button::new("✔ Commit"))
                        .clicked()
                    {
                        let m = self.commit_msg.clone();
                        acts.push(op("commit", &["commit", "-m", &m]));
                    }
                    if ui.button("Stage all").clicked() {
                        acts.push(op("add -A", &["add", "-A"]));
                    }
                    if ui.button("История").clicked() {
                        acts.push(GitAct::Log { top: top.clone() });
                    }
                });
                ui.separator();

                let staged: Vec<&git::GitFile> = st.files.iter().filter(|f| f.staged()).collect();
                let changes: Vec<&git::GitFile> = st.files.iter().filter(|f| f.unstaged()).collect();
                egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(ui.available_height() - 90.0).show(ui, |ui| {
                    egui::CollapsingHeader::new(format!("В индексе ({})", staged.len()))
                        .default_open(true)
                        .show(ui, |ui| {
                            for f in &staged {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(f.x.to_string()).color(badge_color(f.x)).monospace());
                                    if ui.selectable_label(false, &f.path).clicked() {
                                        acts.push(GitAct::Diff {
                                            title: format!("Δ {} (index)", f.path),
                                            top: top.clone(),
                                            path: f.path.clone(),
                                            staged: true,
                                        });
                                    }
                                    if ui.small_button("-").on_hover_text("Убрать из индекса").clicked() {
                                        acts.push(op("unstage", &["restore", "--staged", "--", &f.path]));
                                    }
                                });
                            }
                        });
                    egui::CollapsingHeader::new(format!("Изменения ({})", changes.len()))
                        .default_open(true)
                        .show(ui, |ui| {
                            for f in &changes {
                                ui.horizontal(|ui| {
                                    let b = badge_char(f);
                                    ui.label(RichText::new(b.to_string()).color(badge_color(b)).monospace());
                                    if ui.selectable_label(false, &f.path).clicked() {
                                        if f.untracked() {
                                            acts.push(GitAct::OpenFile(top.join(&f.path)));
                                        } else {
                                            acts.push(GitAct::Diff {
                                                title: format!("Δ {}", f.path),
                                                top: top.clone(),
                                                path: f.path.clone(),
                                                staged: false,
                                            });
                                        }
                                    }
                                    if ui.small_button("+").on_hover_text("В индекс").clicked() {
                                        acts.push(op("add", &["add", "--", &f.path]));
                                    }
                                    if ui.small_button("Откат").on_hover_text("Отменить изменения").clicked() {
                                        acts.push(GitAct::Discard((*f).clone(), top.clone()));
                                    }
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
        for a in acts {
            self.run_git_act(&ctx, a);
        }
    }

    // ───────── интерфейс ─────────

    fn shortcuts(&mut self, ctx: &egui::Context) {
        // Ctrl+` работает всегда; остальное — только если фокус не в терминале
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Backtick)) {
            self.show_terminal = !self.show_terminal;
        }
        if self.term_focused {
            return;
        }
        let cs = Modifiers::COMMAND.plus(Modifiers::SHIFT);
        if ctx.input_mut(|i| i.consume_key(cs, Key::G)) {
            self.show_side = true;
            self.side = Side::Git;
        }
        if ctx.input_mut(|i| i.consume_key(cs, Key::E)) {
            self.show_side = true;
            self.side = Side::Explorer;
        }
        if ctx.input_mut(|i| i.consume_key(cs, Key::O)) {
            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                self.set_root(p);
            }
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::S)) {
            self.save_tab(self.active);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::O)) {
            if let Some(p) = rfd::FileDialog::new().pick_file() {
                self.open_file(p);
            }
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::W)) {
            self.request_close(self.active);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::B)) {
            self.show_side = !self.show_side;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Comma)) {
            self.show_settings = true;
        }
    }

    fn ui_menu(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("Файл", |ui| {
                    if ui.button("Новый файл").clicked() {
                        let uid = self.next_uid;
                        self.next_uid += 1;
                        self.tabs.push(Tab::new(uid, None, "untitled".into(), String::new(), "txt".into(), false));
                        self.active = self.tabs.len() - 1;
                        ui.close_menu();
                    }
                    if ui.button("Открыть файл…   Ctrl+O").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_file() {
                            self.open_file(p);
                        }
                        ui.close_menu();
                    }
                    if ui.button("Открыть папку…   Ctrl+Shift+O").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            self.set_root(p);
                        }
                        ui.close_menu();
                    }
                    if ui.button("Сохранить   Ctrl+S").clicked() {
                        self.save_tab(self.active);
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Настройки   Ctrl+,").clicked() {
                        self.show_settings = true;
                        ui.close_menu();
                    }
                });
                ui.menu_button("Вид", |ui| {
                    if ui.button("Боковая панель   Ctrl+B").clicked() {
                        self.show_side = !self.show_side;
                        ui.close_menu();
                    }
                    if ui.button("Проводник   Ctrl+Shift+E").clicked() {
                        self.show_side = true;
                        self.side = Side::Explorer;
                        ui.close_menu();
                    }
                    if ui.button("Git   Ctrl+Shift+G").clicked() {
                        self.show_side = true;
                        self.side = Side::Git;
                        ui.close_menu();
                    }
                    if ui.button("Терминал   Ctrl+`").clicked() {
                        self.show_terminal = !self.show_terminal;
                        ui.close_menu();
                    }
                });
                ui.menu_button("Терминал", |ui| {
                    if ui.button("Новый терминал").clicked() {
                        self.show_terminal = true;
                        self.want_new_term = true;
                        ui.close_menu();
                    }
                });
            });
        });
    }

    fn ui_status(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if !self.trusted && self.root.is_some() {
                    if ui
                        .button(RichText::new("🛡 Ограниченный режим").color(Color32::from_rgb(230, 160, 60)))
                        .clicked()
                    {
                        self.trust_prompt = true;
                    }
                }
                if let Some(git::Status::Repo(s)) = &self.git {
                    let t = format!("git: {} ↓{} ↑{}", s.branch, s.behind, s.ahead);
                    if ui.button(t).clicked() {
                        self.show_side = true;
                        self.side = Side::Git;
                    }
                }
                ui.label(&self.status);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("⚙").on_hover_text("Настройки").clicked() {
                        self.show_settings = true;
                    }
                    let ind = if self.eff.insert_spaces {
                        format!("Пробелов: {}", self.eff.tab_size)
                    } else {
                        "Tab".to_string()
                    };
                    ui.label(format!("{ind}   UTF-8"));
                });
            });
        });
    }

    fn ui_terminal(&mut self, ctx: &egui::Context) {
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
                    ui.label(RichText::new("ТЕРМИНАЛ").small().strong());
                    for (i, t) in self.terminals.iter().enumerate() {
                        if ui.selectable_label(i == self.term_active, &t.title).clicked() {
                            self.term_active = i;
                        }
                        if ui.small_button("×").clicked() {
                            close = Some(i);
                        }
                    }
                    if self.trusted && ui.small_button("+").on_hover_text("Новый терминал").clicked() {
                        self.want_new_term = true;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Скрыть").clicked() {
                            self.show_terminal = false;
                        }
                    });
                });
                if let Some(i) = close {
                    self.terminals.remove(i);
                    self.term_active = self.term_active.min(self.terminals.len().saturating_sub(1));
                    self.term_error = None;
                }
                if !self.trusted {
                    ui.label("🛡 Терминал отключён в ограниченном режиме: он может выполнять код из этой папки.");
                    if ui.button("Доверять папке…").clicked() {
                        self.trust_prompt = true;
                    }
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
        if !self.show_side {
            return;
        }
        egui::SidePanel::left("side")
            .resizable(true)
            .default_width(270.0)
            .show(ctx, |ui| {
                let n = match &self.git {
                    Some(git::Status::Repo(s)) => s.files.len(),
                    _ => 0,
                };
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.side, Side::Explorer, "📂 Файлы");
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
        let bg = self
            .themes
            .themes
            .get(&self.eff.theme)
            .and_then(|t| t.settings.background)
            .map(|c| Color32::from_rgb(c.r, c.g, c.b))
            .unwrap_or(ctx.style().visuals.panel_fill);
        let frame = egui::Frame::none().fill(bg).inner_margin(egui::Margin::same(4.0));
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            let mut close = None;
            let mut activate = None;
            ui.horizontal_wrapped(|ui| {
                for (i, t) in self.tabs.iter().enumerate() {
                    let label = if t.dirty { format!("{} ●", t.title) } else { t.title.clone() };
                    if ui.selectable_label(i == self.active, label).clicked() {
                        activate = Some(i);
                    }
                    if ui.small_button("×").clicked() {
                        close = Some(i);
                    }
                    ui.add_space(4.0);
                }
            });
            if let Some(i) = activate {
                self.active = i;
            }
            if let Some(i) = close {
                self.request_close(i);
            }
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
            ui.label("Лёгкая IDE на Rust");
            ui.add_space(16.0);
            if ui.button("Открыть папку…").clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                    self.set_root(p);
                }
            }
            if ui.button("Открыть файл…").clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_file() {
                    self.open_file(p);
                }
            }
            ui.add_space(16.0);
            ui.label("Ctrl+S сохранить · Ctrl+B панель · Ctrl+` терминал · Ctrl+Shift+G git · Ctrl+, настройки");
        });
    }

    fn editor_ui(&mut self, ui: &mut egui::Ui) {
        let eff = self.eff.clone();
        let theme: &Theme = self
            .themes
            .themes
            .get(&eff.theme)
            .or_else(|| self.themes.themes.get("base16-ocean.dark"))
            .unwrap_or_else(|| self.themes.themes.values().next().unwrap());
        let fg = theme
            .settings
            .foreground
            .map(|c| Color32::from_rgb(c.r, c.g, c.b))
            .unwrap_or(Color32::LIGHT_GRAY);
        let ss = &self.ss;
        let tab = &mut self.tabs[self.active];

        let font = FontId::monospace(eff.font_size);
        let (cw, rh) = ui.fonts(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
        let Tab { uid, text, dirty, readonly, last_edit, hl, gkey, galley, ext, .. } = tab;
        let id = egui::Id::new(("editor", *uid));
        let ctx = ui.ctx().clone();

        // Tab → пробелы, Enter → автоотступ
        if !*readonly && ctx.memory(|m| m.has_focus(id)) {
            if eff.insert_spaces && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab)) {
                if let Some((a, b)) = cursor_range(&ctx, id) {
                    replace_range(&ctx, id, text, a, b, &" ".repeat(eff.tab_size.max(1)));
                    *dirty = true;
                    *last_edit = Instant::now();
                }
            } else if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                if let Some((a, b)) = cursor_range(&ctx, id) {
                    let before = &text[..byte_idx(text, a)];
                    let line = before.rsplit('\n').next().unwrap_or("");
                    let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
                    let opens = line.trim_end().ends_with(|c: char| matches!(c, '{' | '(' | '[' | ':'));
                    let extra = if opens {
                        if eff.insert_spaces {
                            " ".repeat(eff.tab_size.max(1))
                        } else {
                            "\t".to_string()
                        }
                    } else {
                        String::new()
                    };
                    let ins = format!("\n{indent}{extra}");
                    replace_range(&ctx, id, text, a, b, &ins);
                    *dirty = true;
                    *last_edit = Instant::now();
                }
            }
        }

        // Кэш раскладки: пересчёт только если изменился текст/параметры
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
                if let Some(g) = galley.as_ref() {
                    return g.clone();
                }
            }
            let mut job = if t.len() > 1_500_000 {
                // огромные файлы — без подсветки
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
        let mut ro: &str = text.as_str();
        let buf: &mut dyn egui::TextBuffer = if ro_flag { &mut ro } else { text };

        let sa = if eff.word_wrap { egui::ScrollArea::vertical() } else { egui::ScrollArea::both() };
        sa.auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let gutter = if eff.line_numbers {
                    Some(ui.allocate_exact_size(vec2(gw, 1.0), Sense::hover()).0)
                } else {
                    None
                };
                let width = if eff.word_wrap { ui.available_width() } else { f32::INFINITY };
                let out = egui::TextEdit::multiline(buf)
                    .id(id)
                    .frame(false)
                    .lock_focus(true)
                    .desired_width(width)
                    .desired_rows(rows)
                    .layouter(&mut layouter)
                    .show(ui);
                if out.response.changed() {
                    changed = true;
                }
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
                                painter.text(
                                    pos2(gr.max.x - 6.0, y),
                                    Align2::RIGHT_TOP,
                                    n.to_string(),
                                    font.clone(),
                                    col,
                                );
                            }
                            n += 1;
                        }
                        newline = row.ends_with_newline;
                        if y > clip.max.y {
                            break;
                        }
                    }
                }
            });
        });
        if changed {
            *dirty = true;
            *last_edit = Instant::now();
        }
    }

    // ───────── диалоги ─────────

    fn ui_dialogs(&mut self, ctx: &egui::Context) {
        self.dialog_trust(ctx);
        self.dialog_settings(ctx);

        // закрытие вкладки с несохранёнными изменениями
        if let Some(i) = self.confirm_close {
            let (mut save, mut discard, mut cancel) = (false, false, false);
            let title = self.tabs.get(i).map(|t| t.title.clone()).unwrap_or_default();
            egui::Window::new("Несохранённые изменения")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!("Сохранить изменения в «{title}»?"));
                    ui.horizontal(|ui| {
                        save = ui.button("Сохранить").clicked();
                        discard = ui.button("Не сохранять").clicked();
                        cancel = ui.button("Отмена").clicked();
                    });
                });
            if save {
                self.save_tab(i);
                if !self.tabs.get(i).map(|t| t.dirty).unwrap_or(false) {
                    self.close_tab(i);
                }
                self.confirm_close = None;
            } else if discard {
                self.close_tab(i);
                self.confirm_close = None;
            } else if cancel {
                self.confirm_close = None;
            }
        }

        // откат изменений в git
        if let Some((f, top)) = self.confirm_discard.clone() {
            let (mut yes, mut no) = (false, false);
            egui::Window::new("Отменить изменения?")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!("Изменения в «{}» будут потеряны безвозвратно.", f.path));
                    ui.horizontal(|ui| {
                        yes = ui.button("Да, откатить").clicked();
                        no = ui.button("Отмена").clicked();
                    });
                });
            if yes {
                let args: Vec<String> = if f.untracked() {
                    vec!["clean".into(), "-f".into(), "--".into(), f.path.clone()]
                } else {
                    vec!["restore".into(), "--".into(), f.path.clone()]
                };
                self.confirm_discard = None;
                self.run_git_act(ctx, GitAct::Op { label: "discard".into(), args, top });
            } else if no {
                self.confirm_discard = None;
            }
        }

        // новый файл / папка
        let (mut create, mut cancel) = (false, false);
        if let Some((dir, is_dir, name)) = self.new_entry.as_mut() {
            let title = if *is_dir { "Новая папка" } else { "Новый файл" };
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(dir.display().to_string());
                    let r = ui.text_edit_singleline(name);
                    r.request_focus();
                    if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        create = true;
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Создать").clicked() {
                            create = true;
                        }
                        if ui.button("Отмена").clicked() {
                            cancel = true;
                        }
                    });
                });
        }
        if create {
            if let Some((dir, is_dir, name)) = self.new_entry.take() {
                self.create_entry(dir, is_dir, &name);
            }
        } else if cancel {
            self.new_entry = None;
        }
    }

    fn dialog_trust(&mut self, ctx: &egui::Context) {
        if !self.trust_prompt {
            return;
        }
        let Some(root) = self.root.clone() else {
            self.trust_prompt = false;
            return;
        };
        let parent_name = root
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| root.display().to_string());
        let (mut yes, mut no) = (false, false);
        egui::Window::new("🛡 Вы доверяете авторам файлов в этой папке?")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(460.0);
                ui.label(RichText::new(root.display().to_string()).monospace());
                ui.add_space(6.0);
                ui.label(
                    "Редактор может запускать терминал и git и применять настройки проекта — \
                     всё это способно выполнять код из файлов папки. Доверяйте, только если знаете автора.",
                );
                ui.add_space(4.0);
                ui.label(
                    "В ограниченном режиме файлы можно читать и править, но терминал, git и \
                     настройки проекта отключены.",
                );
                ui.add_space(6.0);
                ui.checkbox(&mut self.trust_parent, format!("Доверять всем папкам внутри «{parent_name}»"));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    yes = ui.button("✔ Да, я доверяю авторам").clicked();
                    no = ui.button("✖ Нет, ограниченный режим").clicked();
                });
            });
        if yes {
            let target = if self.trust_parent {
                root.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| root.clone())
            } else {
                root.clone()
            };
            self.trust.add(&target);
            self.trusted = true;
            self.trust_prompt = false;
            self.load_project();
            self.recompute();
            self.git_dirty = true;
        } else if no {
            self.trust_prompt = false;
        }
    }

    fn dialog_settings(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut open = true;
        let mut changed = false;
        let mut pchanged = false;
        let theme_names: Vec<String> = self.themes.themes.keys().cloned().collect();
        let has_project = self.root.is_some() && self.trusted;
        egui::Window::new("⚙ Настройки")
            .open(&mut open)
            .default_width(460.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.settings_tab, 0, "Пользователь");
                    ui.add_enabled_ui(has_project, |ui| {
                        ui.selectable_value(&mut self.settings_tab, 1, "Проект");
                    });
                });
                ui.separator();
                if self.settings_tab == 1 && has_project {
                    let p = &mut self.project;
                    ui.label("Переопределяют пользовательские настройки только для этой папки\n(.rustcode/settings.json).");
                    pchanged |= opt_row(ui, "Размер табуляции", &mut p.tab_size, 4usize, |ui, v| {
                        ui.add(egui::Slider::new(v, 1..=8)).changed()
                    });
                    pchanged |= opt_row(ui, "Пробелы вместо Tab", &mut p.insert_spaces, true, |ui, v| {
                        ui.checkbox(v, "").changed()
                    });
                    pchanged |= opt_row(ui, "Перенос строк", &mut p.word_wrap, false, |ui, v| {
                        ui.checkbox(v, "").changed()
                    });
                    pchanged |= opt_row(ui, "Убирать пробелы в конце строк", &mut p.trim_trailing_whitespace, true, |ui, v| {
                        ui.checkbox(v, "").changed()
                    });
                    pchanged |= opt_row(ui, "Shell терминала", &mut p.terminal_shell, String::new(), |ui, v| {
                        ui.text_edit_singleline(v).changed()
                    });
                    pchanged |= opt_row(
                        ui,
                        "Скрывать в проводнике",
                        &mut p.explorer_exclude,
                        vec![".git".to_string(), "target".to_string()],
                        |ui, v| edit_list(ui, v),
                    );
                } else {
                    let s = &mut self.user;
                    changed |= ui.add(egui::Slider::new(&mut s.font_size, 9.0..=32.0).text("Размер шрифта")).changed();
                    changed |= ui.add(egui::Slider::new(&mut s.tab_size, 1..=8).text("Размер табуляции")).changed();
                    changed |= ui.checkbox(&mut s.insert_spaces, "Вставлять пробелы вместо Tab").changed();
                    changed |= ui.checkbox(&mut s.word_wrap, "Перенос строк").changed();
                    changed |= ui.checkbox(&mut s.line_numbers, "Номера строк").changed();
                    changed |= ui.checkbox(&mut s.auto_save, "Автосохранение (через 1 с после правки)").changed();
                    changed |= ui.checkbox(&mut s.trim_trailing_whitespace, "Убирать пробелы в конце строк при сохранении").changed();
                    egui::ComboBox::from_label("Тема")
                        .selected_text(s.theme.clone())
                        .show_ui(ui, |ui| {
                            for n in &theme_names {
                                if ui.selectable_value(&mut s.theme, n.clone(), n).changed() {
                                    changed = true;
                                }
                            }
                        });
                    ui.separator();
                    changed |= ui
                        .add(egui::Slider::new(&mut s.terminal_font_size, 9.0..=28.0).text("Шрифт терминала"))
                        .changed();
                    ui.horizontal(|ui| {
                        ui.label("Shell терминала (пусто = по умолчанию):");
                        changed |= ui.text_edit_singleline(&mut s.terminal_shell).changed();
                    });
                    changed |= ui
                        .add(egui::Slider::new(&mut s.git_poll_secs, 1..=30).text("Опрос git, сек"))
                        .changed();
                    ui.label("Скрывать в проводнике (имя или маска *.ext):");
                    changed |= edit_list(ui, &mut s.explorer_exclude);
                }
            });
        self.show_settings = open;
        if changed || pchanged {
            self.recompute();
            settings::save_json(&settings::user_settings_path(), &self.user);
            if pchanged {
                if let Some(r) = &self.root {
                    settings::save_json(&settings::project_settings_path(r), &self.project);
                }
            }
        }
    }
}

/// Строка "переопределить настройку проекта": чекбокс + редактор значения.
fn opt_row<T: Clone>(
    ui: &mut egui::Ui,
    label: &str,
    opt: &mut Option<T>,
    default: T,
    mut edit: impl FnMut(&mut egui::Ui, &mut T) -> bool,
) -> bool {
    let mut ch = false;
    ui.horizontal(|ui| {
        let mut on = opt.is_some();
        if ui.checkbox(&mut on, label).changed() {
            *opt = if on { Some(default.clone()) } else { None };
            ch = true;
        }
        if let Some(v) = opt.as_mut() {
            if edit(ui, v) {
                ch = true;
            }
        }
    });
    ch
}

fn edit_list(ui: &mut egui::Ui, v: &mut Vec<String>) -> bool {
    let mut ch = false;
    let mut del = None;
    ui.vertical(|ui| {
        for (i, s) in v.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                if ui.text_edit_singleline(s).changed() {
                    ch = true;
                }
                if ui.small_button("×").clicked() {
                    del = Some(i);
                }
            });
        }
        if ui.small_button("+ добавить").clicked() {
            v.push(String::new());
            ch = true;
        }
    });
    if let Some(i) = del {
        v.remove(i);
        ch = true;
    }
    ch
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump();
        self.apply_theme(ctx);
        self.shortcuts(ctx);
        self.autosave();
        self.poll_git(ctx);

        self.ui_menu(ctx);
        self.ui_status(ctx);
        self.ui_terminal(ctx);
        self.ui_sidebar(ctx);
        self.ui_central(ctx);
        self.ui_dialogs(ctx);

        // редкий "пульс" для автосохранения и опроса git
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "RustCode",
        opts,
        Box::new(|_cc| {
            let mut app = App::new();
            // rustcode <папка|файл>
            if let Some(arg) = std::env::args().nth(1) {
                let p = PathBuf::from(arg);
                if p.is_dir() {
                    app.set_root(p);
                } else if p.is_file() {
                    app.open_file(p);
                }
            }
            Ok(Box::new(app))
        }),
    )
}
