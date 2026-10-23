//! What kast detects: apps with their config folder, size and processes, then Howdy, KDE
//! Plasma and Hyprland. Collected on a worker thread (sizes can take a moment).

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Cell, Paragraph, Row, Table, Wrap};

use crate::tui::app::{App, Command, Screen};
use crate::tui::theme::Theme;
use crate::tui::widgets::block;
use crate::vault::pid_list;
use crate::vault::status::{ModelStatus, StatusReport};

#[derive(Debug, Clone, Default)]
pub struct StatusState {
    pub report: Option<Box<StatusReport>>,
    pub loading: bool,
    pub scroll: u16,
}

fn state(app: &mut App) -> Option<&mut StatusState> {
    match &mut app.screen {
        Screen::Status(s) => Some(s),
        _ => None,
    }
}

pub fn on_report(app: &mut App, r: Box<StatusReport>) {
    if let Some(st) = state(app) {
        st.report = Some(r);
        st.loading = false;
    }
}

pub fn key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let st = state(app)?;
    match k.code {
        KeyCode::Char('r') if !st.loading => {
            st.loading = true;
            return Some(Command::LoadStatus);
        }
        KeyCode::Up | KeyCode::Char('k') => st.scroll = st.scroll.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => st.scroll = st.scroll.saturating_add(1).min(40),
        KeyCode::PageUp => st.scroll = st.scroll.saturating_sub(10),
        KeyCode::PageDown => st.scroll = st.scroll.saturating_add(10).min(40),
        KeyCode::Home | KeyCode::Char('g') => st.scroll = 0,
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => app.screen = Screen::Menu,
        _ => {}
    }
    None
}

pub fn mouse(app: &mut App, m: MouseEvent) {
    if let Some(st) = state(app) {
        match m.kind {
            MouseEventKind::ScrollUp => st.scroll = st.scroll.saturating_sub(1),
            MouseEventKind::ScrollDown => st.scroll = st.scroll.saturating_add(1).min(40),
            _ => {}
        }
    }
}

fn process(theme: &Theme, pids: &[i32], idle: &'static str) -> Span<'static> {
    if pids.is_empty() { Span::styled(idle, theme.dim()) } else { Span::styled(format!("{} running (PID {})", theme.sym.warn, pid_list(pids)), theme.warn()) }
}

pub fn view(f: &mut Frame, body: Rect, app: &App, st: &StatusState) {
    let theme = &app.theme;
    let Some(r) = &st.report else {
        let spinner = theme.sym.spinner[app.frame % theme.sym.spinner.len()];
        let msg = format!("{spinner} Checking apps, sizes and processes{}", theme.sym.ellipsis);
        f.render_widget(Paragraph::new(Line::styled(msg, theme.dim())).block(block(theme, " App status ")), body);
        return;
    };
    let wide = body.width >= 100;
    let [table_area, rest] = Layout::vertical([Constraint::Length(r.apps.len() as u16 + 3), Constraint::Min(3)]).areas(body);

    let rows: Vec<Row> = r
        .apps
        .iter()
        .map(|a| {
            let name = a.title.split(" (").next().unwrap_or(a.title).to_string();
            let (state, dir, size) = match &a.config {
                Some((d, s)) => (Span::styled(format!("{} installed", theme.sym.ok), theme.ok()), d.clone(), s.clone()),
                None => (Span::styled(format!("{} not found", theme.sym.pending), theme.dim()), String::new(), String::new()),
            };
            let mut cells = vec![Cell::from(name), Cell::from(state), Cell::from(size), Cell::from(process(theme, &a.pids, "stopped"))];
            if wide {
                cells.insert(2, Cell::from(dir));
            }
            Row::new(cells)
        })
        .collect();
    let (header, widths) = if wide {
        (
            vec!["App", "Status", "Config folder", "Size", "Process"],
            vec![Constraint::Length(13), Constraint::Length(12), Constraint::Min(20), Constraint::Length(6), Constraint::Length(28)],
        )
    } else {
        (vec!["App", "Status", "Size", "Process"], vec![Constraint::Length(13), Constraint::Length(12), Constraint::Length(6), Constraint::Min(10)])
    };
    let title = if st.loading { format!(" App status (refreshing{}) ", theme.sym.ellipsis) } else { " App status ".to_string() };
    f.render_widget(Table::new(rows, widths).header(Row::new(header).style(theme.bold())).block(block(theme, title)), table_area);

    let field = |k: &'static str, v: Span<'static>| Line::from(vec![Span::styled(format!("  {k:<12}"), theme.bold()), v]);
    let mut lines = vec![Line::styled("Howdy face recognition", theme.title())];
    let h = &r.howdy;
    lines.push(field("Package", h.package.clone().map_or_else(|| Span::styled("not installed", theme.dim()), Span::raw)));
    match &h.model {
        ModelStatus::Enrolled { path, size, perms } => {
            lines.push(field("Face model", Span::styled(format!("{} enrolled ({path}, {size})", theme.sym.ok), theme.ok())));
            let ok = perms.starts_with("600 root:root");
            let style = if ok { ratatui::style::Style::new() } else { theme.warn() };
            lines.push(field("Permissions", Span::styled(format!("{perms}{}", if ok { "" } else { " (expected 600 root:root)" }), style)));
            if let Some(faces) = &h.faces {
                lines.push(field("Profiles", Span::raw(format!("{faces} face profile(s)"))));
            }
        }
        ModelStatus::NeedsSudo => lines.push(field("Face model", Span::styled("run kast with sudo to inspect /etc/howdy/models", theme.dim()))),
        ModelStatus::NotEnrolled { path } => {
            lines.push(field("Face model", Span::styled(format!("{} not enrolled for {} ({path})", theme.sym.warn, r.user), theme.warn())));
            lines.push(field("Add one", Span::raw(crate::ir::enroll_command(&r.user, h.ir.supported()))));
        }
    }
    let ir_style = match &h.ir {
        crate::ir::IrState::Supported { pam: true, .. } => theme.ok(),
        crate::ir::IrState::Supported { .. } | crate::ir::IrState::Unusable { .. } => theme.warn(),
        _ => theme.dim(),
    };
    lines.push(field("IR emitter", Span::styled(crate::vault::status::ir_text(&h.ir), ir_style)));
    if h.ir.supported() && matches!(h.model, ModelStatus::Enrolled { .. }) {
        lines.push(Line::styled(format!("  {}", crate::ir::reenroll_note(&r.user)), theme.warn()));
    }
    if h.config_open {
        lines.push(field("Lock screen", Span::styled(crate::vault::status::LOCK_SCREEN_NOTE, theme.warn())));
    }
    lines.push(field("Process", process(theme, &h.pids, "idle")));
    lines.push(Line::raw(""));
    lines.push(Line::styled("KDE Plasma", theme.title()));
    let k = &r.kde;
    if k.detected {
        lines.push(field("Settings", Span::styled(format!("{} detected, snapshot {}", theme.sym.ok, k.snapshot_size), theme.ok())));
        lines.push(field("Config", Span::raw(k.config_home.clone())));
        lines.push(field("Data", Span::raw(k.data_home.clone())));
    } else {
        lines.push(field("Settings", Span::styled("not found", theme.dim())));
    }
    let session = |running: bool, name: &str| {
        if running {
            Span::styled(format!("{} {name} running", theme.sym.warn), theme.warn())
        } else {
            Span::styled(format!("{name} not running"), theme.dim())
        }
    };
    lines.push(field("Session", session(k.plasma_running, "plasmashell")));
    lines.push(Line::raw(""));
    lines.push(Line::styled("Hyprland", theme.title()));
    match &r.hypr.config {
        Some((dir, size)) => lines.push(field("Config", Span::styled(format!("{} {dir} ({size})", theme.sym.ok), theme.ok()))),
        None => lines.push(field("Config", Span::styled("not found", theme.dim()))),
    }
    lines.push(field("Session", session(r.hypr.running, "Hyprland")));
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((st.scroll, 0)).block(block(theme, " System ")), rest);
}

pub const HINTS: [(&str, &str); 4] = [("r", "refresh"), ("↑/↓", "scroll"), ("Esc", "back"), ("?", "help")];
pub const HELP: [(&str, &str); 4] =
    [("r", "Check again"), ("↑/↓  PgUp/PgDn", "Scroll the lower panel"), ("Esc  q  Enter", "Back to the menu"), ("?", "This help")];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::tests::{app, key, render};
    use crate::tui::app::{Event, update};
    use crate::tui::event::Loaded;
    use crate::vault::status::{AppStatus, HowdyStatus, HyprStatus, KdeStatus};

    fn report() -> StatusReport {
        let app = |n, title, app, config: Option<(&str, &str)>, pids: Vec<i32>| AppStatus {
            n,
            title,
            app,
            config: config.map(|(a, b)| (a.to_string(), b.to_string())),
            extras: vec![],
            pids,
        };
        StatusReport {
            user: "p3ter".into(),
            apps: vec![
                app(1, "BeeperTexts (~/.config/BeeperTexts)", "beeper", Some(("/home/p3ter/.config/BeeperTexts", "2.1G")), vec![4242]),
                app(2, "ZaloData (~/.config/ZaloData)", "zalo", None, vec![]),
                app(4, "Zen Browser (~/.config/zen)", "zen", Some(("/home/p3ter/.config/zen", "512M")), vec![]),
            ],
            howdy: HowdyStatus {
                package: Some("howdy-next (3.0.0-1)".into()),
                model: ModelStatus::Enrolled { path: "/etc/howdy/models/p3ter.dat".into(), size: "8.0K".into(), perms: "644 root:root".into() },
                faces: Some("2".into()),
                pids: vec![],
                ir: crate::ir::IrState::Supported {
                    device: "/dev/v4l/by-path/pci-0000:63:00.4-usb-0:1:1.2-video-index0".into(),
                    mode: crate::ir::Mode::Alternate,
                    pam: true,
                },
                config_open: false,
            },
            kde: KdeStatus {
                detected: true,
                config_home: "/home/p3ter/.config".into(),
                data_home: "/home/p3ter/.local/share".into(),
                snapshot_size: "96M".into(),
                plasma_running: true,
            },
            hypr: HyprStatus { config: None, running: false },
        }
    }

    #[test]
    fn loads_and_refreshes() {
        let mut a = app(80, 24);
        assert!(matches!(update(&mut a, key(KeyCode::Char('8'))), Some(Command::LoadStatus)));
        update(&mut a, Event::Loaded(Loaded::Status(Box::new(report()))));
        assert!(matches!(update(&mut a, key(KeyCode::Char('r'))), Some(Command::LoadStatus)));
        // No second refresh while one runs
        assert!(update(&mut a, key(KeyCode::Char('r'))).is_none());
        update(&mut a, key(KeyCode::Esc));
        assert!(matches!(a.screen, Screen::Menu));
    }

    #[test]
    fn snapshot_status_140x40() {
        let mut a = app(140, 40);
        update(&mut a, key(KeyCode::Char('8')));
        update(&mut a, Event::Loaded(Loaded::Status(Box::new(report()))));
        insta::assert_snapshot!(render(&a));
    }
}
