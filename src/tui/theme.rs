//! Colours and symbols. Only the 16 named ANSI colours are used, so the Arch TTY console
//! renders the same as a terminal emulator; `NO_COLOR` drops colour entirely and `TERM=linux`
//! swaps the Unicode symbols for ASCII. Colour is never the only signal: every state also
//! has a symbol or a word.

use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::BorderType;

#[derive(Clone, Debug)]
pub struct Symbols {
    pub ok: &'static str,
    pub fail: &'static str,
    pub warn: &'static str,
    pub pending: &'static str,
    pub pointer: &'static str,
    pub on: &'static str,
    pub off: &'static str,
    pub up_down: &'static str,
    pub left_right: &'static str,
    pub ellipsis: &'static str,
    pub radio_on: &'static str,
    pub start: &'static str,
    pub caret: &'static str,
    pub bullet: &'static str,
    pub spinner: &'static [&'static str],
    pub border: BorderType,
}

const UNICODE: Symbols = Symbols {
    ok: "✓",
    fail: "✗",
    warn: "⚠",
    pending: "·",
    pointer: "❯",
    on: "[x]",
    off: "[ ]",
    up_down: "↑/↓",
    left_right: "←/→",
    ellipsis: "…",
    radio_on: "(•)",
    start: "▶",
    caret: "▏",
    bullet: "•",
    spinner: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
    border: BorderType::Rounded,
};

const ASCII: Symbols = Symbols {
    ok: "+",
    fail: "x",
    warn: "!",
    pending: ".",
    pointer: ">",
    on: "[x]",
    off: "[ ]",
    up_down: "Up/Dn",
    left_right: "Lt/Rt",
    ellipsis: "...",
    radio_on: "(*)",
    start: ">",
    caret: "_",
    bullet: "*",
    spinner: &["|", "/", "-", "\\"],
    border: BorderType::Plain,
};

#[derive(Clone, Debug)]
pub struct Theme {
    pub color: bool,
    pub sym: Symbols,
}

impl Theme {
    pub fn from_env() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let linux_console = std::env::var("TERM").is_ok_and(|t| t == "linux");
        Theme::new(!no_color, linux_console)
    }

    pub fn new(color: bool, ascii: bool) -> Self {
        Theme { color, sym: if ascii { ASCII } else { UNICODE } }
    }

    fn fg(&self, c: Color) -> Style {
        if self.color { Style::new().fg(c) } else { Style::new() }
    }

    pub fn accent(&self) -> Style {
        self.fg(Color::Cyan)
    }
    pub fn title(&self) -> Style {
        self.fg(Color::Cyan).add_modifier(Modifier::BOLD)
    }
    pub fn dim(&self) -> Style {
        if self.color { Style::new().fg(Color::DarkGray) } else { Style::new().add_modifier(Modifier::DIM) }
    }
    pub fn ok(&self) -> Style {
        self.fg(Color::Green)
    }
    pub fn warn(&self) -> Style {
        self.fg(Color::Yellow)
    }
    pub fn error(&self) -> Style {
        self.fg(Color::Red).add_modifier(Modifier::BOLD)
    }
    pub fn info(&self) -> Style {
        self.fg(Color::Blue)
    }
    pub fn bold(&self) -> Style {
        Style::new().add_modifier(Modifier::BOLD)
    }
    /// Highlighted row: reverse video works with and without colour.
    pub fn selected(&self) -> Style {
        if self.color {
            Style::new().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        }
    }
    pub fn header(&self) -> Style {
        if self.color { Style::new().bg(Color::Blue).fg(Color::White) } else { Style::new().add_modifier(Modifier::REVERSED) }
    }
    pub fn gauge_overall(&self) -> Style {
        self.fg(Color::Green)
    }
    pub fn gauge_step(&self) -> Style {
        self.fg(Color::Yellow)
    }
}
