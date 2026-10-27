//! OOXML (.docx) → paragraphs of plain text plus side-band annotations.
//!
//! Reads at most seven zip entries, all optional except the first:
//! `document.xml` (text), `styles.xml` + `numbering.xml` (headings, list
//! markers, character styles), `footnotes.xml` / `endnotes.xml` (only if the
//! body references them), and the matching `_rels` files (hyperlink targets).
//! Fonts, themes and media are never opened.
//!
//! The text itself is always plain. Everything else (list markers, inline
//! bold/italic marks, hyperlinks, heading levels) lives *beside* it as byte
//! ranges. The screen and the plain yank never see the marks; the HTML yank
//! (`gy`) turns them into tags. The one exception is footnote markers like
//! `[1]`: they are real content, which keeps `/[1]` + `n` working as a way to
//! hop between a reference and its note.

use std::{
    collections::HashMap,
    fs::File,
    io::{BufReader, Read, Seek},
    path::Path,
};

use quick_xml::{
    Reader,
    events::{BytesStart, Event},
};

use crate::numbering::{ListMark, Numbering, SUP, Styles, attr, format_num, run_prop};

pub type Error = Box<dyn std::error::Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Body,
    Heading,
    ListItem,
    /// One table row; cells are joined by `\t` (pastes straight into a sheet).
    TableRow,
    /// A footnote/endnote in the notes section at the end.
    Note,
}

/// Inline formatting over `text[start..end]` (flags from `numbering::BOLD`…).
/// Marks within a paragraph never overlap.
#[derive(Debug, Clone, PartialEq)]
pub struct Mark {
    pub start: usize,
    pub end: usize,
    pub flags: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub start: usize,
    pub end: usize,
    pub href: String,
}

#[derive(Debug, Clone)]
pub struct Paragraph {
    pub text: String,
    pub kind: Kind,
    /// Display-only list marker ("1.", "a)", "•"); empty for none.
    pub marker: String,
    /// List nesting level (0-based).
    pub level: u8,
    /// Table id or list numId: neighbours with the same kind *and* group
    /// render tight; a new table/list gets its own block.
    pub group: u32,
    /// Display width of each `\t` in order (set by layout); empty → 4.
    pub tabs: Vec<u16>,
    /// Heading level 1–6 (HTML), 0 when not a heading.
    pub hlevel: u8,
    /// List details for `<ol start type>` (list items only).
    pub list: Option<ListMark>,
    /// Table header row (`<th>`).
    pub header: bool,
    pub marks: Vec<Mark>,
    pub links: Vec<Link>,
}

impl Paragraph {
    pub fn new(text: impl Into<String>, kind: Kind) -> Self {
        Paragraph {
            text: text.into(),
            kind,
            marker: String::new(),
            level: 0,
            group: 0,
            tabs: Vec::new(),
            hlevel: 0,
            list: None,
            header: false,
            marks: Vec::new(),
            links: Vec::new(),
        }
    }

    /// Append `other.text[from..to]` after `sep`, carrying its annotations.
    fn append(&mut self, sep: &str, text: &str, from: usize, marks: &[Mark], links: &[Link]) {
        self.text.push_str(sep);
        let base = self.text.len();
        self.text.push_str(text);
        let to = from + text.len();
        let shift = |s: usize, e: usize| {
            let (s, e) = (s.clamp(from, to), e.clamp(from, to));
            (s < e).then(|| (s - from + base, e - from + base))
        };
        for m in marks {
            if let Some((start, end)) = shift(m.start, m.end) {
                self.marks.push(Mark {
                    start,
                    end,
                    flags: m.flags,
                });
            }
        }
        for l in links {
            if let Some((start, end)) = shift(l.start, l.end) {
                self.links.push(Link {
                    start,
                    end,
                    href: l.href.clone(),
                });
            }
        }
    }
}

/// Open a Word file, choosing the parser by content, not extension: a
/// `.doc` that is really OOXML (or vice versa) still opens.
pub fn open(path: &Path) -> Result<Vec<Paragraph>, Error> {
    let mut magic = [0u8; 8];
    let n = File::open(path)?.read(&mut magic)?;
    match &magic[..n] {
        m if m == crate::doc::OLE_MAGIC => crate::doc::open(path),
        [b'P', b'K', ..] => open_docx(path),
        _ => Err("not a Word document (.docx or .doc)".into()),
    }
}

fn open_docx(path: &Path) -> Result<Vec<Paragraph>, Error> {
    let mut zip = zip::ZipArchive::new(BufReader::new(File::open(path)?))?;
    let Some(doc) = read_entry(&mut zip, "word/document.xml")? else {
        let odt = zip.by_name("content.xml").is_ok();
        return Err(if odt {
            "OpenDocument (.odt) files aren't supported".into()
        } else {
            "not a Word document: word/document.xml missing".into()
        });
    };
    let styles = match read_entry(&mut zip, "word/styles.xml")? {
        Some(x) => Styles::parse(&x)?,
        None => Styles::default(),
    };
    let mut numbering = match read_entry(&mut zip, "word/numbering.xml")? {
        Some(x) => Numbering::parse(&x)?,
        None => Numbering::default(),
    };
    let rels = read_rels(&mut zip, "word/_rels/document.xml.rels")?;

    let mut p = Parser::new(&styles, &mut numbering, &rels);
    p.run(&doc)?;
    let (foot, end) = (
        std::mem::take(&mut p.foot_refs),
        std::mem::take(&mut p.end_refs),
    );
    let mut out: Vec<Paragraph> = p.out.into_iter().map(|(_, p)| p).collect();

    for (refs, entry, rels_entry, title, fmt) in [
        (
            foot,
            "word/footnotes.xml",
            "word/_rels/footnotes.xml.rels",
            "Footnotes",
            "decimal",
        ),
        (
            end,
            "word/endnotes.xml",
            "word/_rels/endnotes.xml.rels",
            "Endnotes",
            "lowerRoman",
        ),
    ] {
        if refs.is_empty() {
            continue;
        }
        let Some(xml) = read_entry(&mut zip, entry)? else {
            continue;
        };
        let rels = read_rels(&mut zip, rels_entry)?;
        let mut np = Parser::new(&styles, &mut numbering, &rels);
        np.run(&xml)?;
        append_notes(&mut out, np.out, &refs, title, fmt);
    }
    Ok(out)
}

fn read_entry<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<Option<String>, Error> {
    match zip.by_name(name) {
        Ok(mut e) => {
            let mut s = String::with_capacity(e.size() as usize);
            e.read_to_string(&mut s)?;
            Ok(Some(s))
        }
        Err(zip::result::ZipError::FileNotFound) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Relationship id → target, for hyperlinks (`r:id="rId5"` → URL).
fn read_rels<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<HashMap<String, String>, Error> {
    let mut map = HashMap::new();
    let Some(xml) = read_entry(zip, name)? else {
        return Ok(map);
    };
    let mut r = Reader::from_str(&xml);
    loop {
        match r.read_event()? {
            Event::Empty(e) | Event::Start(e) if e.local_name().as_ref() == "Relationship" => {
                if let (Some(id), Some(t)) = (attr(&e, "Id"), attr(&e, "Target")) {
                    map.insert(id, t);
                }
            }
            Event::Eof => return Ok(map),
            _ => {}
        }
    }
}

/// Append "Footnotes" + one `[n] text` paragraph per note, in reference order.
fn append_notes(
    out: &mut Vec<Paragraph>,
    notes: Vec<(Option<String>, Paragraph)>,
    refs: &[String],
    title: &str,
    fmt: &str,
) {
    let mut by_id: HashMap<String, Vec<Paragraph>> = HashMap::new();
    for (id, p) in notes {
        if let Some(id) = id {
            by_id.entry(id).or_default().push(p);
        }
    }
    let mut section = Vec::new();
    for (i, id) in refs.iter().enumerate() {
        let Some(paras) = by_id.get(id) else { continue };
        let mut note = Paragraph::new(format!("[{}]", format_num(i as u32 + 1, fmt)), Kind::Note);
        for (k, p) in paras.iter().enumerate() {
            let t = if k == 0 {
                p.text.trim_start()
            } else {
                p.text.as_str()
            };
            let from = p.text.len() - t.len();
            note.append(if k == 0 { " " } else { "\n" }, t, from, &p.marks, &p.links);
        }
        section.push(note);
    }
    if !section.is_empty() {
        let mut h = Paragraph::new(title, Kind::Heading);
        h.hlevel = 2;
        out.push(h);
        out.extend(section);
    }
}

/// Paragraph under construction. Paragraphs nest (text boxes live inside a
/// run of their anchor paragraph), so these are kept on a stack.
#[derive(Default)]
struct Open {
    text: String,
    marks: Vec<Mark>,
    links: Vec<Link>,
    style: Option<String>,
    num_id: Option<String>,
    ilvl: Option<u8>,
    outline: Option<u8>,
}

#[derive(Default)]
struct Row {
    cells: Vec<Paragraph>,
    header: bool,
}

/// A complex field (`fldChar begin … separate … end`); only HYPERLINK is used.
struct Field {
    instr: String,
    /// Byte offset where the visible result starts, plus the paragraph depth
    /// it started in (results spanning paragraphs are not linked).
    result: Option<(usize, usize)>,
}

struct Parser<'a> {
    styles: &'a Styles,
    numbering: &'a mut Numbering,
    rels: &'a HashMap<String, String>,
    /// Emitted paragraphs, tagged with their enclosing note id (notes files).
    out: Vec<(Option<String>, Paragraph)>,
    stack: Vec<Open>,
    runs: Vec<u8>, // formatting flags per open <w:r>
    in_text: bool,
    in_instr: bool,
    in_ppr: bool,
    in_rpr: bool,
    in_numpr: bool,
    skip: u32,
    tables: Vec<u32>, // ids of open <w:tbl>, innermost last
    table_seq: u32,
    rows: Vec<Row>,
    links: Vec<(Option<String>, usize, usize)>, // open <w:hyperlink>: href, start, depth
    fields: Vec<Field>,
    note: Option<String>,
    foot_refs: Vec<String>,
    end_refs: Vec<String>,
}

impl<'a> Parser<'a> {
    fn new(
        styles: &'a Styles,
        numbering: &'a mut Numbering,
        rels: &'a HashMap<String, String>,
    ) -> Self {
        Parser {
            styles,
            numbering,
            rels,
            out: Vec::new(),
            stack: Vec::new(),
            runs: Vec::new(),
            in_text: false,
            in_instr: false,
            in_ppr: false,
            in_rpr: false,
            in_numpr: false,
            skip: 0,
            tables: Vec::new(),
            table_seq: 0,
            rows: Vec::new(),
            links: Vec::new(),
            fields: Vec::new(),
            note: None,
            foot_refs: Vec::new(),
            end_refs: Vec::new(),
        }
    }

    fn run(&mut self, xml: &str) -> Result<(), Error> {
        let mut r = Reader::from_str(xml);
        loop {
            match r.read_event()? {
                Event::Start(e) => {
                    if self.skip > 0 {
                        self.skip += 1;
                    } else {
                        self.start(&e);
                        self.leaf(&e);
                    }
                }
                Event::Empty(e) if self.skip == 0 => self.leaf(&e),
                Event::Text(t) if self.skip == 0 => self.text(t.as_ref()),
                // quick-xml surfaces `&amp;` / `&#8217;` as separate events.
                Event::GeneralRef(e) if self.skip == 0 && (self.in_text || self.in_instr) => {
                    let c = match e.resolve_char_ref()? {
                        Some(c) => Some(c),
                        None => match e.as_ref() {
                            "amp" => Some('&'),
                            "lt" => Some('<'),
                            "gt" => Some('>'),
                            "quot" => Some('"'),
                            "apos" => Some('\''),
                            _ => None,
                        },
                    };
                    if let Some(c) = c {
                        self.text(c.encode_utf8(&mut [0; 4]));
                    }
                }
                Event::End(e) => {
                    if self.skip > 0 {
                        self.skip -= 1;
                    } else {
                        self.end(e.name().as_ref());
                    }
                }
                Event::Eof => return Ok(()),
                _ => {}
            }
        }
    }

    fn text(&mut self, s: &str) {
        if self.in_instr {
            if let Some(f) = self.fields.last_mut() {
                f.instr.push_str(s);
            }
        } else if self.in_text {
            let flags = self.runs.last().copied().unwrap_or(0);
            self.push(s, flags);
        }
    }

    /// Append visible text with its formatting flags.
    fn push(&mut self, s: &str, flags: u8) {
        let Some(p) = self.stack.last_mut() else {
            return;
        };
        let start = p.text.len();
        push_clean(&mut p.text, s);
        let end = p.text.len();
        if flags == 0 || end == start {
            return;
        }
        match p.marks.last_mut() {
            Some(m) if m.end == start && m.flags == flags => m.end = end,
            _ => p.marks.push(Mark { start, end, flags }),
        }
    }

    fn cur_len(&self) -> usize {
        self.stack.last().map_or(0, |p| p.text.len())
    }

    /// Container elements (only ever arrive as Start).
    fn start(&mut self, e: &BytesStart) {
        match e.name().as_ref() {
            // Duplicate VML copy of text boxes and deleted tracked changes.
            "mc:Fallback" | "w:delText" | "w:del" => self.skip = 1,
            "w:instrText" => self.in_instr = true,
            "w:p" => self.stack.push(Open::default()),
            "w:pPr" => self.in_ppr = true,
            "w:rPr" if !self.runs.is_empty() && !self.in_ppr => self.in_rpr = true,
            "w:numPr" if self.in_ppr => self.in_numpr = true,
            "w:r" => self.runs.push(0),
            "w:t" => self.in_text = true,
            "w:tbl" => {
                self.table_seq += 1;
                self.tables.push(self.table_seq);
            }
            "w:tr" => self.rows.push(Row::default()),
            "w:tc" => {
                if let Some(row) = self.rows.last_mut() {
                    row.cells.push(Paragraph::new("", Kind::Body));
                }
            }
            "w:footnote" | "w:endnote" => self.note = attr(e, "w:id"),
            "w:hyperlink" => {
                let href = attr(e, "r:id")
                    .and_then(|id| self.rels.get(&id).cloned())
                    .or_else(|| attr(e, "w:anchor").map(|a| format!("#{a}")));
                self.links.push((href, self.cur_len(), self.stack.len()));
            }
            "w:fldSimple" => {
                let instr = attr(e, "w:instr").unwrap_or_default();
                let result = Some((self.cur_len(), self.stack.len()));
                self.fields.push(Field { instr, result });
            }
            _ => {}
        }
    }

    /// Leaf elements (usually Empty, occasionally Start with children).
    fn leaf(&mut self, e: &BytesStart) {
        let in_run = !self.runs.is_empty();
        let name = e.name();
        if self.in_rpr {
            if let Some((f, on)) = run_prop(e)
                && let Some(flags) = self.runs.last_mut()
            {
                if on { *flags |= f } else { *flags &= !f }
            } else if name.as_ref() == "w:rStyle"
                && let (Some(id), Some(flags)) = (attr(e, "w:val"), self.runs.last_mut())
            {
                *flags |= self.styles.char_flags(&id);
            }
            return;
        }
        match name.as_ref() {
            "w:footnoteReference" | "w:endnoteReference" if in_run => {
                let Some(id) = attr(e, "w:id") else { return };
                let foot = name.as_ref() == "w:footnoteReference";
                let refs = if foot {
                    &mut self.foot_refs
                } else {
                    &mut self.end_refs
                };
                let n = refs.iter().position(|r| *r == id).unwrap_or_else(|| {
                    refs.push(id);
                    refs.len() - 1
                });
                let label = format_num(n as u32 + 1, if foot { "decimal" } else { "lowerRoman" });
                self.push(&format!("[{label}]"), SUP);
                return;
            }
            "w:fldChar" if in_run => {
                match attr(e, "w:fldCharType").as_deref() {
                    Some("begin") => self.fields.push(Field {
                        instr: String::new(),
                        result: None,
                    }),
                    Some("separate") => {
                        let at = (self.cur_len(), self.stack.len());
                        if let Some(f) = self.fields.last_mut() {
                            f.result = Some(at);
                        }
                    }
                    Some("end") => self.end_field(),
                    _ => {}
                }
                return;
            }
            "w:tblHeader" => {
                if let Some(r) = self.rows.last_mut() {
                    r.header = attr(e, "w:val").is_none_or(|v| v != "0" && v != "false");
                }
                return;
            }
            _ => {}
        }
        let (in_ppr, in_numpr) = (self.in_ppr, self.in_numpr);
        let Some(p) = self.stack.last_mut() else {
            return;
        };
        match name.as_ref() {
            "w:tab" if in_run => p.text.push('\t'),
            "w:br" | "w:cr" if in_run => {
                // Page/column breaks are layout, not content.
                if attr(e, "w:type").is_none_or(|t| t == "textWrapping") {
                    p.text.push('\n');
                }
            }
            "w:noBreakHyphen" if in_run => p.text.push('-'),
            "w:pStyle" if in_ppr => p.style = attr(e, "w:val"),
            "w:outlineLvl" if in_ppr => {
                p.outline = attr(e, "w:val")
                    .and_then(|v| v.parse::<u8>().ok())
                    .filter(|&l| l < 9)
            }
            "w:numId" if in_numpr => p.num_id = attr(e, "w:val"),
            "w:ilvl" if in_numpr => p.ilvl = attr(e, "w:val").and_then(|v| v.parse().ok()),
            _ => {}
        }
    }

    /// Close a field; a HYPERLINK field links its visible result.
    fn end_field(&mut self) {
        let Some(f) = self.fields.pop() else { return };
        let Some((start, depth)) = f.result else {
            return;
        };
        self.add_link(hyperlink_target(&f.instr), start, depth);
    }

    /// Link `[start, now)` in the current paragraph, unless the link began in
    /// a different (e.g. enclosing) paragraph.
    fn add_link(&mut self, href: Option<String>, start: usize, depth: usize) {
        let (len, same) = (self.cur_len(), depth == self.stack.len());
        if let (Some(href), Some(p)) = (href, self.stack.last_mut())
            && same
            && len > start
        {
            p.links.push(Link {
                start,
                end: len,
                href,
            });
        }
    }

    fn end(&mut self, name: &str) {
        match name {
            "w:t" => self.in_text = false,
            "w:instrText" => self.in_instr = false,
            "w:r" => {
                self.runs.pop();
            }
            "w:rPr" => self.in_rpr = false,
            "w:pPr" => self.in_ppr = false,
            "w:numPr" => self.in_numpr = false,
            "w:tbl" => {
                self.tables.pop();
            }
            "w:footnote" | "w:endnote" => self.note = None,
            "w:fldSimple" => self.end_field(),
            "w:hyperlink" => {
                if let Some((href, start, depth)) = self.links.pop() {
                    self.add_link(href, start, depth);
                }
            }
            "w:tr" => {
                let Some(row) = self.rows.pop() else { return };
                if row.cells.iter().all(|c| c.text.is_empty()) {
                    return;
                }
                let mut p = Paragraph::new("", Kind::TableRow);
                for (k, c) in row.cells.iter().enumerate() {
                    p.append(
                        if k == 0 { "" } else { "\t" },
                        &c.text,
                        0,
                        &c.marks,
                        &c.links,
                    );
                }
                p.group = self.tables.last().copied().unwrap_or(0);
                p.header = row.header;
                self.out.push((self.note.clone(), p));
            }
            "w:p" => {
                let Some(open) = self.stack.pop() else { return };
                // Paragraphs inside a cell fold into that cell.
                if let Some(cell) = self.rows.last_mut().and_then(|r| r.cells.last_mut()) {
                    let flat = open.text.replace(['\t', '\n'], " "); // same byte length
                    let t = flat.trim();
                    if !t.is_empty() {
                        let from = flat.len() - flat.trim_start().len();
                        let sep = if cell.text.is_empty() { "" } else { " " };
                        cell.append(sep, t, from, &open.marks, &open.links);
                    }
                    return;
                }
                let p = self.finish(open);
                // Empty paragraphs are Word's spacing hack; the layout adds
                // its own rhythm.
                if !p.text.trim().is_empty() {
                    self.out.push((self.note.clone(), p));
                }
            }
            _ => {}
        }
    }

    /// Resolve heading/list status from direct props, then the style chain.
    fn finish(&mut self, o: Open) -> Paragraph {
        let style = o.style.as_deref().map(|s| self.styles.resolve(s));
        let hlevel = o
            .outline
            .map(|l| l + 1)
            .or(style.as_ref().and_then(|s| s.heading))
            // No styles.xml: fall back to the English style-ID convention.
            .or_else(|| {
                let s = o.style.as_deref()?.to_ascii_lowercase();
                if !self.styles.is_empty() {
                    None
                } else if s == "title" {
                    Some(1)
                } else {
                    s.strip_prefix("heading")?.parse().ok()
                }
            });
        let style_num = style.and_then(|s| s.num);
        let num = match (o.num_id, o.ilvl) {
            (Some(id), l) => Some((id, l.unwrap_or(0))),
            (None, Some(l)) => style_num.map(|(id, _)| (id, l)),
            (None, None) => style_num,
        };
        // Advance counters even for empty items — Word numbers them too.
        let mark = num.as_ref().and_then(|(id, l)| self.numbering.next(id, *l));

        let text = o.text.trim_end();
        let mut p = Paragraph::new("", Kind::Body);
        p.append("", text, 0, &o.marks, &o.links);
        if let Some(m) = mark {
            p.marker = m.text.clone();
            p.list = Some(m);
            if let Some((id, l)) = num {
                p.level = l;
                p.group = id.parse().unwrap_or(0); // separate lists get a gap
            }
            p.kind = Kind::ListItem;
        }
        if let Some(h) = hlevel {
            p.kind = Kind::Heading; // numbered headings keep their marker
            p.hlevel = h.clamp(1, 6);
        }
        p
    }
}

/// `HYPERLINK "https://x" \l "frag" \o "tip"` → `https://x#frag`.
fn hyperlink_target(instr: &str) -> Option<String> {
    let mut toks = Vec::new();
    let mut it = instr.trim().chars().peekable();
    while let Some(&c) = it.peek() {
        if c.is_whitespace() {
            it.next();
        } else if c == '"' {
            it.next();
            toks.push((
                true,
                it.by_ref().take_while(|&c| c != '"').collect::<String>(),
            ));
        } else {
            let mut t = String::new();
            while let Some(&c) = it.peek().filter(|c| !c.is_whitespace()) {
                t.push(c);
                it.next();
            }
            toks.push((false, t));
        }
    }
    let mut toks = toks.into_iter();
    if !toks
        .next()
        .is_some_and(|(_, t)| t.eq_ignore_ascii_case("HYPERLINK"))
    {
        return None;
    }
    let (mut url, mut frag) = (None, None);
    while let Some((quoted, t)) = toks.next() {
        match t.as_str() {
            "\\l" if !quoted => frag = toks.next().map(|t| t.1),
            "\\o" | "\\t" if !quoted => {
                toks.next();
            }
            s if s.starts_with('\\') && !quoted => {}
            _ if url.is_none() => url = Some(t),
            _ => {}
        }
    }
    match (url, frag) {
        (Some(u), Some(f)) => Some(format!("{u}#{f}")),
        (Some(u), None) => Some(u),
        (None, Some(f)) => Some(format!("#{f}")),
        (None, None) => None,
    }
}

/// Append text, dropping invisible characters that would corrupt a yank.
fn push_clean(buf: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '\u{00AD}' | '\u{200B}' | '\u{FEFF}' => {} // soft hyphen, ZWSP, BOM
            '\u{00A0}' => buf.push(' '),
            c if c.is_control() && c != '\t' && c != '\n' => {}
            c => buf.push(c),
        }
    }
}

#[cfg(test)]
pub fn parse_document_xml(xml: &str) -> Result<Vec<Paragraph>, Error> {
    let (styles, mut numbering, rels) = (Styles::default(), Numbering::default(), HashMap::new());
    let mut p = Parser::new(&styles, &mut numbering, &rels);
    p.run(xml)?;
    Ok(p.out.into_iter().map(|(_, p)| p).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::numbering::{BOLD, ITALIC};

    const XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="w" xmlns:mc="mc"><w:body>
  <w:p><w:pPr><w:pStyle w:val="Heading1"/><w:tabs><w:tab w:val="left" w:pos="720"/></w:tabs></w:pPr>
    <w:r><w:rPr><w:b/></w:rPr><w:t>Intro</w:t></w:r></w:p>
  <w:p/>
  <w:p><w:r><w:t xml:space="preserve">Fish &amp; chips</w:t></w:r><w:r><w:tab/><w:t>tab</w:t></w:r>
    <w:r><w:br/><w:t>line2</w:t><w:br w:type="page"/></w:r>
    <w:del><w:r><w:delText>gone</w:delText></w:r></w:del>
    <w:r><w:instrText> PAGE </w:instrText></w:r>
    <w:r><w:footnoteReference w:id="5"/></w:r></w:p>
  <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="9"/></w:numPr></w:pPr><w:r><w:t>item</w:t></w:r></w:p>
  <w:p><w:r><mc:AlternateContent><mc:Choice><w:txbxContent><w:p><w:r><w:t>box</w:t></w:r></w:p></w:txbxContent></mc:Choice>
    <mc:Fallback><w:p><w:r><w:t>box</w:t></w:r></w:p></mc:Fallback></mc:AlternateContent></w:r><w:r><w:t>anchor</w:t></w:r></w:p>
  <w:tbl><w:tr><w:tc><w:p><w:r><w:t>a</w:t></w:r></w:p><w:p><w:r><w:t>b</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>c</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
</w:body></w:document>"#;

    #[test]
    fn extracts_plain_text() {
        let p = parse_document_xml(XML).unwrap();
        let texts: Vec<_> = p.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Intro",
                "Fish & chips\ttab\nline2[1]",
                "item",
                "box",
                "anchor",
                "a b\tc"
            ]
        );
        assert_eq!(p[0].kind, Kind::Heading);
        assert_eq!(p[0].hlevel, 1);
        assert_eq!(p[1].kind, Kind::Body);
        assert_eq!(p[2].kind, Kind::ListItem);
        assert_eq!(p[2].marker, "•", "unknown numId still renders as a list");
        assert_eq!(p[5].kind, Kind::TableRow);
        assert_eq!(p[5].group, 1);
    }

    #[test]
    fn marks_and_links_are_side_band() {
        let xml = r#"<w:document xmlns:w="w" xmlns:r="r"><w:body>
          <w:p><w:pPr><w:rPr><w:b/></w:rPr></w:pPr>
            <w:r><w:t xml:space="preserve">a </w:t></w:r>
            <w:r><w:rPr><w:b/></w:rPr><w:t>bold</w:t></w:r>
            <w:r><w:rPr><w:b/><w:i/></w:rPr><w:t>both</w:t></w:r>
            <w:r><w:rPr><w:b w:val="0"/></w:rPr><w:t xml:space="preserve"> off </w:t></w:r>
            <w:hyperlink w:anchor="top"><w:r><w:t>up</w:t></w:r></w:hyperlink>
            <w:r><w:fldChar w:fldCharType="begin"/></w:r>
            <w:r><w:instrText xml:space="preserve"> HYPERLINK "https://ex.com/?a=1&amp;b=2" \o "tip" </w:instrText></w:r>
            <w:r><w:fldChar w:fldCharType="separate"/></w:r>
            <w:r><w:t>site</w:t></w:r>
            <w:r><w:fldChar w:fldCharType="end"/></w:r></w:p>
        </w:body></w:document>"#;
        let p = &parse_document_xml(xml).unwrap()[0];
        assert_eq!(
            p.text, "a boldboth off upsite",
            "field codes never leak into text"
        );
        assert_eq!(
            p.marks,
            [
                Mark {
                    start: 2,
                    end: 6,
                    flags: BOLD
                },
                Mark {
                    start: 6,
                    end: 10,
                    flags: BOLD | ITALIC
                }
            ]
        );
        assert_eq!(
            p.links[0],
            Link {
                start: 15,
                end: 17,
                href: "#top".into()
            }
        );
        assert_eq!(
            p.links[1],
            Link {
                start: 17,
                end: 21,
                href: "https://ex.com/?a=1&b=2".into()
            }
        );
    }

    #[test]
    fn table_cells_keep_marks() {
        let xml = r#"<w:document xmlns:w="w"><w:body><w:tbl>
          <w:tr><w:trPr><w:tblHeader/></w:trPr><w:tc><w:p><w:r><w:t>H</w:t></w:r></w:p></w:tc></w:tr>
          <w:tr><w:tc><w:p><w:r><w:t xml:space="preserve">  x</w:t></w:r></w:p></w:tc>
                <w:tc><w:p><w:r><w:rPr><w:i/></w:rPr><w:t>em</w:t></w:r></w:p></w:tc></w:tr>
        </w:tbl></w:body></w:document>"#;
        let p = parse_document_xml(xml).unwrap();
        assert!(p[0].header && !p[1].header);
        assert_eq!(p[1].text, "x\tem");
        assert_eq!(
            p[1].marks,
            [Mark {
                start: 2,
                end: 4,
                flags: ITALIC
            }]
        );
    }

    #[test]
    fn hyperlink_instr() {
        assert_eq!(
            hyperlink_target(r#"HYPERLINK "https://a.b" \l "sec""#).as_deref(),
            Some("https://a.b#sec")
        );
        assert_eq!(
            hyperlink_target(r#"HYPERLINK \l "_Toc1""#).as_deref(),
            Some("#_Toc1")
        );
        assert_eq!(hyperlink_target(" PAGE "), None);
    }

    #[test]
    fn notes_section_follows_reference_order() {
        let notes = r#"<w:footnotes xmlns:w="w">
          <w:footnote w:type="separator" w:id="-1"><w:p><w:r><w:separator/></w:r></w:p></w:footnote>
          <w:footnote w:id="7"><w:p><w:r><w:footnoteRef/></w:r><w:r><w:t xml:space="preserve"> Second.</w:t></w:r></w:p></w:footnote>
          <w:footnote w:id="5"><w:p><w:r><w:footnoteRef/></w:r><w:r><w:t>First.</w:t></w:r></w:p></w:footnote>
        </w:footnotes>"#;
        let (s, mut n, rels) = (Styles::default(), Numbering::default(), HashMap::new());
        let mut np = Parser::new(&s, &mut n, &rels);
        np.run(notes).unwrap();
        let mut out = Vec::new();
        append_notes(
            &mut out,
            np.out,
            &["5".into(), "7".into()],
            "Footnotes",
            "decimal",
        );
        let texts: Vec<_> = out.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(texts, ["Footnotes", "[1] First.", "[2] Second."]);
    }
}
