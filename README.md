# wordless

A hyper-minimal, keyboard-only `.docx` pager for Wayland. It opens instantly, shows only the words, and every yank lands on the clipboard as plain UTF-8 text.

```
wordless report.docx            # read
wordless -w 88 report.docx      # wider measure (default 72 columns)
wordless --dump report.docx     # plain text to stdout (pipe into rg, wc, etc.)
wordless --html report.docx     # semantic HTML to stdout
```

Build: `cargo build --release` → `target/release/wordless` (~0.9 MB stripped).
Runtime dependency: `wl-clipboard` (`pacman -S wl-clipboard`). Without it, wordless falls back to OSC 52.

---

## File formats

| Format | Support |
|---|---|
| `.docx` (Word 2007+) | Full: text, headings, numbered lists, tables, footnotes, and HTML yank with formatting and links |
| `.doc` (Word 97–2003) | Reading, search and plain yank. HTML yank gives `<p>` paragraphs only; formatting and list structure live in binary property tables that aren't read. Tables are detected from cell marks; a row with an *empty* cell may split early. |

The format is detected from the file's first bytes, not its extension, so a mislabeled file still opens. Password-protected `.doc` files and Word 6/95 files are rejected with a clear message.

## Design decision: TUI (Ratatui), not a GUI

| | **Ratatui (TUI)** | Iced / Slint (GUI) |
|---|---|---|
| Cold start | No GPU context or font atlas. Time to first frame is in milliseconds | wgpu/GL init plus font loading, usually 100–300 ms |
| Typography | Uses your terminal's font, size, ligatures and theme, so nothing to configure | Must ship or locate fonts and reimplement theming |
| Yazi / file managers | Runs in the terminal you're already in (`block = true`) | Spawns a new Hyprland window and steals focus |
| Text selection | Vim visual mode, which keyboard-only use needs anyway | Mouse-drag selection (not needed here) |
| Scrolling | Moves by whole rows | Pixel-smooth |
| Footprint | ~0.9 MB, 4 direct crates | 10–30 MB, hundreds of crates |

The one thing a GUI does better is pixel-smooth scrolling. For a pager driven by `j/k/Ctrl-d`, scrolling by whole rows is the expected behavior. Smoothness here comes from never dropping or lagging behind input: the event loop drains every queued key before drawing, so holding `j` costs one frame per batch, not one per keypress.

## Crate architecture

| Concern | Crate | Why |
|---|---|---|
| Zip container | `zip` (`default-features = false`, `deflate`) | Word only uses DEFLATE; this skips the bzip2/zstd/aes/time code |
| XML | `quick-xml` 0.42 | Zero-copy pull parser that streams `word/document.xml` without building a DOM |
| TUI | `ratatui` 0.30 (`crossterm` feature only) | Crossterm is re-exported, so there's no separate dependency |
| Width | `unicode-width` | Correct wrapping for CJK and emoji |
| Clipboard | **`wl-copy` subprocess** with OSC 52 fallback | See below |

Crates I chose against:
- **`docx-rs`** is mainly a writer and builds a full styled model, which is exactly the overhead this tool avoids.
- **`arboard` / `wl-clipboard-rs`** serve the clipboard from inside your own process. On Wayland the clipboard belongs to a live process, so the copied text would disappear when you quit the pager unless you fork a server yourself. `wl-copy` already does that fork, and using it adds no crates.

### What the parser reads (and ignores)

The parser reads up to five zip entries; only the first is required:

| Entry | Used for |
|---|---|
| `word/document.xml` | the text |
| `word/styles.xml` | heading detection and lists defined by a style. Built-in style **names** are English in the XML even in localized Word, so headings are detected in any language |
| `word/numbering.xml` | real list markers: `1.`, `1.a)`, `iv.`, `07`, `2nd`, bullets, restarts and nested counters |
| `word/footnotes.xml`, `word/endnotes.xml` | only opened if the body references them |

Fonts, themes, media and relationships are never opened.

- Kept: `w:t` text, `w:tab`, line breaks, `&amp;`-style entities, text-box content, table rows (cells are joined with `\t`), footnote/endnote references as `[1]` / `[i]`.
- Dropped: `mc:Fallback` (a duplicate VML copy of text boxes), `w:del` (tracked deletions), `w:instrText` (field codes), page breaks, tab-stop definitions, soft hyphens, zero-width characters and empty spacer paragraphs.
- The only structure kept is a paragraph's kind (`Body | Heading | ListItem | TableRow | Note`), its list marker and nesting level, and table column widths. These control layout and **never enter the text**, so they can't leak into a yank. Yanking a numbered item gives you the item's text without the `3.`, just as Vim never yanks line numbers.
- **Footnotes are the one exception.** `[1]` is part of the text, and the notes section at the end starts with the same `[1]`. Search is literal, so `/[1]` followed by `n` jumps between a reference and its note with no extra keybinding.

### Why yanks are always clean

Wrapping, list markers and table padding exist only on screen. Each display row is a byte range into its source paragraph, so a yank slices the original string. Soft-wrapped lines never add fake newlines, and the window size never changes what you copy. Paragraph boundaries become `\n` and table cells become `\t`. The text goes to `wl-copy --type text/plain;charset=utf-8`, so only a plain-text MIME type is offered.

### HTML yank (`Y` / `gy`)

Lowercase `y` yanks plain text; Shift+`Y` yanks **formatted**. It puts **semantic HTML source** on the clipboard as text, ready to paste into a CMS, an editor or a template:

| Word | HTML |
|---|---|
| bold, italic, underline, strikethrough | `<strong>`, `<em>`, `<u>`, `<s>` |
| superscript, subscript, footnote refs | `<sup>`, `<sub>` |
| hyperlinks (both link elements and `HYPERLINK` fields), bookmarks | `<a href="…">`, `<a href="#bookmark">` |
| line break inside a paragraph | `<br>` |
| paragraphs, Heading 1–6 / Title | `<p>`, `<h1>`–`<h6>` |
| numbered / bulleted lists, nesting, restarts | `<ol start type>` / `<ul>` with nested `<li>` |
| tables, header rows | `<table><tr><th>/<td>` |

No classes, inline styles, fonts or colors are ever emitted. Formatting from Word's built-in character styles ("Strong", "Emphasis") counts, while a heading's own bold doesn't produce `<strong>` inside `<h2>`.

- `Y` / `{n}Y`: the current paragraph (or *n* paragraphs) as blocks.
- `V…Y`: whole paragraphs as blocks.
- `v…Y` inside one paragraph: an inline fragment without a `<p>` wrapper, the way copying a few words from a web page would.
- `v…Y` across paragraphs: blocks; the first and last are trimmed to the selection.
- `gy{motion}` is the HTML version of `y{motion}` (`gyj`, `gy}`, `gyy`), for when you'd rather not select first.

Like list markers, formatting is stored *beside* the text as byte ranges. The screen and the plain yank never see it.

## Code layout

```
src/
├── main.rs       CLI args, parse-before-tty, event loop (input coalescing)
├── docx.rs       format sniffing; zip → quick-xml pull parser → Vec<Paragraph>; notes
├── doc.rs        .doc: OLE2 container (cfb) → FIB → piece table → paragraphs
├── numbering.rs  styles.xml (headings, style lists) + numbering.xml (markers)
├── layout.rs     soft-wrap rows (byte ranges), table alignment, extract(), search
├── html.rs       gy: inline tag-stack renderer + block renderer (lists, tables)
├── app.rs        state + Vim grammar: [count] [operator] motion. No I/O; unit-tested
├── ui.rs         centered column, status row, native terminal cursor
└── clipboard.rs  wl-copy → OSC 52 fallback
```

## Keys

| | |
|---|---|
| `j k h l`, arrows | move (`Enter` = `j`) |
| `w b e`, `0 ^ $` | word / row motions |
| `{ }` | previous / next paragraph |
| `gg G`, `{n}G`, `{n}%` | top / bottom / row *n* / percent |
| `H M L` | screen top / middle / bottom |
| `Ctrl-d/u`, `Ctrl-f/b`, `Space` | half page / page |
| `Ctrl-e/y` | scroll view without moving the cursor |
| `zz zt zb` | center / top / bottom the cursor row |
| `/ ?`, `n N` | smartcase search, highlighted as you type |
| `v`, `V`, `o` | visual / visual-row, swap ends |
| `y` (visual), `y{motion}`, `yy`, `{n}yy` | yank as plain text (`yy` = whole paragraph) |
| `Y` (visual or normal), `{n}Y`, `gy{motion}` | yank as semantic HTML (see above) |
| `:{n}`, `:noh`, `:q`, `q` | go to paragraph *n*, clear highlights, quit |

Tables line up in columns (`Parser  │ Jose │ done`) when they fit the measure. Wider tables fall back to wrapped rows with ` │ ` separators. A table row yanks as tab-separated cells, so it pastes straight into a spreadsheet.

Counts work everywhere (`5j`, `3w`, `y2}`, `v3ey`). You can search inside visual mode: `v/end<CR>y`.

## Integration

**Yazi** (`~/.config/yazi/yazi.toml`):

```toml
[opener]
docx = [{ run = 'wordless "$@"', block = true, desc = "wordless", for = "unix" }]

[open]
prepend_rules = [{ url = "*.docx", use = "docx" }]   # older Yazi: name = "*.docx"
```

**Desktop / xdg-open:** copy `assets/wordless.desktop` to `~/.local/share/applications/`, then run
`xdg-mime default wordless.desktop application/vnd.openxmlformats-officedocument.wordprocessingml.document`.

## Performance

Measured on a 1.37M-word document (11 MB of XML, about 4,500 pages), release build:

| Stage | Time |
|---|---|
| Unzip + parse | ~100 ms |
| Layout (155k display rows; also rerun on resize) | ~55 ms |
| Incremental search, per keystroke | ~10 ms (ASCII fast path); non-ASCII patterns take the exact Unicode path |

Typical documents (under 50k words) open in under 15 ms. Memory-mapping wouldn't help, because `document.xml` is compressed inside the zip and must be decompressed anyway. Background parsing would only matter for files far beyond book length.

## Next steps

- Wrap table cells inside their columns when a table is wider than the measure (currently it falls back to unaligned rows).
- Add a toggle to include list markers in yanks (`1. Step one`), for pasting procedures.
- Optionally read headers and footers (`word/header*.xml`).
- Add a footnote jump key on top of the existing `/[1]` workflow.
