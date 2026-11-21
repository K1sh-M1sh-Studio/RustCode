//! Git через консольный `git` (без libgit2 — не нужна C-сборка).

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug)]
pub struct GitFile {
    pub x: char,
    pub y: char,
    pub path: String,
}

impl GitFile {
    pub fn staged(&self) -> bool {
        self.x != ' ' && self.x != '?' && self.x != '!'
    }
    pub fn unstaged(&self) -> bool {
        self.y != ' '
    }
    pub fn untracked(&self) -> bool {
        self.x == '?'
    }
}

#[derive(Clone, Default)]
pub struct GitState {
    pub top: PathBuf,
    pub prefix: String,
    pub branch: String,
    pub upstream: String,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<GitFile>,
    pub branches: Vec<String>,
}

#[derive(Clone)]
pub enum Status {
    NotRepo,
    Repo(GitState),
    Error(String),
}

fn cmd(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

pub fn run(dir: &Path, args: &[&str]) -> Result<String, String> {
    let o = cmd(dir)
        .args(args)
        .output()
        .map_err(|e| format!("git не найден или не запускается: {e}"))?;
    let so = String::from_utf8_lossy(&o.stdout).to_string();
    let se = String::from_utf8_lossy(&o.stderr).to_string();
    if o.status.success() {
        Ok(if so.trim().is_empty() { se } else { so })
    } else {
        Err(if se.trim().is_empty() { so } else { se })
    }
}

fn num_after(s: &str, key: &str) -> u32 {
    s.find(key)
        .map(|i| {
            s[i + key.len()..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

pub fn status(root: &Path) -> Status {
    let out = match run(root, &["status", "--porcelain=v1", "-b", "-z"]) {
        Ok(o) => o,
        Err(e) => {
            return if e.contains("not a git repository") {
                Status::NotRepo
            } else {
                Status::Error(e)
            }
        }
    };
    let mut st = GitState::default();
    st.prefix = run(root, &["rev-parse", "--show-prefix"])
        .unwrap_or_default()
        .trim()
        .to_string();
    let top = run(root, &["rev-parse", "--show-toplevel"]).unwrap_or_default();
    st.top = if top.trim().is_empty() { root.to_path_buf() } else { PathBuf::from(top.trim()) };

    let mut it = out.split('\0');
    while let Some(e) = it.next() {
        if e.is_empty() {
            continue;
        }
        if let Some(h) = e.strip_prefix("## ") {
            if let Some(b) = h.strip_prefix("No commits yet on ") {
                st.branch = b.to_string();
            } else {
                let (name, rest) = match h.split_once("...") {
                    Some((n, r)) => (n, r),
                    None => (h, ""),
                };
                st.branch = name.to_string();
                st.upstream = rest.split(' ').next().unwrap_or("").to_string();
                st.ahead = num_after(rest, "ahead ");
                st.behind = num_after(rest, "behind ");
            }
            continue;
        }
        if e.len() < 4 || !e.is_char_boundary(3) {
            continue;
        }
        let b = e.as_bytes();
        let (x, y) = (b[0] as char, b[1] as char);
        if x == 'R' || x == 'C' || y == 'R' || y == 'C' {
            it.next();
        }
        st.files.push(GitFile { x, y, path: e[3..].to_string() });
    }

    st.branches = run(root, &["branch", "--format=%(refname:short)"])
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('('))
        .collect();
    Status::Repo(st)
}