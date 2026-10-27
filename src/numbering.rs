//! Paragraph/character styles and list numbering.
//!
//! Used to derive structure (heading level, list marker and type) and the
//! handful of inline semantics the HTML yank emits (bold, italic, …). None of
//! it is ever drawn as styling or mixed into the plain text.

use std::collections::{HashMap, HashSet};

use quick_xml::{
    Reader,
    events::{BytesStart, Event},
};

pub type Error = Box<dyn std::error::Error>;

/// Unescaped attribute value.
pub fn attr(e: &BytesStart, name: &str) -> Option<String> {
    let a = e.try_get_attribute(name).ok().flatten()?;
    a.normalized_value(quick_xml::XmlVersion::default())
        .ok()
        .map(|v| v.into_owned())
}

fn val(e: &BytesStart) -> Option<String> {
    attr(e, "w:val")
}

// ---- inline formatting flags ----------------------------------------------------

pub const BOLD: u8 = 1;
pub const ITALIC: u8 = 2;
pub const UNDERLINE: u8 = 4;
pub const STRIKE: u8 = 8;
pub const SUP: u8 = 16;
pub const SUB: u8 = 32;

/// Interpret a run-property element: `Some((flag, on))` if it is one we keep.
/// `<w:b/>` is on; `<w:b w:val="0"/>` explicitly turns it off.
pub fn run_prop(e: &BytesStart) -> Option<(u8, bool)> {
    let v = val(e);
    let on = || {
        v.as_deref()
            .is_none_or(|v| !matches!(v, "0" | "false" | "off" | "none"))
    };
    match e.name().as_ref() {
        "w:b" => Some((BOLD, on())),
        "w:i" => Some((ITALIC, on())),
        "w:u" => Some((UNDERLINE, on())),
        "w:strike" | "w:dstrike" => Some((STRIKE, on())),
        "w:vertAlign" => match v.as_deref() {
            Some("superscript") => Some((SUP, true)),
            Some("subscript") => Some((SUB, true)),
            _ => Some((SUP | SUB, false)),
        },
        _ => None,
    }
}

// ---- styles.xml ---------------------------------------------------------------

#[derive(Default, Debug)]
struct StyleDef {
    based_on: Option<String>,
    heading: Option<u8>,
    num_id: Option<String>,
    ilvl: Option<u8>,
    /// Run formatting in the style's rPr (used for character styles only).
    flags: u8,
}

#[derive(Default, Debug)]
pub struct Styles {
    map: HashMap<String, StyleDef>,
}

pub struct Resolved {
    /// Heading level 1–9, if this is a heading style.
    pub heading: Option<u8>,
    pub num: Option<(String, u8)>,
}

impl Styles {
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn parse(xml: &str) -> Result<Self, Error> {
        let mut r = Reader::from_str(xml);
        let mut map = HashMap::new();
        let mut cur: Option<(String, StyleDef)> = None;
        let mut in_rpr = false;
        loop {
            match r.read_event()? {
                Event::Start(e) if e.name().as_ref() == "w:style" => {
                    let ok =
                        attr(&e, "w:type").is_none_or(|t| t == "paragraph" || t == "character");
                    cur = attr(&e, "w:styleId")
                        .filter(|_| ok)
                        .map(|id| (id, StyleDef::default()));
                }
                Event::Start(e) if e.name().as_ref() == "w:rPr" => in_rpr = true,
                Event::End(e) if e.name().as_ref() == "w:rPr" => in_rpr = false,
                Event::Empty(e) | Event::Start(e) => {
                    let Some((_, d)) = cur.as_mut() else { continue };
                    if in_rpr {
                        if let Some((f, on)) = run_prop(&e) {
                            if on { d.flags |= f } else { d.flags &= !f }
                        }
                        continue;
                    }
                    match e.name().as_ref() {
                        // Built-in style *names* are English in the XML even in
                        // localized Word, unlike style IDs ("berschrift1").
                        "w:name" => {
                            let n = val(&e).unwrap_or_default().to_ascii_lowercase();
                            if n == "title" {
                                d.heading = Some(1);
                            } else if let Some(l) =
                                n.strip_prefix("heading ").and_then(|l| l.parse().ok())
                            {
                                d.heading = Some(l);
                            }
                        }
                        "w:outlineLvl" => {
                            if let Some(l) = val(&e)
                                .and_then(|v| v.parse::<u8>().ok())
                                .filter(|&l| l < 9)
                            {
                                d.heading = Some(l + 1);
                            }
                        }
                        "w:basedOn" => d.based_on = val(&e),
                        "w:numId" => d.num_id = val(&e),
                        "w:ilvl" => d.ilvl = val(&e).and_then(|v| v.parse().ok()),
                        _ => {}
                    }
                }
                Event::End(e) if e.name().as_ref() == "w:style" => {
                    if let Some((id, d)) = cur.take() {
                        map.insert(id, d);
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(Styles { map })
    }

    /// Walk the `basedOn` chain (bounded, in case of cycles).
    pub fn resolve(&self, id: &str) -> Resolved {
        let mut out = Resolved {
            heading: None,
            num: None,
        };
        let mut cur = self.map.get(id);
        for _ in 0..16 {
            let Some(d) = cur else { break };
            out.heading = out.heading.or(d.heading);
            if out.num.is_none()
                && let Some(n) = &d.num_id
            {
                out.num = Some((n.clone(), d.ilvl.unwrap_or(0)));
            }
            cur = d.based_on.as_deref().and_then(|b| self.map.get(b));
        }
        out
    }

    /// Run formatting contributed by a character style (`w:rStyle`), e.g.
    /// Word's built-in "Strong" and "Emphasis".
    pub fn char_flags(&self, id: &str) -> u8 {
        let mut flags = 0;
        let mut cur = self.map.get(id);
        for _ in 0..16 {
            let Some(d) = cur else { break };
            flags |= d.flags;
            cur = d.based_on.as_deref().and_then(|b| self.map.get(b));
        }
        flags
    }
}

// ---- numbering.xml ------------------------------------------------------------

#[derive(Clone, Debug)]
struct Level {
    start: u32,
    fmt: String,
    text: String,
}

impl Default for Level {
    fn default() -> Self {
        Level {
            start: 1,
            fmt: "decimal".into(),
            text: String::new(),
        }
    }
}

#[derive(Default, Debug)]
struct Num {
    abs: String,
    overrides: Vec<(u8, u32)>,
}

#[derive(Default, Debug)]
pub struct Numbering {
    abstracts: HashMap<String, [Level; 9]>,
    nums: HashMap<String, Num>,
    /// Running counters per abstract list (instances sharing one continue it).
    counters: HashMap<String, [Option<u32>; 9]>,
    seen: HashSet<String>,
}

impl Numbering {
    pub fn parse(xml: &str) -> Result<Self, Error> {
        let mut r = Reader::from_str(xml);
        let mut n = Numbering::default();
        let mut abs: Option<(String, [Level; 9])> = None;
        let mut lvl: Option<usize> = None;
        let mut num: Option<(String, Num)> = None;
        let mut ovr: Option<u8> = None;
        loop {
            let (e, start) = match r.read_event()? {
                Event::Start(e) => (e, true),
                Event::Empty(e) => (e, false),
                Event::End(e) => {
                    match e.name().as_ref() {
                        "w:abstractNum" => {
                            if let Some((id, l)) = abs.take() {
                                n.abstracts.insert(id, l);
                            }
                        }
                        "w:lvl" => lvl = None,
                        "w:num" => {
                            if let Some((id, x)) = num.take() {
                                n.nums.insert(id, x);
                            }
                        }
                        "w:lvlOverride" => ovr = None,
                        _ => {}
                    }
                    continue;
                }
                Event::Eof => break,
                _ => continue,
            };
            match e.name().as_ref() {
                "w:abstractNum" if start => {
                    abs = attr(&e, "w:abstractNumId").map(|id| (id, Default::default()))
                }
                "w:lvl" if start && abs.is_some() => {
                    lvl = attr(&e, "w:ilvl")
                        .and_then(|v| v.parse().ok())
                        .filter(|&l: &usize| l < 9)
                }
                "w:num" if start => num = attr(&e, "w:numId").map(|id| (id, Num::default())),
                "w:lvlOverride" if start => ovr = attr(&e, "w:ilvl").and_then(|v| v.parse().ok()),
                "w:abstractNumId" => {
                    if let (Some((_, x)), Some(v)) = (num.as_mut(), val(&e)) {
                        x.abs = v;
                    }
                }
                "w:startOverride" => {
                    if let (Some((_, x)), Some(l), Some(v)) =
                        (num.as_mut(), ovr, val(&e).and_then(|v| v.parse().ok()))
                    {
                        x.overrides.push((l, v));
                    }
                }
                tag @ ("w:start" | "w:numFmt" | "w:lvlText") => {
                    if let (Some((_, levels)), Some(l), Some(v)) = (abs.as_mut(), lvl, val(&e)) {
                        let level = &mut levels[l];
                        match tag {
                            "w:start" => level.start = v.parse().unwrap_or(1),
                            "w:numFmt" => level.fmt = v,
                            _ => level.text = v,
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(n)
    }

    /// Advance the counter for (numId, ilvl) and return its marker.
    /// `None` means "not a list" (numId 0 or unknown); an empty marker text
    /// means a list level with no visible marker.
    pub fn next(&mut self, num_id: &str, ilvl: u8) -> Option<ListMark> {
        let ilvl = (ilvl as usize).min(8);
        if num_id == "0" {
            return None;
        }
        let Some(num) = self.nums.get(num_id) else {
            // numbering.xml missing/broken: still a list
            return Some(ListMark {
                text: "•".into(),
                value: 1,
                fmt: "bullet".into(),
            });
        };
        let levels = self.abstracts.get(&num.abs)?;
        let counters = self.counters.entry(num.abs.clone()).or_insert([None; 9]);
        if self.seen.insert(num_id.to_string()) {
            for &(l, v) in &num.overrides {
                if let Some(c) = counters.get_mut(l as usize) {
                    *c = Some(v.saturating_sub(1));
                }
            }
        }
        counters[ilvl] = Some(counters[ilvl].map_or(levels[ilvl].start, |c| c + 1));
        for c in counters.iter_mut().skip(ilvl + 1) {
            *c = None;
        }

        let lvl = &levels[ilvl];
        let value = counters[ilvl].unwrap_or(1);
        let mark = |text| {
            Some(ListMark {
                text,
                value,
                fmt: lvl.fmt.clone(),
            })
        };
        match lvl.fmt.as_str() {
            "bullet" => return mark(bullet(&lvl.text)),
            "none" => return mark(String::new()),
            _ => {}
        }
        // Substitute %1..%9 with each level's counter in that level's format.
        let mut out = String::new();
        let mut chars = lvl.text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '%'
                && let Some(d) = chars
                    .peek()
                    .and_then(|d| d.to_digit(10))
                    .filter(|d| (1..=9).contains(d))
            {
                chars.next();
                let k = d as usize - 1;
                let v = counters[k].unwrap_or(levels[k].start);
                out.push_str(&format_num(v, &levels[k].fmt));
            } else {
                out.push(c);
            }
        }
        mark(out)
    }
}

/// A list item's marker plus what the HTML yank needs (`<ol start type>`).
#[derive(Debug, Clone, PartialEq)]
pub struct ListMark {
    pub text: String,
    pub value: u32,
    pub fmt: String,
}

/// Word bullets are often Symbol/Wingdings code points in the Private Use
/// Area, which render as tofu. Map them to plain Unicode.
fn bullet(text: &str) -> String {
    match text.chars().next() {
        None => "•",
        Some('o') => "◦",
        Some('§' | '\u{F0A7}' | '\u{F06E}') => "▪",
        Some('\u{F0D8}' | '\u{F0E8}') => "›",
        Some('\u{F0FC}') => "✓",
        Some(c) if ('\u{F000}'..='\u{F0FF}').contains(&c) => "•",
        Some('-' | '–') => "–",
        Some(_) => return text.to_string(),
    }
    .into()
}

pub fn format_num(n: u32, fmt: &str) -> String {
    match fmt {
        "lowerLetter" => letters(n, b'a'),
        "upperLetter" => letters(n, b'A'),
        "lowerRoman" => roman(n).to_lowercase(),
        "upperRoman" => roman(n),
        "decimalZero" => format!("{n:02}"),
        "ordinal" => {
            let suf = match (n % 10, n % 100) {
                (_, 11..=13) => "th",
                (1, _) => "st",
                (2, _) => "nd",
                (3, _) => "rd",
                _ => "th",
            };
            format!("{n}{suf}")
        }
        _ => n.to_string(),
    }
}

/// Word's scheme: a..z, then aa..zz, aaa..
fn letters(n: u32, base: u8) -> String {
    if n == 0 {
        return "0".into();
    }
    let c = (base + ((n - 1) % 26) as u8) as char;
    c.to_string().repeat(((n - 1) / 26 + 1) as usize)
}

fn roman(mut n: u32) -> String {
    const T: [(u32, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut s = String::new();
    for &(v, r) in &T {
        while n >= v {
            s.push_str(r);
            n -= v;
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const NUMBERING: &str = r#"<w:numbering xmlns:w="w">
      <w:abstractNum w:abstractNumId="0">
        <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl>
        <w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%1.%2)"/></w:lvl>
      </w:abstractNum>
      <w:abstractNum w:abstractNumId="1">
        <w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/><w:lvlText w:val="&#61623;"/></w:lvl>
      </w:abstractNum>
      <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
      <w:num w:numId="2"><w:abstractNumId w:val="1"/></w:num>
      <w:num w:numId="3"><w:abstractNumId w:val="0"/>
        <w:lvlOverride w:ilvl="0"><w:startOverride w:val="7"/></w:lvlOverride></w:num>
    </w:numbering>"#;

    #[test]
    fn counters_nest_and_restart() {
        let mut n = Numbering::parse(NUMBERING).unwrap();
        let seq: Vec<_> = [
            ("1", 0),
            ("1", 1),
            ("1", 1),
            ("1", 0),
            ("1", 1),
            ("2", 0),
            ("3", 0),
            ("3", 0),
            ("0", 0),
        ]
        .iter()
        .map(|(id, l)| n.next(id, *l).map(|m| m.text))
        .collect();
        let want =
            ["1.", "1.a)", "1.b)", "2.", "2.a)", "•", "7.", "8."].map(|s| Some(s.to_string()));
        assert_eq!(seq[..8], want);
        assert_eq!(seq[8], None);
    }

    #[test]
    fn formats() {
        assert_eq!(format_num(28, "lowerLetter"), "bb");
        assert_eq!(format_num(14, "upperRoman"), "XIV");
        assert_eq!(format_num(12, "ordinal"), "12th");
        assert_eq!(format_num(22, "ordinal"), "22nd");
    }

    #[test]
    fn styles_resolve_chain() {
        let s = Styles::parse(r#"<w:styles xmlns:w="w">
          <w:style w:type="paragraph" w:styleId="berschrift1"><w:name w:val="heading 1"/></w:style>
          <w:style w:type="paragraph" w:styleId="Mine"><w:basedOn w:val="berschrift1"/></w:style>
          <w:style w:type="paragraph" w:styleId="ListNumber"><w:pPr><w:numPr><w:numId w:val="4"/></w:numPr></w:pPr></w:style>
          <w:style w:type="character" w:styleId="Heading1Char"><w:name w:val="heading 1 char"/></w:style>
        </w:styles>"#).unwrap();
        assert_eq!(s.resolve("Mine").heading, Some(1));
        assert_eq!(s.resolve("ListNumber").num, Some(("4".into(), 0)));
        assert_eq!(s.resolve("Heading1Char").heading, None);
    }
}
