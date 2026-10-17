//! Full log file viewer (the last lines of kast.log).

use ratatui::Frame;
use ratatui::layout::Rect;

use super::run::{LogBuffer, Scroll, log_view};
use crate::report::Level;
use crate::tui::theme::Theme;

pub const MAX_LINES: usize = 5000;

#[derive(Debug, Clone, Default)]
pub struct LogState {
    pub log: LogBuffer,
    pub scroll: Scroll,
}

impl LogState {
    /// Lines of kast.log; `[ts] [LEVEL] message` lines are coloured by their level.
    pub fn from_lines(lines: Vec<String>) -> Self {
        let mut log = LogBuffer::default();
        for l in lines {
            let level = if l.contains("] [ERROR] ") {
                Level::Error
            } else if l.contains("] [WARN] ") {
                Level::Warn
            } else if l.contains("] [INFO] ") {
                Level::Info
            } else {
                Level::Raw
            };
            log.push(level, l);
        }
        LogState { log, scroll: Scroll::default() }
    }
}

/// `path` goes in the title, so the file can be found without these keys.
pub fn view(f: &mut Frame, body: Rect, theme: &Theme, st: &LogState, path: &str) {
    let sep = if theme.sym.ellipsis == "..." { "-" } else { "·" };
    log_view(f, body, theme, &st.log, st.scroll, &format!("Log {sep} {path}"));
}

pub const HINTS: [(&str, &str); 7] = [("PgUp/PgDn", "scroll"), ("g/G", "top/end"), ("e", "view"), ("y", "copy"), ("o", "open"), ("Esc", "back"), ("?", "help")];
pub const HELP: [(&str, &str); 11] = [
    ("PgUp/PgDn  ↑/↓", "Scroll"),
    ("g / G", "Oldest / newest line"),
    ("Mouse wheel", "Scroll"),
    ("e / E", "View the log in a pager / an editor"),
    ("o", "Open the log with the default app"),
    ("f", "Show the log in the file manager"),
    ("y", "Copy this session's log"),
    ("Y", "Copy the log file's path"),
    ("$KAST_PAGER", "Pager for e (else $PAGER, then less)"),
    ("tmux", "For y: set -g set-clipboard on"),
    ("Esc  q  Enter", "Back"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_from_log_lines() {
        let st = LogState::from_lines(vec![
            "[2026-10-17 10:00:00] [WARN] Issue recorded: x".into(),
            "resolving dependencies...".into(),
            "[2026-10-17 10:00:01] [ERROR] Failed".into(),
        ]);
        assert_eq!(st.log.get(0).unwrap().0, Level::Warn);
        assert_eq!(st.log.get(1).unwrap().0, Level::Raw);
        assert_eq!(st.log.get(2).unwrap().0, Level::Error);
    }
}
