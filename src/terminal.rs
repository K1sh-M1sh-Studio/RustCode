//! Настоящий терминал: псевдотерминал (portable-pty) + эмулятор VT100 (vt100),
//! отрисовка сетки ячеек с цветами прямо в egui.

use eframe::egui::text::{LayoutJob, TextFormat};
use eframe::egui::{self, pos2, vec2, Color32, Event, EventFilter, FontId, Key, Modifiers, Rect, Sense};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

const TERM_BG: Color32 = Color32::from_rgb(24, 24, 24);
const TERM_FG: Color32 = Color32::from_rgb(204, 204, 204);

const ANSI: [(u8, u8, u8); 16] = [
    (0, 0, 0), (205, 49, 49), (13, 188, 121), (229, 229, 16),
    (36, 114, 200), (188, 63, 188), (17, 168, 205), (229, 229, 229),
    (102, 102, 102), (241, 76, 76), (35, 209, 139), (245, 245, 67),
    (59, 142, 234), (214, 112, 214), (41, 184, 219), (255, 255, 255),
];

fn idx_color(n: u8) -> Color32 {
    match n {
        0..=15 => {
            let c = ANSI[n as usize];
            Color32::from_rgb(c.0, c.1, c.2)
        }
        16..=231 => {
            let n = n - 16;
            let lv = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Color32::from_rgb(lv(n / 36), lv(n / 6 % 6), lv(n % 6))
        }
        _ => {
            let g = 8 + (n - 232) * 10;
            Color32::from_rgb(g, g, g)
        }
    }
}

fn map_color(c: vt100::Color) -> Option<Color32> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(n) => Some(idx_color(n)),
        vt100::Color::Rgb(r, g, b) => Some(Color32::from_rgb(r, g, b)),
    }
}

pub struct Terminal {
    pub title: String,
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    alive: Arc<AtomicBool>,
    size: (u16, u16),
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

impl Terminal {
    pub fn spawn(ctx: &egui::Context, shell: &str, cwd: &Path, title: String) -> Result<Self, String> {
        let pair = native_pty_system()
            .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| e.to_string())?;
        let mut cmd = CommandBuilder::new(shell);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        let child = pair.slave.spawn_command(cmd).map_err(|e| format!("{shell}: {e}"))?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer = pair.master.take_writer().map_err(|e| e.to_string())?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            NonZeroU16::new(24).unwrap(),
            NonZeroU16::new(80).unwrap(),
            5000,
        )));
        let writer = Arc::new(Mutex::new(writer));
        let alive = Arc::new(AtomicBool::new(true));

        {
            let parser = parser.clone();
            let writer = writer.clone();
            let alive = alive.clone();
            let ctx = ctx.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = &buf[..n];
                            let reply = {
                                let mut p = parser.lock().unwrap();
                                p.process(chunk);
                                // Windows ConPTY и некоторые оболочки спрашивают позицию курсора (ESC[6n)
                                // и зависают, если не получат ответ.
                                if chunk.windows(4).any(|w| w == b"\x1b[6n") {
                                    let (r, c) = p.screen().cursor_position();
                                    Some(format!("\x1b[{};{}R", r + 1, c + 1))
                                } else {
                                    None
                                }
                            };
                            if let Some(r) = reply {
                                let mut w = writer.lock().unwrap();
                                let _ = w.write_all(r.as_bytes());
                                let _ = w.flush();
                            }
                            ctx.request_repaint();
                        }
                    }
                }
                alive.store(false, Ordering::SeqCst);
                ctx.request_repaint();
            });
        }

        Ok(Self { title, parser, writer, master: pair.master, child, alive, size: (24, 80) })
    }

    fn send(&self, bytes: &[u8]) {
        let mut w = self.writer.lock().unwrap();
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        if (rows, cols) != self.size && rows > 0 && cols > 0 {
            self.size = (rows, cols);
            let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
            self.parser.lock().unwrap().set_size(
                NonZeroU16::new(rows).unwrap(),
                NonZeroU16::new(cols).unwrap(),
            );
        }
    }

    /// Рисует терминал и обрабатывает ввод. Возвращает true, если терминал в фокусе.
    pub fn ui(&mut self, ui: &mut egui::Ui, font_size: f32) -> bool {
        let font = FontId::monospace(font_size);
        let (cw, ch) = ui.fonts(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
        let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click());
        if resp.clicked() {
            resp.request_focus();
        }
        let focused = resp.has_focus();
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    resp.id,
                    EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                )
            });
        }

        let cols = (((rect.width() - 8.0) / cw).floor().max(2.0)) as u16;
        let rows = ((rect.height() / ch).floor().max(2.0)) as u16;
        self.resize(rows, cols);

        // Прокрутка колесом
        if resp.hovered() {
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            if dy.abs() > 0.5 {
                let mut p = self.parser.lock().unwrap();
                let cur = p.screen().scrollback() as i32;
                let new = (cur + (dy / ch).round() as i32).max(0) as usize;
                p.screen_mut().set_scrollback(new);
            }
        }

        // Ввод
        if focused {
            let events = ui.input(|i| i.events.clone());
            let app = self.parser.lock().unwrap().screen().application_cursor();
            let mut typed = false;
            for ev in events {
                match ev {
                    Event::Text(t) => {
                        self.send(t.as_bytes());
                        typed = true;
                    }
                    Event::Paste(t) => {
                        self.send(t.replace("\r\n", "\r").replace('\n', "\r").as_bytes());
                        typed = true;
                    }
                    Event::Copy => {
                        self.send(&[3]); // Ctrl+C -> SIGINT
                        typed = true;
                    }
                    Event::Cut => {
                        self.send(&[0x18]);
                        typed = true;
                    }
                    Event::Key { key, pressed: true, modifiers, .. } => {
                        if let Some(b) = key_bytes(key, &modifiers, app) {
                            self.send(&b);
                            typed = true;
                        }
                    }
                    _ => {}
                }
            }
            if typed {
                self.parser.lock().unwrap().screen_mut().set_scrollback(0);
            }
        }

        // Отрисовка
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, TERM_BG);
        {
            let parser = self.parser.lock().unwrap();
            let screen = parser.screen();
            let (rows_n, cols_n) = screen.size();
            for r in 0..rows_n.get() {
                let mut job = LayoutJob::default();
                let mut run = String::new();
                let mut cur: Option<(Color32, Color32)> = None;
                let flush = |job: &mut LayoutJob, run: &mut String, cur: Option<(Color32, Color32)>| {
                    if let Some((fg, bg)) = cur {
                        if !run.is_empty() {
                            job.append(
                                run,
                                0.0,
                                TextFormat {
                                    font_id: font.clone(),
                                    color: fg,
                                    background: bg,
                                    ..Default::default()
                                },
                            );
                            run.clear();
                        }
                    }
                };
                for c in 0..cols_n.get() {
                    let (txt, fg, bg) = match screen.cell(r, c) {
                        Some(cell) => {
                            if cell.is_wide_continuation() {
                                continue;
                            }
                            let mut fg = map_color(cell.fgcolor()).unwrap_or(TERM_FG);
                            let mut bg = map_color(cell.bgcolor()).unwrap_or(Color32::TRANSPARENT);
                            if cell.inverse() {
                                let nb = if bg == Color32::TRANSPARENT { TERM_BG } else { bg };
                                bg = fg;
                                fg = nb;
                            }
                            (cell.contents(), fg, bg)
                        }
                        None => ("", TERM_FG, Color32::TRANSPARENT),
                    };
                    let t = if txt.is_empty() { " " } else { txt };
                    if cur == Some((fg, bg)) {
                        run.push_str(t);
                    } else {
                        flush(&mut job, &mut run, cur);
                        cur = Some((fg, bg));
                        run.push_str(t);
                    }
                }
                flush(&mut job, &mut run, cur);
                let galley = painter.layout_job(job);
                painter.galley(pos2(rect.min.x + 4.0, rect.min.y + r as f32 * ch), galley, TERM_FG);
            }
            if screen.scrollback() == 0 && !screen.hide_cursor() {
                let (cr, cc) = screen.cursor_position();
                let cur_rect = Rect::from_min_size(
                    pos2(rect.min.x + 4.0 + cc as f32 * cw, rect.min.y + cr as f32 * ch),
                    vec2(cw, ch),
                );
                if focused {
                    painter.rect_filled(cur_rect, 0.0, Color32::from_white_alpha(140));
                } else {
                    painter.rect_stroke(cur_rect, 0.0, egui::Stroke::new(1.0, Color32::GRAY));
                }
            }
        }

        if !self.alive.load(Ordering::SeqCst) {
            painter.text(
                rect.right_bottom() - vec2(8.0, 8.0),
                egui::Align2::RIGHT_BOTTOM,
                "[процесс завершён — закройте вкладку или откройте новую]",
                FontId::proportional(12.0),
                Color32::LIGHT_RED,
            );
        }
        focused
    }
}

fn key_bytes(key: Key, m: &Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    let arrow = |c: u8| -> Vec<u8> {
        if app_cursor {
            vec![0x1b, b'O', c]
        } else {
            vec![0x1b, b'[', c]
        }
    };
    let s: Vec<u8> = match key {
        Key::Enter => b"\r".to_vec(),
        Key::Backspace => vec![0x7f],
        Key::Tab => {
            if m.shift {
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }
        }
        Key::Escape => vec![0x1b],
        Key::ArrowUp => arrow(b'A'),
        Key::ArrowDown => arrow(b'B'),
        Key::ArrowRight => arrow(b'C'),
        Key::ArrowLeft => arrow(b'D'),
        Key::Home => b"\x1b[H".to_vec(),
        Key::End => b"\x1b[F".to_vec(),
        Key::Delete => b"\x1b[3~".to_vec(),
        Key::Insert => b"\x1b[2~".to_vec(),
        Key::PageUp => b"\x1b[5~".to_vec(),
        Key::PageDown => b"\x1b[6~".to_vec(),
        _ => {
            // Ctrl+буква -> управляющий символ
            if m.ctrl && !m.alt {
                let n = key.name();
                if n.len() == 1 && n.as_bytes()[0].is_ascii_uppercase() {
                    let c = n.as_bytes()[0].to_ascii_lowercase();
                    // C/X/V приходят отдельными событиями Copy/Cut/Paste (кроме macOS)
                    if matches!(c, b'c' | b'x' | b'v') && !cfg!(target_os = "macos") {
                        return None;
                    }
                    return Some(vec![c - b'a' + 1]);
                }
            }
            return None;
        }
    };
    Some(s)
}
