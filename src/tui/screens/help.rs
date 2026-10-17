//! `?` overlay: the key bindings of the current screen.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};

use crate::tui::theme::Theme;
use crate::tui::widgets::{block, popup};

pub fn view(f: &mut Frame, area: Rect, theme: &Theme, title: &str, keys: &[(&str, &str)]) {
    let key_w = keys.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(4) + 2;
    let mut lines: Vec<Line> = keys
        .iter()
        .map(|(k, what)| {
            let k = if theme.sym.up_down == "↑/↓" { k.to_string() } else { ascii_keys(k) };
            Line::from(vec![Span::styled(format!("{k:key_w$}"), theme.bold()), Span::raw(what.to_string())])
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::styled("Press any key to close", theme.dim()));
    let w = (lines.iter().map(|l| l.width()).max().unwrap_or(30) as u16 + 4).min(area.width);
    // Rows that wrap on a narrow screen take more than one line
    let inner = w.saturating_sub(2).max(1) as usize;
    let rows: usize = lines.iter().map(|l| l.width().max(1).div_ceil(inner)).sum();
    let r = popup(f, area, w, rows as u16 + 2);
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block(theme, format!(" Help: {title} "))), r);
}

/// Key names for the Linux console font, which has no arrows.
pub fn ascii_keys(k: &str) -> String {
    k.replace("↑/↓", "Up/Down").replace("←/→", "Left/Right").replace('↑', "Up").replace('↓', "Down")
}
