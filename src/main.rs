//! wordless — a hyper-minimal, Vim-driven .docx pager.
//!
//!   wordless [--width N] [--dump | --html] FILE.docx|FILE.doc

mod app;
mod clipboard;
mod doc;
mod docx;
mod html;
mod layout;
mod numbering;
mod ui;

use std::{path::PathBuf, process::ExitCode, time::Duration};

use ratatui::crossterm::event::{self, Event, KeyEventKind};

use app::{App, Flow};

const USAGE: &str = "usage: wordless [--width N] [--dump | --html] FILE.docx|FILE.doc";

fn main() -> ExitCode {
    let mut width = 72usize; // comfortable reading measure (~66–80 chars)
    let mut dump = false;
    let mut html_out = false;
    let mut file: Option<PathBuf> = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(a) = args.next() {
        match a.to_str() {
            Some("-h" | "--help") => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            Some("--dump") => dump = true,
            Some("--html") => html_out = true,
            Some("-w" | "--width") => match args.next().and_then(|v| v.to_str()?.parse().ok()) {
                Some(w) => width = w,
                None => return fail("--width needs a number"),
            },
            _ => file = Some(a.into()),
        }
    }
    let Some(path) = file else { return fail(USAGE) };

    // Parse before touching the terminal: errors print cleanly, and the first
    // frame is the full document.
    let paras = match docx::open(&path) {
        Ok(p) => p,
        Err(e) => return fail(&format!("{}: {e}", path.display())),
    };

    if html_out {
        println!(
            "{}",
            html::render_paras(&paras, 0, paras.len().saturating_sub(1))
        );
        return ExitCode::SUCCESS;
    }
    if dump {
        let text: Vec<&str> = paras.iter().map(|p| p.text.as_str()).collect();
        println!("{}", text.join("\n"));
        return ExitCode::SUCCESS;
    }

    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let mut app = App::new(paras, name, width);

    let mut terminal = ratatui::init(); // raw mode + alt screen + panic-safe restore
    let res = run(&mut terminal, &mut app);
    ratatui::restore();
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e.to_string()),
    }
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> std::io::Result<()> {
    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        // Block for one event, then drain everything already queued before
        // redrawing. Holding `j` (key repeat) therefore never builds up lag:
        // 30 queued keypresses cost one frame, not 30.
        let mut ev = event::read()?;
        loop {
            if let Event::Key(k) = ev
                && k.kind == KeyEventKind::Press
                && app.handle_key(k) == Flow::Quit
            {
                return Ok(());
            }
            if let Some(text) = app.yank.take()
                && let Err(e) = clipboard::copy(&text)
            {
                app.msg = Some(e);
            }
            if !event::poll(Duration::ZERO)? {
                break;
            }
            ev = event::read()?;
        }
        // Event::Resize needs no handling: draw() re-wraps on width change.
    }
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("wordless: {msg}");
    ExitCode::FAILURE
}
