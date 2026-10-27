//! Rendering. No borders, no chrome: a centered text column at a readable
//! measure, and a single dim status row. Colors come from the terminal's own
//! ANSI palette, so it inherits the user's Hyprland/foot/kitty theme.

use ratatui::{
    Frame,
    layout::{Alignment, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::app::{App, Mode};
use crate::docx::Kind;
use crate::layout::{COL_SEP, Pos, indent, tab_width, width_between};

const SELECT: Style = Style::new().add_modifier(Modifier::REVERSED);
const MATCH: Style = Style::new().fg(Color::Black).bg(Color::Yellow);
const DIM: Style = Style::new().add_modifier(Modifier::DIM);

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    app.resize(area.width, area.height.saturating_sub(1));

    let tw = app.text_width as u16;
    let left = area.width.saturating_sub(tw) / 2;
    let body = Rect {
        x: left,
        y: 0,
        width: tw.min(area.width),
        height: app.height as u16,
    };
    let sel = app.selection();

    let end = (app.top + app.height).min(app.rows.len());
    let lines: Vec<Line> = (app.top..end).map(|r| row_line(app, r, sel)).collect();
    f.render_widget(Paragraph::new(lines), body);

    // Status row.
    let status = Rect {
        x: 0,
        y: area.height.saturating_sub(1),
        width: area.width,
        height: 1,
    };
    let (left_txt, prompt) = match &app.mode {
        Mode::Search { buf, forward } => {
            (format!("{}{}", if *forward { '/' } else { '?' }, buf), true)
        }
        Mode::Command(buf) => (format!(":{buf}"), true),
        _ => (
            app.msg.clone().unwrap_or_else(|| match app.mode {
                Mode::Visual => "-- VISUAL --".into(),
                Mode::VisualLine => "-- VISUAL LINE --".into(),
                _ => app.name.clone(),
            }),
            false,
        ),
    };
    let pct = if app.rows.len() <= app.height {
        "All".to_string()
    } else if app.top == 0 {
        "Top".into()
    } else if app.top + app.height >= app.rows.len() {
        "Bot".into()
    } else {
        format!("{}%", app.top * 100 / (app.rows.len() - app.height))
    };
    let para = app.rows.get(app.cur).map_or(0, |r| r.para) + 1;
    let pending = match app.pending {
        Some('h') => "gy".to_string(),
        Some('Y') => "yg".to_string(),
        Some(c) => c.to_string(),
        None => String::new(),
    };
    let right = format!("{pending}  ¶ {para}/{}  {pct} ", app.paras.len());
    let left_style = if prompt || app.msg.is_some() {
        Style::new()
    } else {
        DIM
    };
    f.render_widget(
        Paragraph::new(Span::styled(format!(" {left_txt}"), left_style)),
        status,
    );
    f.render_widget(
        Paragraph::new(Span::styled(right, DIM)).alignment(Alignment::Right),
        status,
    );

    // Use the terminal's real cursor so it follows the user's shape/blink config.
    let pos = if prompt {
        Position::new(1 + left_txt.chars().count() as u16, status.y)
    } else {
        let row = &app.rows[app.cur];
        let (indent, x) = app.paras.get(row.para).map_or((0, 0), |p| {
            let b = p.text[row.start..row.end]
                .char_indices()
                .nth(app.col)
                .map_or(row.end, |(i, _)| row.start + i);
            (indent(p), width_between(p, row.start, b))
        });
        Position::new(left + (indent + x) as u16, (app.cur - app.top) as u16)
    };
    f.set_cursor_position(pos);
}

/// Build one display row, splitting into spans only where the style changes.
fn row_line<'a>(app: &'a App, r: usize, sel: Option<(Pos, Pos)>) -> Line<'a> {
    let row = app.rows[r];
    let Some(p) = app.paras.get(row.para) else {
        return Line::default();
    };
    let in_sel = |b: usize| {
        sel.is_some_and(|(lo, hi)| {
            (Pos {
                para: row.para,
                byte: b,
            }) >= lo
                && Pos {
                    para: row.para,
                    byte: b,
                } < hi
        })
    };

    if row.gap {
        // A selected gap shows that the paragraph break (newline) is included.
        return if in_sel(row.start) {
            Line::from(Span::styled(" ", SELECT))
        } else {
            Line::default()
        };
    }

    let base = if p.kind == Kind::Heading {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let mut spans = Vec::new();
    let gutter = indent(p);
    if gutter > 0 {
        // Hanging indent: marker on the first row, aligned blank space after.
        let g = if row.first {
            let nest =
                gutter - p.marker.chars().count().min(gutter) - usize::from(!p.marker.is_empty());
            format!("{}{} ", " ".repeat(nest), p.marker)
        } else {
            " ".repeat(gutter)
        };
        spans.push(Span::styled(g, DIM));
    }
    let mut nth_tab = p.text[..row.start].matches('\t').count();

    // Matches in this paragraph (matches are sorted by position).
    let first = app.matches.partition_point(|m| m.0.para < row.para);
    let hits = app.matches[first..]
        .iter()
        .take_while(|m| m.0.para == row.para);
    let hits: Vec<_> = hits
        .filter(|m| m.1 > row.start && m.0.byte < row.end)
        .collect();

    let (mut run, mut run_style) = (String::new(), base);
    for (i, c) in p.text[row.start..row.end].char_indices() {
        let b = row.start + i;
        let mut st = base;
        if hits.iter().any(|m| m.0.byte <= b && b < m.1) {
            st = st.patch(MATCH);
        }
        if in_sel(b) {
            st = st.patch(SELECT);
        }
        if st != run_style && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
        }
        run_style = st;
        if c == '\t' && p.kind == Kind::TableRow {
            // Column padding + divider; the clipboard still receives one tab.
            if !run.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut run), run_style));
            }
            let pad = tab_width(p, nth_tab).saturating_sub(COL_SEP);
            let style = if in_sel(b) { SELECT } else { DIM };
            spans.push(Span::styled(format!("{} │ ", " ".repeat(pad)), style));
            nth_tab += 1;
        } else if c == '\t' {
            run.push_str(&" ".repeat(tab_width(p, nth_tab)));
            nth_tab += 1;
        } else {
            run.push(c)
        }
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style));
    }
    Line::from(spans)
}
