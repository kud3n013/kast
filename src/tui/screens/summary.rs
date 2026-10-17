//! End of the automatic run: how it went and what is left to do.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};

use crate::setup::{HOWDY_RESTORE_HINT, PARTITION_ENABLED, Step, Summary, partition_hint};
use crate::tui::theme::Theme;
use crate::tui::widgets::block;

pub fn view(f: &mut Frame, body: Rect, theme: &Theme, s: &Summary) {
    let field = |name: &str, value: Span<'static>| Line::from(vec![Span::styled(format!("  {name:<18}"), theme.bold()), value]);
    let mut lines = vec![Line::raw("")];
    if s.issues.is_empty() {
        lines.push(Line::styled(format!("  {} Automatic setup completed successfully", theme.sym.ok), theme.ok().patch(theme.bold())));
    } else {
        lines.push(Line::styled(format!("  {} Automatic setup finished with {} issue(s)", theme.sym.warn, s.issues.len()), theme.warn().patch(theme.bold())));
    }
    lines.push(Line::raw(""));
    lines.push(field("Time elapsed", Span::raw(s.elapsed.clone())));
    lines.push(field("Target user", Span::raw(format!("{} (UID {}, GID {})", s.user, s.uid, s.gid))));
    let skipped = |st: Step| s.skipped.contains(&st);
    let partition = match &s.partition {
        Some(p) => Span::raw(p.clone()),
        None if PARTITION_ENABLED && skipped(Step::Partition) => Span::styled("skipped (menu option 2 or `sudo kast partition`)", theme.dim()),
        None => Span::styled(partition_hint(), theme.dim()),
    };
    lines.push(field("Data partition", partition));
    let repo = if s.repo_configured {
        Span::raw("configured in /etc/pacman.conf")
    } else if skipped(Step::Repo) {
        Span::styled("skipped", theme.dim())
    } else {
        Span::styled(format!("{} not configured", theme.sym.warn), theme.warn())
    };
    lines.push(field("kud3n repository", repo));
    lines.push(field("Log file", Span::raw(s.log_file.clone())));
    lines.push(field("Package list", Span::raw(crate::tui::screens::packages::sep(theme, &s.packages))));
    for (name, result) in [("Backup", &s.export), ("Restore", &s.import)] {
        if let Some(r) = result {
            let style = if r.starts_with("failed") { theme.error() } else { ratatui::style::Style::new() };
            lines.push(field(name, Span::styled(r.clone(), style)));
        }
    }
    let left_out: Vec<&str> = s.skipped.iter().map(|st| st.label()).collect();
    if !left_out.is_empty() {
        lines.push(field("Skipped steps", Span::styled(left_out.join(", "), theme.dim())));
    }
    if s.howdy_enrolled {
        lines.push(field("Howdy face model", Span::styled(format!("{} enrolled for {}", theme.sym.ok, s.user), theme.ok())));
    }
    if !s.issues.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled("  Issues", theme.warn().patch(theme.bold())));
        for i in &s.issues {
            lines.push(Line::from(vec![Span::styled(format!("    {} ", theme.sym.fail), theme.error()), Span::raw(i.clone())]));
        }
    }
    if !s.howdy_enrolled {
        lines.push(Line::raw(""));
        lines.push(Line::styled("  Howdy: no face model enrolled yet. Run:", theme.warn()));
        lines.push(Line::raw(format!("    sudo howdy -U {} add", s.user)));
        lines.push(Line::styled(format!("    {HOWDY_RESTORE_HINT}"), theme.dim()));
    }
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block(theme, " Summary ")), body);
}

pub const HINTS: [(&str, &str); 6] = [("Enter", "back to menu"), ("l", "full log"), ("?", "help"), ("e", "view"), ("o", "open"), ("y", "copy")];
pub const HELP: [(&str, &str); 7] = [
    ("Enter  Esc  q", "Back to the menu"),
    ("l", "Open the full log file"),
    ("e / E", "View in a pager ($KAST_PAGER) / editor"),
    ("o / f", "Open with the default app / file manager"),
    ("y / Y", "Copy this run's log / the log's path"),
    ("tmux", "For y: set -g set-clipboard on"),
    ("?", "This help"),
];
