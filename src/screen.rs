//! Headless terminal grid → text rendering on top of `alacritty_terminal`.
//!
//! We keep a `Term` fed by a `vte` parser and render its visible grid (plus
//! optional scrollback) to plain text — that is the whole point of the
//! emulator: get vim/htop/REPLs to render correctly without a GPU.

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::Processor;

/// No-op event sink. We never act on terminal events (bell, title, clipboard);
/// we only care about the rendered grid.
#[derive(Clone, Copy)]
pub struct NullSink;

impl EventListener for NullSink {
    fn send_event(&self, _event: Event) {}
}

/// Minimal `Dimensions` for constructing/resizing a `Term`.
#[derive(Clone, Copy)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

impl alacritty_terminal::grid::Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.cols as usize
    }
}

/// A terminal emulator instance: the parser plus the grid state.
pub struct Emulator {
    parser: Processor,
    term: Term<NullSink>,
    size: Size,
}

impl Emulator {
    pub fn new(cols: u16, rows: u16, scrollback: usize) -> Self {
        let size = Size {
            cols: cols.max(1),
            rows: rows.max(1),
        };
        let config = Config {
            scrolling_history: scrollback,
            ..Default::default()
        };
        Self {
            parser: Processor::new(),
            term: Term::new(config, &size, NullSink),
            size,
        }
    }

    /// Feed raw PTY output bytes through the VT parser.
    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.size = Size {
            cols: cols.max(1),
            rows: rows.max(1),
        };
        self.term.resize(self.size);
    }

    pub fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// (row, col) of the cursor within the visible viewport, 0-indexed.
    pub fn cursor(&self) -> (usize, usize) {
        let p = self.term.grid().cursor.point;
        (p.line.0.max(0) as usize, p.column.0)
    }

    /// Render the visible screen (and `scrollback` lines above it) as text,
    /// with trailing blank columns and trailing blank lines trimmed.
    pub fn render(&self, scrollback: usize) -> String {
        let grid = self.term.grid();
        let cols = self.size.cols as usize;
        let rows = self.size.rows as usize;
        let history = grid.history_size();
        let start = -(history.min(scrollback) as i32);

        let mut out = String::with_capacity((rows + scrollback) * (cols + 1));
        let mut lines: Vec<String> = Vec::with_capacity(rows + scrollback);
        for line in start..rows as i32 {
            let mut s = String::with_capacity(cols);
            for col in 0..cols {
                let cell = &grid[Line(line)][Column(col)];
                // Skip the spacer cell that follows a wide char.
                if cell
                    .flags
                    .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER)
                {
                    continue;
                }
                s.push(cell.c);
            }
            lines.push(s.trim_end().to_string());
        }
        // Drop trailing empty lines so a mostly-idle screen isn't 30 blank rows.
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        for (i, l) in lines.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(l);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_plain_text() {
        let mut e = Emulator::new(20, 5, 100);
        e.advance(b"hello");
        assert_eq!(e.render(0), "hello");
    }

    #[test]
    fn handles_newlines_and_cr() {
        let mut e = Emulator::new(20, 5, 100);
        e.advance(b"line1\r\nline2");
        assert_eq!(e.render(0), "line1\nline2");
    }

    #[test]
    fn cursor_movement_escape() {
        let mut e = Emulator::new(20, 5, 100);
        // Move cursor to row 3 col 5 (1-indexed in ANSI), write X.
        e.advance(b"\x1b[3;5HX");
        let (row, col) = e.cursor();
        assert_eq!(row, 2);
        assert_eq!(col, 5); // cursor advances past the X
    }

    #[test]
    fn alt_screen_toggle() {
        let mut e = Emulator::new(20, 5, 100);
        assert!(!e.alt_screen());
        e.advance(b"\x1b[?1049h");
        assert!(e.alt_screen());
        e.advance(b"\x1b[?1049l");
        assert!(!e.alt_screen());
    }

    #[test]
    fn trims_trailing_blanks() {
        let mut e = Emulator::new(40, 10, 100);
        e.advance(b"one\r\ntwo\r\n");
        // Only two non-empty lines despite a 10-row grid.
        assert_eq!(e.render(0), "one\ntwo");
    }

    #[test]
    fn scrollback_captured() {
        let mut e = Emulator::new(10, 2, 100);
        for i in 0..10 {
            e.advance(format!("row{i}\r\n").as_bytes());
        }
        let full = e.render(100);
        assert!(
            full.contains("row0"),
            "scrollback should retain row0: {full:?}"
        );
    }
}
