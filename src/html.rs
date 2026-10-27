//! HTML yank (`gy`): render a document range as clean, semantic HTML.
//!
//! Only meaning survives, never presentation: `<strong> <em> <u> <s> <sup>
//! <sub> <a> <br>` inline; `<p> <h1–6> <ol>/<ul>/<li> <table>/<tr>/<th>/<td>`
//! as blocks. No classes, no inline styles, no fonts or colors: the output is
//! ready to drop into a CMS or a code editor.
//!
//! A charwise selection inside one paragraph yields an inline fragment (no
//! `<p>`), the way copying a few words from a web page would.

use crate::docx::{Kind, Paragraph};
use crate::layout::Pos;
use crate::numbering::{BOLD, ITALIC, STRIKE, SUB, SUP, UNDERLINE};

/// Render `[lo, hi)`.
pub fn render(paras: &[Paragraph], lo: Pos, hi: Pos) -> String {
    let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
    if paras.is_empty() || lo == hi {
        return String::new();
    }
    let first = &paras[lo.para];
    let whole = lo.byte == 0 && hi.byte >= first.text.len();
    if lo.para == hi.para && !whole && first.kind != Kind::TableRow {
        return inline(first, lo.byte, hi.byte);
    }

    let mut b = Blocks::default();
    let last = hi.para.min(paras.len() - 1);
    for (i, p) in paras.iter().enumerate().take(last + 1).skip(lo.para) {
        let from = if i == lo.para {
            lo.byte.min(p.text.len())
        } else {
            0
        };
        let to = if i == hi.para {
            hi.byte.min(p.text.len())
        } else {
            p.text.len()
        };
        if i == hi.para && i != lo.para && to == 0 {
            break; // selection ends at the very start of this paragraph
        }
        b.push(p, from, to);
    }
    b.finish()
}

/// Whole paragraphs `a..=b` (linewise yanks).
pub fn render_paras(paras: &[Paragraph], a: usize, b: usize) -> String {
    if paras.is_empty() {
        return String::new();
    }
    let (a, b) = (a.min(b), a.max(b).min(paras.len() - 1));
    let mut s = Blocks::default();
    for p in &paras[a..=b] {
        s.push(p, 0, p.text.len());
    }
    s.finish()
}

#[derive(Clone, Copy, PartialEq)]
struct ListFrame {
    ordered: bool,
    group: u32,
    li_open: bool,
    li_child: bool,
}

#[derive(Default)]
struct Blocks {
    out: String,
    lists: Vec<ListFrame>,
    base_level: u8,
    table: Option<u32>,
}

impl Blocks {
    fn push(&mut self, p: &Paragraph, from: usize, to: usize) {
        if p.kind != Kind::ListItem {
            self.close_lists();
        }
        if p.kind != Kind::TableRow || self.table != Some(p.group) {
            self.close_table();
        }
        match p.kind {
            Kind::Heading => {
                let h = p.hlevel.clamp(1, 6);
                self.out
                    .push_str(&format!("<h{h}>{}</h{h}>\n", inline(p, from, to)));
            }
            Kind::Body | Kind::Note => self
                .out
                .push_str(&format!("<p>{}</p>\n", inline(p, from, to))),
            Kind::ListItem => self.list_item(p, from, to),
            Kind::TableRow => {
                if self.table.is_none() {
                    self.out.push_str("<table>\n");
                    self.table = Some(p.group);
                }
                let tag = if p.header { "th" } else { "td" };
                self.out.push_str("  <tr>");
                let mut start = 0;
                for cell in p.text.split('\t') {
                    let (a, b) = (start, start + cell.len());
                    start = b + 1;
                    let (a, b) = (a.max(from), b.min(to));
                    if a < b || (a == b && cell.is_empty()) {
                        self.out
                            .push_str(&format!("<{tag}>{}</{tag}>", inline(p, a, b)));
                    }
                }
                self.out.push_str("</tr>\n");
            }
        }
    }

    fn list_item(&mut self, p: &Paragraph, from: usize, to: usize) {
        if self.lists.is_empty() {
            self.base_level = p.level;
        }
        let depth = p.level.saturating_sub(self.base_level) as usize + 1;
        let ordered = p
            .list
            .as_ref()
            .is_some_and(|l| !matches!(l.fmt.as_str(), "bullet" | "none"));

        while self.lists.len() > depth {
            self.close_list();
        }
        if let Some(top) = self.lists.last()
            && self.lists.len() == depth
        {
            // A different list at the same depth starts a new <ol>/<ul>.
            if top.ordered != ordered || (depth == 1 && top.group != p.group) {
                self.close_list();
            } else {
                self.close_li();
            }
        }
        while self.lists.len() < depth {
            let k = self.lists.len();
            if let Some(parent) = self.lists.last_mut() {
                if parent.li_open {
                    parent.li_child = true;
                    self.out.push('\n');
                } else {
                    // Jumped a level with no parent item: give it one.
                    self.out
                        .push_str(&format!("{}<li>\n", pad(4 * (k - 1) + 2)));
                    parent.li_open = true;
                    parent.li_child = true;
                }
            }
            let tag = if ordered { "ol" } else { "ul" };
            let mut attrs = String::new();
            if let Some(l) = p
                .list
                .as_ref()
                .filter(|_| ordered && self.lists.len() + 1 == depth)
            {
                let ty = match l.fmt.as_str() {
                    "lowerLetter" => Some("a"),
                    "upperLetter" => Some("A"),
                    "lowerRoman" => Some("i"),
                    "upperRoman" => Some("I"),
                    _ => None,
                };
                if let Some(t) = ty {
                    attrs.push_str(&format!(" type=\"{t}\""));
                }
                if l.value != 1 {
                    attrs.push_str(&format!(" start=\"{}\"", l.value));
                }
            }
            self.out
                .push_str(&format!("{}<{tag}{attrs}>\n", pad(4 * k)));
            self.lists.push(ListFrame {
                ordered,
                group: p.group,
                li_open: false,
                li_child: false,
            });
        }
        let k = self.lists.len() - 1;
        self.out
            .push_str(&format!("{}<li>{}", pad(4 * k + 2), inline(p, from, to)));
        let top = self.lists.last_mut().expect("list just opened");
        top.li_open = true;
        top.li_child = false;
    }

    fn close_li(&mut self) {
        let k = self.lists.len().saturating_sub(1);
        let Some(top) = self.lists.last_mut() else {
            return;
        };
        if !top.li_open {
            return;
        }
        if top.li_child {
            self.out.push_str(&format!("{}</li>\n", pad(4 * k + 2)));
        } else {
            self.out.push_str("</li>\n");
        }
        top.li_open = false;
    }

    fn close_list(&mut self) {
        self.close_li();
        if let Some(top) = self.lists.pop() {
            let tag = if top.ordered { "ol" } else { "ul" };
            self.out
                .push_str(&format!("{}</{tag}>\n", pad(4 * self.lists.len())));
        }
    }

    fn close_lists(&mut self) {
        while !self.lists.is_empty() {
            self.close_list();
        }
    }

    fn close_table(&mut self) {
        if self.table.take().is_some() {
            self.out.push_str("</table>\n");
        }
    }

    fn finish(mut self) -> String {
        self.close_lists();
        self.close_table();
        while self.out.ends_with('\n') {
            self.out.pop();
        }
        self.out
    }
}

fn pad(n: usize) -> String {
    " ".repeat(n)
}

#[derive(Clone, PartialEq)]
enum Tag {
    A(usize),
    F(u8),
}

const FLAG_TAGS: [(u8, &str); 6] = [
    (BOLD, "strong"),
    (ITALIC, "em"),
    (UNDERLINE, "u"),
    (STRIKE, "s"),
    (SUP, "sup"),
    (SUB, "sub"),
];

/// Inline HTML for `p.text[from..to]`, with minimal correctly-nested tags:
/// `**bold *both* bold**` → `<strong>bold <em>both</em> bold</strong>`.
fn inline(p: &Paragraph, from: usize, to: usize) -> String {
    let (from, to) = (from.min(p.text.len()), to.min(p.text.len()));
    let mut cuts = vec![from, to];
    for m in &p.marks {
        cuts.extend([m.start, m.end]);
    }
    for l in &p.links {
        cuts.extend([l.start, l.end]);
    }
    cuts.retain(|&c| c >= from && c <= to && p.text.is_char_boundary(c));
    cuts.sort_unstable();
    cuts.dedup();

    let mut out = String::new();
    let mut stack: Vec<Tag> = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let mut want = Vec::new();
        if let Some(i) = p.links.iter().position(|l| l.start <= a && a < l.end) {
            want.push(Tag::A(i));
        }
        let flags = p
            .marks
            .iter()
            .find(|m| m.start <= a && a < m.end)
            .map_or(0, |m| m.flags);
        want.extend(
            FLAG_TAGS
                .iter()
                .filter(|(f, _)| flags & f != 0)
                .map(|(f, _)| Tag::F(*f)),
        );

        // Keep the longest prefix of open tags that are still wanted.
        let keep = stack.iter().take_while(|t| want.contains(t)).count();
        while stack.len() > keep {
            close_tag(&mut out, &stack.pop().expect("non-empty"));
        }
        for t in want {
            if !stack.contains(&t) {
                open_tag(&mut out, &t, p);
                stack.push(t);
            }
        }
        escape_text(&mut out, &p.text[a..b]);
    }
    while let Some(t) = stack.pop() {
        close_tag(&mut out, &t);
    }
    out
}

fn tag_name(t: &Tag) -> &'static str {
    match t {
        Tag::A(_) => "a",
        Tag::F(f) => FLAG_TAGS
            .iter()
            .find(|(x, _)| x == f)
            .map_or("span", |(_, n)| n),
    }
}

fn open_tag(out: &mut String, t: &Tag, p: &Paragraph) {
    match t {
        Tag::A(i) => {
            out.push_str("<a href=\"");
            escape_attr(out, &p.links[*i].href);
            out.push_str("\">");
        }
        _ => {
            out.push('<');
            out.push_str(tag_name(t));
            out.push('>');
        }
    }
}

fn close_tag(out: &mut String, t: &Tag) {
    out.push_str("</");
    out.push_str(tag_name(t));
    out.push('>');
}

fn escape_text(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\n' => out.push_str("<br>"),
            c => out.push(c),
        }
    }
}

fn escape_attr(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docx::{Link, Mark};
    use crate::numbering::ListMark;

    fn body(t: &str) -> Paragraph {
        Paragraph::new(t, Kind::Body)
    }

    fn item(t: &str, level: u8, fmt: &str, value: u32, group: u32) -> Paragraph {
        let mut p = Paragraph::new(t, Kind::ListItem);
        p.level = level;
        p.group = group;
        p.list = Some(ListMark {
            text: String::new(),
            value,
            fmt: fmt.into(),
        });
        p
    }

    #[test]
    fn nested_inline_tags() {
        let mut p = body("a bold both bold <x> & link");
        p.marks = vec![
            Mark {
                start: 2,
                end: 7,
                flags: BOLD,
            },
            Mark {
                start: 7,
                end: 11,
                flags: BOLD | ITALIC,
            },
            Mark {
                start: 11,
                end: 16,
                flags: BOLD,
            },
        ];
        p.links = vec![Link {
            start: 23,
            end: 27,
            href: "https://x.y/?a=1&b=\"2\"".into(),
        }];
        assert_eq!(
            inline(&p, 0, p.text.len()),
            "a <strong>bold <em>both</em> bold</strong> &lt;x&gt; &amp; <a href=\"https://x.y/?a=1&amp;b=&quot;2&quot;\">link</a>"
        );
        // Partial ranges close what they opened.
        assert_eq!(inline(&p, 4, 9), "<strong>ld <em>bo</em></strong>");
    }

    #[test]
    fn blocks_lists_and_tables() {
        let mut h = Paragraph::new("Steps", Kind::Heading);
        h.hlevel = 2;
        let mut th = Paragraph::new("Key\tAction", Kind::TableRow);
        th.header = true;
        th.group = 1;
        let mut td = Paragraph::new("y\tYank", Kind::TableRow);
        td.group = 1;
        let ps = vec![
            h,
            item("One", 0, "decimal", 3, 1),
            item("Two", 0, "decimal", 4, 1),
            item("Sub", 1, "lowerLetter", 1, 1),
            item("Three", 0, "decimal", 5, 1),
            item("Dot", 0, "bullet", 1, 2),
            body("After & done"),
            th,
            td,
        ];
        let last = ps.len() - 1;
        let html = render(
            &ps,
            Pos { para: 0, byte: 0 },
            Pos {
                para: last,
                byte: ps[last].text.len(),
            },
        );
        assert_eq!(
            html,
            r#"<h2>Steps</h2>
<ol start="3">
  <li>One</li>
  <li>Two
    <ol type="a">
      <li>Sub</li>
    </ol>
  </li>
  <li>Three</li>
</ol>
<ul>
  <li>Dot</li>
</ul>
<p>After &amp; done</p>
<table>
  <tr><th>Key</th><th>Action</th></tr>
  <tr><td>y</td><td>Yank</td></tr>
</table>"#
        );
    }

    #[test]
    fn fragment_inside_one_paragraph() {
        let ps = vec![body("hello world")];
        assert_eq!(
            render(&ps, Pos { para: 0, byte: 6 }, Pos { para: 0, byte: 11 }),
            "world"
        );
        assert_eq!(
            render(&ps, Pos { para: 0, byte: 0 }, Pos { para: 0, byte: 11 }),
            "<p>hello world</p>"
        );
    }
}
