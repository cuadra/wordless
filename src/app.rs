//! Editor state and the Vim key grammar: `[count] [operator] motion`.
//!
//! Deliberately free of terminal I/O so it can be driven by unit tests.
//! Motions are defined once and shared by Normal mode, Visual mode and the
//! `y` operator, which is what makes `yw`, `y}`, `5j`, `v3e y` all "just work".

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::docx::Paragraph;
use crate::html;
use crate::layout::{self, Pos, Row};

const SCROLLOFF: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Visual,
    VisualLine,
    Search { buf: String, forward: bool },
    Command(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Quit,
}

/// How a motion's range is interpreted by an operator.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Span {
    Exclusive,
    Inclusive,
    Line,
}

pub struct App {
    pub paras: Vec<Paragraph>,
    pub name: String,
    pub rows: Vec<Row>,
    pub measure: usize,
    pub text_width: usize,
    pub height: usize,
    pub top: usize,
    pub cur: usize,
    pub col: usize,
    want_col: usize,
    pub mode: Mode,
    /// Mode to return to after a `/` prompt (keeps Visual alive: `v/foo<CR>y`).
    resume: Mode,
    anchor: Pos,
    count: Option<usize>,
    pub pending: Option<char>,
    /// The pending yank was started with `gy`: produce HTML, not plain text.
    html_op: bool,
    pattern: String,
    forward: bool,
    pub matches: Vec<(Pos, usize)>,
    pub msg: Option<String>,
    /// Text waiting to be shipped to the clipboard by the I/O layer.
    pub yank: Option<String>,
}

impl App {
    pub fn new(paras: Vec<Paragraph>, name: String, measure: usize) -> Self {
        App {
            paras,
            name,
            rows: Vec::new(),
            measure,
            text_width: 0,
            height: 1,
            top: 0,
            cur: 0,
            col: 0,
            want_col: 0,
            mode: Mode::Normal,
            resume: Mode::Normal,
            anchor: Pos::default(),
            count: None,
            pending: None,
            html_op: false,
            pattern: String::new(),
            forward: true,
            matches: Vec::new(),
            msg: None,
            yank: None,
        }
    }

    /// Called by the renderer every frame; re-wraps only when width changes.
    pub fn resize(&mut self, cols: u16, text_rows: u16) {
        self.height = (text_rows as usize).max(1);
        let w = self.measure.min((cols as usize).saturating_sub(4)).max(8);
        if w != self.text_width || self.rows.is_empty() {
            let (here, top) = (self.pos(), self.row_pos(self.top));
            self.text_width = w;
            self.rows = layout::layout(&mut self.paras, w);
            self.top = self.locate(top).0;
            self.jump(here);
        }
        self.follow();
    }

    // ---- position helpers -------------------------------------------------

    fn row_text(&self, r: usize) -> &str {
        let row = &self.rows[r];
        self.paras
            .get(row.para)
            .map_or("", |p| &p.text[row.start..row.end])
    }

    pub fn row_len(&self, r: usize) -> usize {
        self.row_text(r).chars().count()
    }

    fn row_pos(&self, r: usize) -> Pos {
        self.rows.get(r).map_or(Pos::default(), |row| Pos {
            para: row.para,
            byte: row.start,
        })
    }

    /// Cursor as an absolute document position.
    pub fn pos(&self) -> Pos {
        let Some(row) = self.rows.get(self.cur) else {
            return Pos::default();
        };
        let off = self
            .row_text(self.cur)
            .char_indices()
            .nth(self.col)
            .map_or(row.end - row.start, |(i, _)| i);
        Pos {
            para: row.para,
            byte: row.start + off,
        }
    }

    /// Absolute position → (row, col).
    fn locate(&self, p: Pos) -> (usize, usize) {
        let r = self
            .rows
            .partition_point(|row| (row.para, row.start) <= (p.para, p.byte))
            .saturating_sub(1);
        let row = &self.rows[r];
        let b = p.byte.clamp(row.start, row.end);
        let col = self
            .paras
            .get(row.para)
            .map_or(0, |pa| pa.text[row.start..b].chars().count());
        (r, col)
    }

    fn jump(&mut self, p: Pos) {
        let (r, c) = self.locate(p);
        self.cur = r;
        self.col = c.min(self.row_len(r).saturating_sub(1));
        self.want_col = self.col;
    }

    fn goto_row(&mut self, r: usize) {
        self.cur = r.min(self.rows.len() - 1);
        self.col = self.want_col.min(self.row_len(self.cur).saturating_sub(1));
    }

    fn last_row(&self) -> usize {
        self.rows.len() - 1
    }

    /// Keep the cursor visible with a scrolloff margin.
    fn follow(&mut self) {
        let so = SCROLLOFF.min(self.height.saturating_sub(1) / 2);
        if self.cur < self.top + so {
            self.top = self.cur.saturating_sub(so);
        }
        if self.cur + so >= self.top + self.height {
            self.top = self.cur + so + 1 - self.height;
        }
        self.top = self.top.min(self.rows.len().saturating_sub(self.height));
    }

    /// Scroll the viewport (Ctrl-e/y); drag the cursor along if it falls off.
    fn scroll_view(&mut self, delta: isize) {
        let max_top = self.rows.len().saturating_sub(self.height);
        self.top = self.top.saturating_add_signed(delta).min(max_top);
        let so = SCROLLOFF.min(self.height.saturating_sub(1) / 2);
        let lo = if self.top == 0 { 0 } else { self.top + so };
        let hi = (self.top + self.height)
            .saturating_sub(so + 1)
            .min(self.last_row());
        if self.cur < lo || self.cur > hi {
            self.goto_row(self.cur.clamp(lo, hi.max(lo)));
        }
    }

    // ---- motions ------------------------------------------------------------

    /// Apply a motion. Returns `None` if `c` is not a motion.
    fn motion(&mut self, c: char, n: usize, counted: bool) -> Option<Span> {
        let half = (self.height / 2).max(1);
        let span = match c {
            'j' => {
                self.goto_row(self.cur + n);
                Span::Line
            }
            'k' => {
                self.goto_row(self.cur.saturating_sub(n));
                Span::Line
            }
            'h' => {
                self.col = self.col.saturating_sub(n);
                self.want_col = self.col;
                Span::Exclusive
            }
            'l' => {
                self.col = (self.col + n).min(self.row_len(self.cur).saturating_sub(1));
                self.want_col = self.col;
                Span::Exclusive
            }
            '0' => {
                self.col = 0;
                self.want_col = 0;
                Span::Exclusive
            }
            '^' => {
                self.col = self
                    .row_text(self.cur)
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .count();
                self.want_col = self.col;
                Span::Exclusive
            }
            '$' => {
                self.col = self.row_len(self.cur).saturating_sub(1);
                self.want_col = usize::MAX;
                Span::Inclusive
            }
            'w' => {
                for _ in 0..n {
                    let p = self.word_fwd(self.pos());
                    self.jump(p)
                }
                Span::Exclusive
            }
            'b' => {
                for _ in 0..n {
                    let p = self.word_back(self.pos());
                    self.jump(p)
                }
                Span::Exclusive
            }
            'e' => {
                for _ in 0..n {
                    let p = self.word_end(self.pos());
                    self.jump(p)
                }
                Span::Inclusive
            }
            '}' => {
                let p = self.pos();
                let para = (p.para + n).min(self.paras.len().saturating_sub(1));
                if para == p.para {
                    self.goto_row(self.last_row())
                } else {
                    self.jump(Pos { para, byte: 0 })
                }
                Span::Exclusive
            }
            '{' => {
                let p = self.pos();
                let back = if p.byte > 0 { n - 1 } else { n };
                self.jump(Pos {
                    para: p.para.saturating_sub(back),
                    byte: 0,
                });
                Span::Exclusive
            }
            'G' => {
                self.want_col = 0;
                self.goto_row(if counted { n - 1 } else { self.last_row() });
                Span::Line
            }
            '%' if counted => {
                self.goto_row(self.rows.len() * n.min(100) / 100);
                Span::Line
            }
            'H' => {
                self.goto_row(self.top);
                Span::Line
            }
            'M' => {
                self.goto_row(self.top + self.height.min(self.rows.len() - self.top) / 2);
                Span::Line
            }
            'L' => {
                self.goto_row((self.top + self.height).saturating_sub(1));
                Span::Line
            }
            '\x04' => {
                self.goto_row(self.cur + half);
                self.top += half;
                Span::Line
            } // Ctrl-d
            '\x15' => {
                self.goto_row(self.cur.saturating_sub(half));
                self.top = self.top.saturating_sub(half);
                Span::Line
            } // Ctrl-u
            '\x06' | ' ' => {
                // Ctrl-f / Space
                let page = self.height.saturating_sub(2).max(1) * n;
                self.top = (self.top + page).min(self.rows.len().saturating_sub(self.height));
                self.goto_row(self.cur + page);
                Span::Line
            }
            '\x02' => {
                // Ctrl-b
                let page = self.height.saturating_sub(2).max(1) * n;
                self.top = self.top.saturating_sub(page);
                self.goto_row(self.cur.saturating_sub(page));
                Span::Line
            }
            _ => return None,
        };
        Some(span)
    }

    fn word_fwd(&self, p: Pos) -> Pos {
        let t = &self.paras[p.para].text;
        let mut it = t[p.byte..]
            .char_indices()
            .map(|(i, c)| (p.byte + i, c))
            .peekable();
        if let Some(&(_, c0)) = it.peek() {
            let k0 = class(c0);
            if k0 != 0 {
                while it.next_if(|&(_, c)| class(c) == k0).is_some() {}
            }
            while it.next_if(|&(_, c)| class(c) == 0).is_some() {}
            if let Some((i, _)) = it.next() {
                return Pos {
                    para: p.para,
                    byte: i,
                };
            }
        }
        if p.para + 1 < self.paras.len() {
            Pos {
                para: p.para + 1,
                byte: 0,
            }
        } else {
            p
        }
    }

    fn word_back(&self, p: Pos) -> Pos {
        if p.byte == 0 {
            if p.para == 0 {
                return p;
            }
            let prev = p.para - 1;
            return self.word_back(Pos {
                para: prev,
                byte: self.paras[prev].text.len(),
            });
        }
        let t = &self.paras[p.para].text;
        let chars: Vec<(usize, char)> = t[..p.byte].char_indices().collect();
        let mut i = chars.len();
        while i > 0 && class(chars[i - 1].1) == 0 {
            i -= 1;
        }
        if i == 0 {
            return Pos {
                para: p.para,
                byte: 0,
            };
        }
        let k = class(chars[i - 1].1);
        while i > 0 && class(chars[i - 1].1) == k {
            i -= 1;
        }
        Pos {
            para: p.para,
            byte: chars[i].0,
        }
    }

    fn word_end(&self, p: Pos) -> Pos {
        let t = &self.paras[p.para].text;
        let mut it = t[p.byte..]
            .char_indices()
            .map(|(i, c)| (p.byte + i, c))
            .skip(1)
            .peekable();
        while it.next_if(|&(_, c)| class(c) == 0).is_some() {}
        let Some((mut last, c0)) = it.next() else {
            return if p.para + 1 < self.paras.len() {
                self.word_end(Pos {
                    para: p.para + 1,
                    byte: 0,
                })
            } else {
                p
            };
        };
        while let Some((i, _)) = it.next_if(|&(_, c)| class(c) == class(c0)) {
            last = i;
        }
        Pos {
            para: p.para,
            byte: last,
        }
    }

    // ---- selection & yank ---------------------------------------------------

    /// Byte position just past the character at `p`.
    fn after(&self, p: Pos) -> Pos {
        let t = &self.paras[p.para].text;
        let n = t[p.byte.min(t.len())..]
            .chars()
            .next()
            .map_or(0, char::len_utf8);
        Pos {
            byte: p.byte + n,
            ..p
        }
    }

    fn row_span(&self, a: usize, b: usize) -> (Pos, Pos) {
        let (a, b) = (a.min(b), a.max(b));
        let (ra, rb) = (&self.rows[a], &self.rows[b]);
        (
            Pos {
                para: ra.para,
                byte: ra.start,
            },
            Pos {
                para: rb.para,
                byte: rb.end,
            },
        )
    }

    /// Active visual selection as a half-open range `[lo, hi)`.
    pub fn selection(&self) -> Option<(Pos, Pos)> {
        match self.mode {
            Mode::Visual => {
                let (a, b) = (self.anchor.min(self.pos()), self.anchor.max(self.pos()));
                Some((a, self.after(b)))
            }
            Mode::VisualLine => Some(self.row_span(self.locate(self.anchor).0, self.cur)),
            _ => None,
        }
    }

    fn yank_range(&mut self, lo: Pos, hi: Pos) {
        self.yank_as(lo, hi, false);
    }

    /// Yank `[lo, hi)`. `linewise` (V, `yj`, `yy`…) means whole paragraphs,
    /// which only matters for HTML: it never emits half a `<p>`.
    fn yank_as(&mut self, lo: Pos, hi: Pos, linewise: bool) {
        if std::mem::take(&mut self.html_op) {
            let out = if linewise {
                html::render_paras(&self.paras, lo.para.min(hi.para), lo.para.max(hi.para))
            } else {
                html::render(&self.paras, lo, hi)
            };
            self.msg = Some(format!("yanked {} chars of HTML", out.chars().count()));
            self.yank = Some(out);
            self.mode = Mode::Normal;
            self.jump(lo);
            return;
        }
        let text = layout::extract(&self.paras, lo, hi);
        let lines = text.lines().count().max(1);
        self.msg = Some(match lines {
            1 => format!("yanked {} chars", text.chars().count()),
            n => format!("yanked {n} lines"),
        });
        self.yank = Some(text);
        self.mode = Mode::Normal;
        self.jump(lo);
    }

    fn yank_paragraph(&mut self, n: usize) {
        let p = self.pos().para;
        let last = (p + n - 1).min(self.paras.len().saturating_sub(1));
        if self.paras.is_empty() {
            return;
        }
        let end = self.paras[last].text.len();
        self.yank_as(
            Pos { para: p, byte: 0 },
            Pos {
                para: last,
                byte: end,
            },
            true,
        );
    }

    /// `y{motion}`: run the motion, yank what it covered, restore the cursor.
    fn operator_yank(&mut self, c: char, n: usize, counted: bool) {
        let (start, start_row) = (self.pos(), self.cur);
        let Some(span) = self.motion(c, n, counted) else {
            return;
        };
        let end = self.pos();
        let (lo, hi) = match span {
            Span::Line => self.row_span(start_row, self.cur),
            Span::Exclusive => (start.min(end), start.max(end)),
            Span::Inclusive => (start.min(end), self.after(start.max(end))),
        };
        self.yank_as(lo, hi, span == Span::Line);
    }

    // ---- search ---------------------------------------------------------------

    fn search_next(&mut self, n: usize, forward: bool) {
        if self.matches.is_empty() {
            self.msg = Some(if self.pattern.is_empty() {
                "no previous pattern".into()
            } else {
                format!("pattern not found: {}", self.pattern)
            });
            return;
        }
        for _ in 0..n {
            let here = self.pos();
            let next = if forward {
                self.matches.iter().find(|m| m.0 > here).or_else(|| {
                    self.msg = Some("search hit BOTTOM, continuing at TOP".into());
                    self.matches.first()
                })
            } else {
                self.matches.iter().rev().find(|m| m.0 < here).or_else(|| {
                    self.msg = Some("search hit TOP, continuing at BOTTOM".into());
                    self.matches.last()
                })
            };
            if let Some(&(p, _)) = next {
                self.jump(p);
            }
        }
        let idx = self
            .matches
            .iter()
            .position(|m| m.0 == self.pos())
            .map_or(0, |i| i + 1);
        if self.msg.is_none() {
            self.msg = Some(format!("[{}/{}]", idx, self.matches.len()));
        }
    }

    // ---- key dispatch ---------------------------------------------------------

    pub fn handle_key(&mut self, k: KeyEvent) -> Flow {
        self.msg = None;
        let flow = match self.mode {
            Mode::Search { .. } | Mode::Command(_) => self.prompt_key(k),
            _ => self.normal_key(k),
        };
        self.follow();
        flow
    }

    fn normal_key(&mut self, k: KeyEvent) -> Flow {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // Normalize special keys and Ctrl chords to the char grammar.
        let c = match (k.code, ctrl) {
            (KeyCode::Char('c'), true) => return Flow::Quit,
            (KeyCode::Char('e'), true) => {
                let n = self.take_count() as isize;
                self.scroll_view(n);
                return Flow::Continue;
            }
            (KeyCode::Char('y'), true) => {
                let n = self.take_count() as isize;
                self.scroll_view(-n);
                return Flow::Continue;
            }
            (KeyCode::Char(ch @ ('d' | 'u' | 'f' | 'b')), true) => (ch as u8 - b'a' + 1) as char,
            (KeyCode::Char(ch), false) => ch,
            (KeyCode::Down | KeyCode::Enter, _) => 'j',
            (KeyCode::Up, _) => 'k',
            (KeyCode::Left, _) => 'h',
            (KeyCode::Right, _) => 'l',
            (KeyCode::PageDown, _) => '\x06',
            (KeyCode::PageUp, _) => '\x02',
            (KeyCode::Home, _) => '0',
            (KeyCode::End, _) => '$',
            (KeyCode::Esc, _) => {
                self.count = None;
                self.pending = None;
                self.html_op = false;
                self.mode = Mode::Normal;
                return Flow::Continue;
            }
            _ => return Flow::Continue,
        };

        if c.is_ascii_digit() && (c != '0' || self.count.is_some()) {
            self.count = Some(
                self.count
                    .unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(c as usize - '0' as usize),
            );
            return Flow::Continue;
        }
        let counted = self.count.is_some();
        let n = self.take_count();
        let visual = matches!(self.mode, Mode::Visual | Mode::VisualLine);

        if let Some(p) = self.pending.take() {
            match (p, c) {
                ('g', 'g') => {
                    self.want_col = 0;
                    self.goto_row(if counted { n - 1 } else { 0 })
                }
                // `gy` = HTML yank: in Visual it acts at once, otherwise it
                // becomes an operator ('h') that takes the same motions as `y`.
                ('g', 'y') if visual => {
                    self.html_op = true;
                    let linewise = self.mode == Mode::VisualLine;
                    if let Some((lo, hi)) = self.selection() {
                        self.yank_as(lo, hi, linewise);
                    }
                }
                ('g', 'y') => {
                    self.html_op = true;
                    self.pending = Some('h');
                }
                ('y' | 'h', 'y') => self.yank_paragraph(n),
                ('y' | 'h', 'g') => self.pending = Some('Y'), // yg → wait for second g
                ('Y', 'g') => {
                    let r = self.cur;
                    self.goto_row(0);
                    let (lo, hi) = self.row_span(0, r);
                    self.yank_as(lo, hi, true)
                }
                ('y' | 'h', m) => self.operator_yank(m, n, counted),
                ('z', 'z') => self.top = self.cur.saturating_sub(self.height / 2),
                ('z', 't') => self.top = self.cur,
                ('z', 'b') => self.top = self.cur.saturating_sub(self.height - 1),
                _ => {}
            }
            if self.pending.is_none() {
                self.html_op = false; // operator finished or was abandoned
            }
            return Flow::Continue;
        }

        if self.motion(c, n, counted).is_some() {
            return Flow::Continue;
        }
        match c {
            'g' | 'z' => self.pending = Some(c),
            'y' if visual => {
                if let Some((lo, hi)) = self.selection() {
                    self.yank_range(lo, hi);
                }
            }
            'y' => self.pending = Some('y'),
            // Shift+y = formatted: lowercase yanks plain text, uppercase HTML.
            'Y' if visual => {
                self.html_op = true;
                let linewise = self.mode == Mode::VisualLine;
                if let Some((lo, hi)) = self.selection() {
                    self.yank_as(lo, hi, linewise);
                }
            }
            'Y' => {
                self.html_op = true;
                self.yank_paragraph(n);
            }
            'v' => self.toggle_visual(Mode::Visual),
            'V' => self.toggle_visual(Mode::VisualLine),
            'o' if visual => {
                let here = self.pos();
                self.jump(self.anchor);
                self.anchor = here;
            }
            '/' | '?' => {
                self.resume = if visual {
                    self.mode.clone()
                } else {
                    Mode::Normal
                };
                self.mode = Mode::Search {
                    buf: String::new(),
                    forward: c == '/',
                };
            }
            'n' => self.search_next(n, self.forward),
            'N' => self.search_next(n, !self.forward),
            ':' => self.mode = Mode::Command(String::new()),
            'q' if !visual => return Flow::Quit,
            'q' => self.mode = Mode::Normal,
            _ => {}
        }
        Flow::Continue
    }

    fn toggle_visual(&mut self, m: Mode) {
        if self.mode == m {
            self.mode = Mode::Normal;
        } else {
            if self.mode == Mode::Normal {
                self.anchor = self.pos();
            }
            self.mode = m;
        }
    }

    fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1).max(1)
    }

    fn prompt_key(&mut self, k: KeyEvent) -> Flow {
        let buf = match &mut self.mode {
            Mode::Search { buf, .. } | Mode::Command(buf) => buf,
            _ => unreachable!(),
        };
        match k.code {
            KeyCode::Char(c) => buf.push(c),
            KeyCode::Backspace if !buf.is_empty() => {
                buf.pop();
            }
            KeyCode::Esc | KeyCode::Backspace => {
                self.mode = std::mem::replace(&mut self.resume, Mode::Normal);
                self.matches = layout::search(&self.paras, &self.pattern);
                return Flow::Continue;
            }
            KeyCode::Enter => return self.submit_prompt(),
            _ => {}
        }
        // Incremental highlight while typing the pattern.
        if let Mode::Search { buf, .. } = &self.mode {
            self.matches = layout::search(&self.paras, buf);
        }
        Flow::Continue
    }

    fn submit_prompt(&mut self) -> Flow {
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Search { buf, forward } => {
                if !buf.is_empty() {
                    self.pattern = buf;
                }
                self.forward = forward;
                self.mode = std::mem::replace(&mut self.resume, Mode::Normal);
                self.matches = layout::search(&self.paras, &self.pattern);
                self.search_next(1, forward);
            }
            Mode::Command(cmd) => match cmd.trim() {
                "q" | "q!" | "qa" | "wq" | "x" => return Flow::Quit,
                "noh" | "nohlsearch" => self.matches.clear(),
                s => match s.parse::<usize>() {
                    // `:N` jumps to paragraph N (the unit shown in the status line).
                    Ok(n) => self.jump(Pos {
                        para: n.saturating_sub(1).min(self.paras.len().saturating_sub(1)),
                        byte: 0,
                    }),
                    Err(_) => self.msg = Some(format!("not a command: {s}")),
                },
            },
            _ => {}
        }
        Flow::Continue
    }
}

/// Vim word classes: 0 whitespace, 1 keyword, 2 punctuation.
fn class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docx::Kind;

    fn app() -> App {
        let paras = [
            "The quick brown fox jumps over the lazy dog.",
            "Second paragraph here.",
            "Third.",
        ]
        .map(|t| Paragraph::new(t, Kind::Body))
        .to_vec();
        let mut a = App::new(paras, "t.docx".into(), 20);
        a.resize(24, 10);
        a
    }

    fn keys(a: &mut App, s: &str) {
        for c in s.chars() {
            let code = if c == '\n' {
                KeyCode::Enter
            } else if c == '\x1b' {
                KeyCode::Esc
            } else {
                KeyCode::Char(c)
            };
            a.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
    }

    #[test]
    fn yank_word_and_paragraph() {
        let mut a = app();
        keys(&mut a, "wyw");
        assert_eq!(a.yank.take().as_deref(), Some("quick "));
        keys(&mut a, "yy");
        assert_eq!(a.yank.take().unwrap(), a.paras[0].text);
    }

    #[test]
    fn visual_yank_across_wrapped_rows_has_no_fake_newlines() {
        let mut a = app();
        assert!(a.rows.len() > 4, "paragraph should wrap at width 20");
        keys(&mut a, "wv5ey");
        assert_eq!(a.yank.take().as_deref(), Some("quick brown fox jumps over"));
    }

    #[test]
    fn visual_yank_across_paragraphs() {
        let mut a = app();
        keys(&mut a, "/lazy\nv/here\ney");
        assert_eq!(
            a.yank.take().as_deref(),
            Some("lazy dog.\nSecond paragraph here")
        );
    }

    #[test]
    fn html_yank() {
        let mut a = app();
        a.paras[0].marks = vec![crate::docx::Mark {
            start: 4,
            end: 9,
            flags: crate::numbering::BOLD,
        }];
        keys(&mut a, "gyy");
        assert_eq!(
            a.yank.take().as_deref(),
            Some("<p>The <strong>quick</strong> brown fox jumps over the lazy dog.</p>")
        );
        keys(&mut a, "wvegy"); // charwise inside one paragraph → fragment
        assert_eq!(a.yank.take().as_deref(), Some("<strong>quick</strong>"));
        keys(&mut a, "gyj"); // linewise → whole paragraphs, even from mid-paragraph
        assert!(a.yank.take().unwrap().starts_with("<p>The <strong>"));
        keys(&mut a, "yy"); // plain yank is untouched afterwards
        assert_eq!(a.yank.take().unwrap(), a.paras[0].text);
        keys(&mut a, "Y"); // Shift+y: current paragraph as HTML
        assert!(a.yank.take().unwrap().starts_with("<p>The <strong>quick"));
        keys(&mut a, "wveY"); // ...or the visual selection
        assert_eq!(a.yank.take().as_deref(), Some("<strong>quick</strong>"));
        keys(&mut a, "gy\x1byy"); // Esc cancels the HTML operator
        assert_eq!(a.yank.take().unwrap(), a.paras[0].text);
    }

    #[test]
    fn counts_and_jumps() {
        let mut a = app();
        keys(&mut a, "G");
        assert_eq!(a.cur, a.rows.len() - 1);
        keys(&mut a, "gg");
        assert_eq!(a.cur, 0);
        keys(&mut a, "2}");
        assert_eq!(a.pos(), Pos { para: 2, byte: 0 });
        keys(&mut a, ":2\n");
        assert_eq!(a.pos(), Pos { para: 1, byte: 0 });
    }
}
