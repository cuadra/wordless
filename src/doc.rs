//! Legacy Word 97–2003 (`.doc`) → plain paragraphs.
//!
//! A `.doc` is an OLE2 "compound file" (a tiny FAT filesystem). The text
//! lives in the `WordDocument` stream, scattered across *pieces*; the piece
//! table (the `Clx`) sits in the `0Table` or `1Table` stream, and the FIB
//! header says which. We read exactly those two streams, walk the pieces for
//! the main body text, and turn Word's control characters into paragraphs.
//!
//! Formatting, styles and list structure live in separate binary property
//! tables and are not read, so `.doc` files render as plain paragraphs and
//! the HTML yank produces `<p>` blocks only.

use std::{io::Read, path::Path};

use crate::docx::{Error, Kind, Paragraph};

/// First 8 bytes of every OLE2 compound file.
pub const OLE_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

pub fn open(path: &Path) -> Result<Vec<Paragraph>, Error> {
    let mut cf = cfb::open(path)?;
    if !cf.is_stream("/WordDocument") {
        return Err("not a Word document (an Excel/PowerPoint file?)".into());
    }
    let mut word = Vec::new();
    cf.open_stream("/WordDocument")?.read_to_end(&mut word)?;
    let fib = Fib::parse(&word)?;
    let mut table = Vec::new();
    let name = if fib.table1 { "/1Table" } else { "/0Table" };
    cf.open_stream(name)
        .map_err(|_| "damaged .doc: table stream missing")?
        .read_to_end(&mut table)?;
    let text = body_text(&word, &table, &fib)?;
    Ok(paragraphs(&text))
}

fn bytes(buf: &[u8], off: usize, n: usize) -> Result<&[u8], Error> {
    buf.get(off..off.checked_add(n).ok_or("damaged .doc")?)
        .ok_or_else(|| "damaged .doc: offset out of range".into())
}

fn u16_at(buf: &[u8], off: usize) -> Result<u16, Error> {
    let b = bytes(buf, off, 2)?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(buf: &[u8], off: usize) -> Result<u32, Error> {
    let b = bytes(buf, off, 4)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// The few fields of the File Information Block we need.
struct Fib {
    table1: bool,
    ccp_text: u32,
    fc_clx: u32,
    lcb_clx: u32,
}

impl Fib {
    fn parse(w: &[u8]) -> Result<Fib, Error> {
        if u16_at(w, 0)? != 0xA5EC {
            return Err("not a Word 97–2003 document".into());
        }
        if u16_at(w, 2)? < 0xC1 {
            return Err("Word 6/95 .doc files aren't supported".into());
        }
        let flags = u16_at(w, 0x0A)?;
        if flags & 0x0100 != 0 {
            return Err("this .doc is password-protected".into());
        }
        // FibBase (32 bytes), then three variable-length arrays.
        let mut o = 32;
        o += 2 + 2 * u16_at(w, o)? as usize; // fibRgW
        let lw = o + 2; // fibRgLw
        let ccp_text = u32_at(w, lw + 3 * 4)?;
        o = lw + 4 * u16_at(w, o)? as usize;
        let cb = u16_at(w, o)? as usize; // fibRgFcLcb pairs
        if cb <= 33 {
            return Err("damaged .doc: FIB too short".into());
        }
        let clx = o + 2 + 33 * 8; // fcClx / lcbClx
        Ok(Fib {
            table1: flags & 0x0200 != 0,
            ccp_text,
            fc_clx: u32_at(w, clx)?,
            lcb_clx: u32_at(w, clx + 4)?,
        })
    }
}

/// Reassemble the main-document text (CPs `0..ccpText`) from the piece table.
fn body_text(word: &[u8], table: &[u8], fib: &Fib) -> Result<String, Error> {
    let clx = bytes(table, fib.fc_clx as usize, fib.lcb_clx as usize)?;
    // Skip any Prc (property-modifier) entries; the Pcdt follows.
    let mut i = 0;
    while clx.get(i) == Some(&0x01) {
        i += 3 + i16::from_le_bytes([clx[i + 1], *clx.get(i + 2).ok_or("damaged .doc")?]).max(0)
            as usize;
    }
    if clx.get(i) != Some(&0x02) {
        return Err("damaged .doc: piece table missing".into());
    }
    let lcb = u32_at(clx, i + 1)? as usize;
    let plc = bytes(clx, i + 5, lcb)?;
    let n = lcb.saturating_sub(4) / 12;
    let cp = |k: usize| u32_at(plc, k * 4);

    let mut text = String::with_capacity(fib.ccp_text as usize);
    for k in 0..n {
        let (cp0, cp1) = (cp(k)?, cp(k + 1)?.min(fib.ccp_text));
        if cp0 >= fib.ccp_text {
            break;
        }
        let count = cp1.saturating_sub(cp0) as usize;
        let fc_raw = u32_at(plc, 4 * (n + 1) + 8 * k + 2)?;
        let fc = (fc_raw & 0x3FFF_FFFF) as usize;
        if fc_raw & 0x4000_0000 != 0 {
            // "Compressed": one byte per character, Windows-1252.
            text.extend(bytes(word, fc / 2, count)?.iter().map(|&b| cp1252(b)));
        } else {
            let raw = bytes(word, fc, count * 2)?;
            let units = raw
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]));
            text.extend(char::decode_utf16(units).map(|r| r.unwrap_or('\u{FFFD}')));
        }
    }
    Ok(text)
}

fn cp1252(b: u8) -> char {
    const HI: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž',
        '\u{8F}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}',
        'ž', 'Ÿ',
    ];
    match b {
        0x80..=0x9F => HI[(b - 0x80) as usize],
        b => b as char,
    }
}

/// Word's in-band control characters → paragraphs and table rows.
fn paragraphs(text: &str) -> Vec<Paragraph> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut fields: Vec<bool> = Vec::new(); // true while in a field's instruction part
    let mut cells: Vec<String> = Vec::new();
    let mut after_cell = false;
    let mut table = 1u32;

    let end_para = |cur: &mut String, out: &mut Vec<Paragraph>| {
        let t = cur.trim_end();
        if !t.trim().is_empty() {
            out.push(Paragraph::new(t, Kind::Body));
        }
        cur.clear();
    };

    for c in text.chars() {
        match c {
            '\u{13}' => fields.push(true), // field begin: instruction follows
            '\u{14}' => {
                if let Some(f) = fields.last_mut() {
                    *f = false; // separator: visible result follows
                }
            }
            '\u{15}' => {
                fields.pop();
            }
            _ if fields.last() == Some(&true) => {} // e.g. HYPERLINK "…", PAGE
            '\u{07}' => {
                // Cell mark. Two in a row (with nothing between) is the
                // row-end mark that follows the last cell.
                if after_cell && cur.trim().is_empty() {
                    if cells.iter().any(|c| !c.is_empty()) {
                        let mut p = Paragraph::new(cells.join("\t"), Kind::TableRow);
                        p.group = table;
                        out.push(p);
                    }
                    cells.clear();
                    after_cell = false;
                } else {
                    cells.push(cur.replace(['\t', '\n'], " ").trim().to_string());
                    after_cell = true;
                }
                cur.clear();
                continue;
            }
            '\r' | '\u{0C}' if !cells.is_empty() => cur.push(' '), // paragraph inside a cell
            '\r' | '\u{0C}' => {
                end_para(&mut cur, &mut out);
                table += 1;
            }
            '\u{0B}' => cur.push('\n'), // manual line break
            '\u{1E}' => cur.push('-'),  // non-breaking hyphen
            '\u{A0}' => cur.push(' '),  // non-breaking space
            '\u{1F}' | '\u{AD}' => {}   // optional hyphen
            '\t' => cur.push('\t'),
            c if c.is_control() => {} // pictures, footnote marks, objects
            c => cur.push(c),
        }
        after_cell = false;
    }
    end_para(&mut cur, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_chars_become_structure() {
        let t = "Title\rSee \u{13} HYPERLINK \"http://x\" \u{14}the site\u{15}.\u{0B}Next line\r\
                 a\u{07}b\u{07}\u{07}c\u{07}d\u{07}\u{07}After\u{1E}table\u{1F}\r";
        let p = paragraphs(t);
        let texts: Vec<_> = p.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Title",
                "See the site.\nNext line",
                "a\tb",
                "c\td",
                "After-table"
            ]
        );
        assert_eq!(p[2].kind, Kind::TableRow);
        assert_eq!(p[2].group, p[3].group);
    }

    #[test]
    fn cp1252_high_range() {
        assert_eq!(cp1252(0x93), '“');
        assert_eq!(cp1252(0x96), '–');
        assert_eq!(cp1252(0xE9), 'é');
    }
}
