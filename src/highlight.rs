//! Инкрементальная подсветка.

use eframe::egui::{text::LayoutJob, Color32, FontId, TextFormat};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use syntect::highlighting::{HighlightIterator, HighlightState, Highlighter, Theme};
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};
use syntect::util::LinesWithEndings;

struct Line {
    hash: u64,
    spans: Vec<(usize, Color32)>,
    ps: ParseState,
    hs: HighlightState,
}

#[derive(Default)]
pub struct HlCache {
    key: (String, String),
    lines: Vec<Line>,
}

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn highlight_line(
    ss: &SyntaxSet,
    hl: &Highlighter,
    ps: &mut ParseState,
    hs: &mut HighlightState,
    line: &str,
    fallback: Color32,
) -> Vec<(usize, Color32)> {
    if line.len() > 3000 {
        return vec![(line.len(), fallback)];
    }
    match ps.parse_line(line, ss) {
        Ok(ops) => {
            let mut v: Vec<(usize, Color32)> = Vec::new();
            for (st, s) in HighlightIterator::new(hs, &ops, line, hl) {
                let c = Color32::from_rgb(st.foreground.r, st.foreground.g, st.foreground.b);
                match v.last_mut() {
                    Some(l) if l.1 == c => l.0 += s.len(),
                    _ => v.push((s.len(), c)),
                }
            }
            if v.is_empty() {
                v.push((line.len(), fallback));
            }
            v
        }
        Err(_) => vec![(line.len(), fallback)],
    }
}

impl HlCache {
    pub fn layout(
        &mut self,
        ss: &SyntaxSet,
        theme: &Theme,
        theme_name: &str,
        ext: &str,
        text: &str,
        font: &FontId,
    ) -> LayoutJob {
        let key = (ext.to_string(), theme_name.to_string());
        if self.key != key {
            self.key = key;
            self.lines.clear();
        }
        let fallback = theme
            .settings
            .foreground
            .map(|c| Color32::from_rgb(c.r, c.g, c.b))
            .unwrap_or(Color32::LIGHT_GRAY);
        let syntax = ss
            .find_syntax_by_extension(ext)
            .unwrap_or_else(|| ss.find_syntax_plain_text());
        let hl = Highlighter::new(theme);

        let src: Vec<&str> = LinesWithEndings::from(text).collect();
        let hashes: Vec<u64> = src.iter().map(|l| hash_str(l)).collect();

        let mut i = 0;
        while i < src.len() && i < self.lines.len() && self.lines[i].hash == hashes[i] {
            i += 1;
        }
        let mut old: Vec<Option<Line>> = self.lines.split_off(i).into_iter().map(Some).collect();

        let (mut ps, mut hs) = match self.lines.last() {
            Some(l) => (l.ps.clone(), l.hs.clone()),
            None => (
                ParseState::new(syntax),
                HighlightState::new(&hl, ScopeStack::new()),
            ),
        };

        let mut j = i;
        while j < src.len() {
            let spans = highlight_line(ss, &hl, &mut ps, &mut hs, src[j], fallback);
            let k = j - i;
            let converged = matches!(
                old.get(k),
                Some(Some(o)) if o.hash == hashes[j] && o.ps == ps && o.hs == hs
            );
            self.lines.push(Line { hash: hashes[j], spans, ps: ps.clone(), hs: hs.clone() });
            j += 1;
            if converged {
                let mut kk = k + 1;
                while j < src.len()
                    && kk < old.len()
                    && old[kk].as_ref().map_or(false, |o| o.hash == hashes[j])
                {
                    let l = old[kk].take().unwrap();
                    ps = l.ps.clone();
                    hs = l.hs.clone();
                    self.lines.push(l);
                    j += 1;
                    kk += 1;
                }
            }
        }

        let mut job = LayoutJob::default();
        for (l, line) in self.lines.iter().zip(src.iter()) {
            let mut off = 0;
            for &(len, c) in &l.spans {
                let end = (off + len).min(line.len());
                if end > off {
                    job.append(
                        &line[off..end],
                        0.0,
                        TextFormat { font_id: font.clone(), color: c, ..Default::default() },
                    );
                }
                off = end;
            }
            if off < line.len() {
                job.append(
                    &line[off..],
                    0.0,
                    TextFormat { font_id: font.clone(), color: fallback, ..Default::default() },
                );
            }
        }
        job
    }
}