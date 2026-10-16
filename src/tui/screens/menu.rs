//! Main menu: the actions on the left, what the highlighted one does on the right.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::tui::app::App;
use crate::tui::widgets::block;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Auto,
    Partition,
    Repo,
    Packages,
    Services,
    Import,
    Export,
    Status,
    Info,
    Quit,
}

pub struct Item {
    pub action: Action,
    pub key: char,
    pub label: &'static str,
    pub about: &'static str,
    pub changes: &'static str,
    pub root: bool,
    pub auto: &'static str,
}

pub const ITEMS: [Item; 10] = [
    Item {
        action: Action::Auto,
        key: '1',
        label: "Run automatic setup",
        about: "Upgrades the system (pacman -Syuu), adds kud3n's repository, installs every package, then sets up services. \
                Steps 3 to 5 in one go, with an overall progress bar and a summary at the end.",
        changes: "/etc/pacman.conf, installed packages, /etc/pam.d files (backups kept), Howdy config, systemd services",
        root: true,
        auto: "This is the automatic run (no data partition, no import).",
    },
    Item {
        action: Action::Partition,
        key: '2',
        label: "Set up data partition",
        about: "Mounts the NTFS data partition at /media/data through /etc/fstab and moves Desktop, Documents, Downloads, \
                Music, Pictures, Public, Templates, Videos and Projects onto it, leaving symlinks in your home.",
        changes: "/etc/fstab (backup kept), your home folders, ~/.config/user-dirs.dirs",
        root: true,
        auto: "Optional: not part of the automatic run.",
    },
    Item {
        action: Action::Repo,
        key: '3',
        label: "Add kud3n's repository",
        about: "Adds the [archrepo] repository to /etc/pacman.conf (or updates its server URL), then syncs and upgrades with pacman -Syyu.",
        changes: "/etc/pacman.conf, upgraded packages",
        root: true,
        auto: "Part of the automatic run (step 2).",
    },
    Item {
        action: Action::Packages,
        key: '4',
        label: "Install packages",
        about: "Installs the official, CachyOS, kud3n and add-on package lists. Anything pacman can't install is retried with yay \
                as your user.",
        changes: "Installed packages; a temporary sudoers rule for yay, removed right after",
        root: true,
        auto: "Part of the automatic run (step 3).",
    },
    Item {
        action: Action::Services,
        key: '5',
        label: "Set up services",
        about: "Installs ProtonVPN and Fcitx5, finds the IR camera and configures Howdy (PAM rules and resume hook), \
                then enables valkey and searxng.",
        changes: "/etc/pam.d files (backups kept), Howdy config, sleep hook, systemd services",
        root: true,
        auto: "Part of the automatic run (step 4).",
    },
    Item {
        action: Action::Import,
        key: '6',
        label: "Import data",
        about: "Restores an App Vault archive (.zip) or a Howdy face model (.dat). The archive's manifest is shown first, \
                and you pick exactly what to restore.",
        changes: "App folders in ~/.config, KDE / Hyprland settings, /etc/howdy/models (a safety snapshot is kept)",
        root: true,
        auto: "Optional: not part of the automatic run.",
    },
    Item {
        action: Action::Export,
        key: '7',
        label: "Export data",
        about: "Backs up app data, KDE Plasma or Hyprland settings, or your Howdy face model into a .zip archive (or a .dat).",
        changes: "Only the new backup file",
        root: false,
        auto: "Not part of the automatic run.",
    },
    Item {
        action: Action::Status,
        key: '8',
        label: "App status",
        about: "Shows which applications are installed, their config folders and sizes, and whether they are running; \
                plus Howdy, KDE Plasma and Hyprland.",
        changes: "Nothing (read-only)",
        root: false,
        auto: "Not part of the automatic run.",
    },
    Item {
        action: Action::Info,
        key: '9',
        label: "Inspect a backup",
        about: "Shows what a backup archive contains: when and where it was made and which apps are inside.",
        changes: "Nothing (read-only)",
        root: false,
        auto: "Not part of the automatic run.",
    },
    Item { action: Action::Quit, key: 'q', label: "Quit", about: "Leave kast.", changes: "Nothing", root: false, auto: "" },
];

#[derive(Default, Debug, Clone)]
pub struct MenuState {
    pub selected: usize,
}

impl MenuState {
    pub fn up(&mut self) {
        self.selected = (self.selected + ITEMS.len() - 1) % ITEMS.len();
    }
    pub fn down(&mut self) {
        self.selected = (self.selected + 1) % ITEMS.len();
    }
    pub fn action(&self) -> Action {
        ITEMS[self.selected].action
    }
}

/// (list, details) areas; side by side from 100 columns, stacked below.
pub fn layout(body: Rect) -> (Rect, Rect) {
    if body.width >= 100 {
        let [l, r] = Layout::horizontal([Constraint::Length(40), Constraint::Min(30)]).areas(body);
        (l, r)
    } else {
        let list_h = ITEMS.len() as u16 + 2;
        let [l, r] = Layout::vertical([Constraint::Length(list_h), Constraint::Min(3)]).areas(body);
        (l, r)
    }
}

/// Index of the menu row at screen row `y`, if any.
pub fn row_at(body: Rect, x: u16, y: u16) -> Option<usize> {
    let (list, _) = layout(body);
    let inside = x > list.x && x < list.right().saturating_sub(1) && y > list.y && y < list.bottom().saturating_sub(1);
    let idx = (y - list.y).checked_sub(1)? as usize;
    (inside && idx < ITEMS.len()).then_some(idx)
}

pub fn view(f: &mut Frame, body: Rect, app: &App) {
    let theme = &app.theme;
    let (list_area, desc_area) = layout(body);
    let items: Vec<ListItem> = ITEMS
        .iter()
        .map(|it| {
            let key = if it.key == 'q' { "q".to_string() } else { it.key.to_string() };
            ListItem::new(Line::from(vec![Span::styled(format!(" {key} "), theme.bold()), Span::raw(it.label)]))
        })
        .collect();
    let list = List::new(items)
        .block(block(theme, " Arch Linux / CachyOS setup "))
        .highlight_style(theme.selected())
        .highlight_symbol(format!("{} ", theme.sym.pointer));
    let mut st = ListState::default().with_selected(Some(app.menu.selected));
    f.render_stateful_widget(list, list_area, &mut st);

    let it = &ITEMS[app.menu.selected];
    // Stacked (narrow) layout has little height: skip the heading and spacing
    let mut lines = if body.width >= 100 {
        vec![Line::styled(it.label, theme.title()), Line::raw(""), Line::raw(it.about), Line::raw("")]
    } else {
        vec![Line::raw(it.about)]
    };
    if it.action != Action::Quit {
        lines.push(Line::from(vec![Span::styled("Changes:    ", theme.bold()), Span::raw(it.changes)]));
        let root = if it.root { "yes (kast already runs as root)" } else { "no" };
        lines.push(Line::from(vec![Span::styled("Needs root: ", theme.bold()), Span::raw(root)]));
        lines.push(Line::from(vec![Span::styled("Automatic:  ", theme.bold()), Span::raw(it.auto)]));
    }
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }).block(block(theme, " Details ")), desc_area);
}

pub const HINTS: [(&str, &str); 6] = [("↑/↓", "move"), ("Enter", "run"), ("1-9", "pick"), ("l", "log"), ("?", "help"), ("q", "quit")];

pub const HELP: [(&str, &str); 9] = [
    ("↑/↓  j/k", "Move the selection (wraps around)"),
    ("Home/End  g/G", "First / last entry"),
    ("Enter", "Run the highlighted action"),
    ("1-9", "Run that action directly"),
    ("Mouse", "Click selects, double-click runs, wheel moves"),
    ("l", "Open the log file"),
    ("?", "This help"),
    ("q  Esc", "Quit kast"),
    ("Ctrl+C", "Quit kast"),
];
