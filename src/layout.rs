//! View model: soft-wraps paragraphs into display rows.
//!
//! Wrapping is *view-only*. Every row is a byte range into its source
//! paragraph, so a yank always slices the original text: wrapped rows never
//! inject fake newlines into the clipboard, and resizing never changes what
//! gets copied.

use unicode_width::UnicodeWidthChar;

use crate::docx::{Kind, Paragraph};

/// Absolute position in the document: (paragraph index, byte offset).
/// Derived `Ord` gives document order for free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Pos {
    pub para: usize,
    pub byte: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct Row {
    pub para: usize,
    pub start: usize,
    pub end: usize,
    /// First row of its paragraph (where a list bullet is drawn).
    pub first: bool,
    /// Blank spacer between paragraphs; selectable, contributes no text.
    pub gap: bool,
}

/// Visual gap between table columns (rendered as " │ ").
pub const COL_SEP: usize = 3;
const DEFAULT_TAB: usize = 4;

pub fn char_width(c: char) -> usize {
    match c {
        '\t' => DEFAULT_TAB,
        '\n' => 0,
        c => c.width().unwrap_or(0),
    }
}

fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Display width of the `nth` tab in `p`.
pub fn tab_width(p: &Paragraph, nth: usize) -> usize {
    p.tabs.get(nth).map_or(DEFAULT_TAB, |&w| w as usize)
}

/// Display width of `p.text[from..to]`, honoring per-tab widths.
pub fn width_between(p: &Paragraph, from: usize, to: usize) -> usize {
    let mut nth = p.text[..from].matches('\t').count();
    let mut w = 0;
    for c in p.text[from..to].chars() {
        if c == '\t' {
            w += tab_width(p, nth);
            nth += 1;
        } else {
            w += char_width(c);
        }
    }
    w
}

/// Left gutter for a paragraph: nesting indent + list/heading marker.
pub fn indent(p: &Paragraph) -> usize {
    let nest = if p.kind == Kind::ListItem {
        p.level as usize * 2
    } else {
        0
    };
    let m = str_width(&p.marker);
    nest + if m > 0 { m + 1 } else { 0 }
}

pub fn layout(paras: &mut [Paragraph], width: usize) -> Vec<Row> {
    align_tables(paras, width);
    let mut rows = Vec::with_capacity(paras.len() * 3);
    for i in 0..paras.len() {
        let p = &paras[i];
        let w = width.saturating_sub(indent(p)).max(8);
        for (n, (start, end)) in wrap(p, w).into_iter().enumerate() {
            rows.push(Row {
                para: i,
                start,
                end,
                first: n == 0,
                gap: false,
            });
        }
        // Consecutive list items / table rows / notes stay tight. Two lists
        // only separate when both sides are top-level (nested sub-lists often
        // carry their own numId but belong to the parent visually).
        let tight = matches!(p.kind, Kind::ListItem | Kind::TableRow | Kind::Note)
            && paras.get(i + 1).is_some_and(|n| {
                n.kind == p.kind
                    && (n.group == p.group
                        || (p.kind == Kind::ListItem && (p.level > 0 || n.level > 0)))
            });
        if i + 1 < paras.len() && !tight {
            let len = p.text.len();
            rows.push(Row {
                para: i,
                start: len,
                end: len,
                first: false,
                gap: true,
            });
        }
    }
    if rows.is_empty() {
        rows.push(Row {
            para: 0,
            start: 0,
            end: 0,
            first: true,
            gap: true,
        });
    }
    rows
}

/// Give each table's tabs the widths that line its columns up. A table too
/// wide for the measure falls back to a plain " │ " separator and wraps.
fn align_tables(paras: &mut [Paragraph], width: usize) {
    let mut i = 0;
    while i < paras.len() {
        if paras[i].kind != Kind::TableRow {
            paras[i].tabs.clear();
            i += 1;
            continue;
        }
        let g = paras[i].group;
        let j = i + paras[i..]
            .iter()
            .take_while(|p| p.kind == Kind::TableRow && p.group == g)
            .count();
        let table = &mut paras[i..j];
        let cells: Vec<Vec<usize>> = table
            .iter()
            .map(|p| p.text.split('\t').map(str_width).collect())
            .collect();
        let ncols = cells.iter().map(Vec::len).max().unwrap_or(0);
        let mut colw = vec![0; ncols];
        for row in &cells {
            for (k, &w) in row.iter().enumerate() {
                colw[k] = colw[k].max(w);
            }
        }
        let total = colw.iter().sum::<usize>() + COL_SEP * ncols.saturating_sub(1);
        let fits = total <= width;
        for (p, row) in table.iter_mut().zip(&cells) {
            p.tabs = (0..row.len().saturating_sub(1))
                .map(|k| {
                    (if fits {
                        colw[k] - row[k] + COL_SEP
                    } else {
                        COL_SEP
                    }) as u16
                })
                .collect();
        }
        i = j;
    }
}

/// Greedy word wrap. Returns byte ranges; the breaking space and hard `\n`
/// are excluded from rows but remain in the source text.
fn wrap(p: &Paragraph, width: usize) -> Vec<(usize, usize)> {
    let text = p.text.as_str();
    let mut rows = Vec::new();
    let (mut start, mut col, mut last_space) = (0usize, 0usize, None::<(usize, usize)>);
    let mut nth_tab = 0;
    for (i, c) in text.char_indices() {
        if c == '\n' {
            rows.push((start, i));
            (start, col, last_space) = (i + 1, 0, None);
            continue;
        }
        let w = if c == '\t' {
            nth_tab += 1;
            tab_width(p, nth_tab - 1)
        } else {
            char_width(c)
        };
        if c == ' ' && col + w > width {
            // The space at the margin is itself the break.
            rows.push((start, i));
            (start, col, last_space) = (i + 1, 0, None);
            continue;
        }
        if col + w > width && i > start {
            match last_space {
                Some((sp, sp_col)) if sp > start => {
                    rows.push((start, sp));
                    start = sp + 1;
                    col -= sp_col; // width of the word carried to the new row
                }
                _ => {
                    rows.push((start, i));
                    start = i;
                    col = 0;
                }
            }
            last_space = None;
        }
        col += w;
        if c == ' ' {
            last_space = Some((i, col)); // col *after* the space
        }
    }
    rows.push((start, text.len()));
    rows
}

/// Copy the text in `[from, to)` straight from the source paragraphs.
pub fn extract(paras: &[Paragraph], from: Pos, to: Pos) -> String {
    let (from, to) = if from <= to { (from, to) } else { (to, from) };
    if from.para == to.para {
        let t = &paras[from.para].text;
        return t[from.byte.min(t.len())..to.byte.min(t.len())].to_string();
    }
    let mut s = String::new();
    s.push_str(&paras[from.para].text[from.byte..]);
    for p in &paras[from.para + 1..to.para] {
        s.push('\n');
        s.push_str(&p.text);
    }
    s.push('\n');
    s.push_str(&paras[to.para].text[..to.byte]);
    s
}

/// Literal search, Vim "smartcase": case-insensitive unless the pattern has
/// an uppercase letter. Returns byte ranges into the original text.
pub fn search(paras: &[Paragraph], pat: &str) -> Vec<(Pos, usize)> {
    let needle: Vec<char> = pat.chars().collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let fold = !pat.chars().any(char::is_uppercase);
    if pat.is_ascii() {
        return search_ascii(paras, pat, fold);
    }
    let eq = |a: char, b: char| {
        if fold {
            a == b || a.to_lowercase().eq(b.to_lowercase())
        } else {
            a == b
        }
    };
    let mut hits = Vec::new();
    for (pi, p) in paras.iter().enumerate() {
        let mut next_ok = 0;
        for (i, _) in p.text.char_indices() {
            if i < next_ok {
                continue;
            }
            let mut it = p.text[i..].char_indices();
            let mut end = i;
            let matched = needle.iter().all(|&n| match it.next() {
                Some((j, h)) if eq(h, n) => {
                    end = i + j + h.len_utf8();
                    true
                }
                _ => false,
            });
            if matched {
                hits.push((Pos { para: pi, byte: i }, end));
                next_ok = end;
            }
        }
    }
    hits
}

/// Fast path: ASCII-only folding keeps every byte offset identical, so the
/// std two-way substring search can run on a folded copy.
fn search_ascii(paras: &[Paragraph], pat: &str, fold: bool) -> Vec<(Pos, usize)> {
    let mut hits = Vec::new();
    let mut buf = String::new();
    for (pi, p) in paras.iter().enumerate() {
        let hay = if fold {
            buf.clear();
            buf.push_str(&p.text);
            buf.make_ascii_lowercase();
            buf.as_str()
        } else {
            p.text.as_str()
        };
        hits.extend(
            hay.match_indices(pat)
                .map(|(i, m)| (Pos { para: pi, byte: i }, i + m.len())),
        );
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn para(t: &str) -> Paragraph {
        Paragraph::new(t, Kind::Body)
    }

    #[test]
    fn wraps_on_spaces_and_hard_breaks() {
        assert_eq!(wrap(&para("aaa bbb ccc"), 7), [(0, 7), (8, 11)]);
        assert_eq!(wrap(&para("abcdefghij"), 4), [(0, 4), (4, 8), (8, 10)]);
        assert_eq!(wrap(&para("ab\ncd"), 10), [(0, 2), (3, 5)]);
    }

    #[test]
    fn extract_ignores_wrapping() {
        let mut ps = [para("hello wide world"), para("second")];
        let rows = layout(&mut ps, 8);
        assert!(rows.len() > 3);
        let s = extract(&ps, Pos { para: 0, byte: 6 }, Pos { para: 1, byte: 3 });
        assert_eq!(s, "wide world\nsec");
    }

    #[test]
    fn smartcase() {
        let ps = [para("Rust rust RUST")];
        assert_eq!(search(&ps, "rust").len(), 3);
        assert_eq!(search(&ps, "Rust").len(), 1);
        // Non-ASCII pattern takes the Unicode folding path.
        let ps = [para("Ärger ärger")];
        assert_eq!(search(&ps, "ärger").len(), 2);
        // ASCII pattern over non-ASCII text keeps byte offsets right.
        let ps = [para("naïve Naïve café")];
        assert_eq!(search(&ps, "caf"), [(Pos { para: 0, byte: 14 }, 17)]);
    }

    #[test]
    fn tables_align_when_they_fit() {
        let row = |t: &str| Paragraph {
            group: 1,
            ..Paragraph::new(t, Kind::TableRow)
        };
        let mut ps = [
            row("Name\tQty"),
            row("Widget\t3"),
            Paragraph::new("x", Kind::Body),
            row("a\tb"),
        ];
        ps[3].group = 2;
        layout(&mut ps, 40);
        // "Name" padded to "Widget" (6) + separator.
        assert_eq!(ps[0].tabs, [6 - 4 + COL_SEP as u16]);
        assert_eq!(ps[1].tabs, [COL_SEP as u16]);
        assert_eq!(ps[3].tabs, [COL_SEP as u16], "separate table aligns alone");
        assert_eq!(width_between(&ps[0], 0, ps[0].text.len()), 6 + COL_SEP + 3);
        layout(&mut ps, 8); // too narrow: plain separators
        assert_eq!(ps[0].tabs, [COL_SEP as u16]);
    }
}
