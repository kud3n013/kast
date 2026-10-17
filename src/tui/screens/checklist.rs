//! A list of checkboxes, optionally under group headers: the step list of the automatic run
//! and the app picker. Space toggles the highlighted item (on a header: its whole group),
//! `a` / `n` / `i` check all, none, or invert. Items that can't be picked are drawn dim and
//! the cursor skips them; they keep their checked state for when they can be picked again.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};

use crate::tui::theme::Theme;
use crate::tui::widgets::block;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub key: String,
    pub label: String,
    /// Shown dim after the label ("installed")
    pub tag: Option<String>,
    pub checked: bool,
    /// False: drawn dim, skipped by the cursor, not counted as selected
    pub enabled: bool,
    /// Why it can't be picked ("needs zotero"), shown instead of the tag
    pub note: Option<String>,
}

impl Item {
    pub fn new(key: impl Into<String>, label: impl Into<String>, checked: bool) -> Self {
        Item { key: key.into(), label: label.into(), tag: None, checked, enabled: true, note: None }
    }

    /// Checked and pickable: what the selection holds.
    pub fn selected(&self) -> bool {
        self.checked && self.enabled
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Header(String),
    Item(Item),
}

/// What a key did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Not a checklist key
    Ignored,
    Handled,
    /// Enter
    Confirm,
    /// Esc
    Back,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Checklist {
    pub rows: Vec<Row>,
    pub cursor: usize,
}

impl Checklist {
    pub fn new(rows: Vec<Row>) -> Self {
        let mut c = Checklist { rows, cursor: 0 };
        c.cursor = c.first().unwrap_or(0);
        c
    }

    pub fn items(&self) -> impl Iterator<Item = &Item> {
        self.rows.iter().filter_map(|r| match r {
            Row::Item(i) => Some(i),
            Row::Header(_) => None,
        })
    }

    fn items_mut(&mut self) -> impl Iterator<Item = &mut Item> {
        self.rows.iter_mut().filter_map(|r| match r {
            Row::Item(i) => Some(i),
            Row::Header(_) => None,
        })
    }

    pub fn item(&self, key: &str) -> Option<&Item> {
        self.items().find(|i| i.key == key)
    }

    pub fn item_mut(&mut self, key: &str) -> Option<&mut Item> {
        self.items_mut().find(|i| i.key == key)
    }

    /// The highlighted item (None on a header).
    pub fn current(&self) -> Option<&Item> {
        match self.rows.get(self.cursor) {
            Some(Row::Item(i)) => Some(i),
            _ => None,
        }
    }

    /// Row indexes of the items under the header at `h`.
    fn group(&self, h: usize) -> std::ops::Range<usize> {
        let end = self.rows[h + 1..].iter().position(|r| matches!(r, Row::Header(_))).map_or(self.rows.len(), |p| h + 1 + p);
        h + 1..end
    }

    fn selectable(&self, i: usize) -> bool {
        match &self.rows[i] {
            Row::Item(it) => it.enabled,
            Row::Header(_) => self.group(i).any(|j| matches!(&self.rows[j], Row::Item(it) if it.enabled)),
        }
    }

    fn first(&self) -> Option<usize> {
        (0..self.rows.len()).find(|&i| self.selectable(i))
    }

    fn last(&self) -> Option<usize> {
        (0..self.rows.len()).rev().find(|&i| self.selectable(i))
    }

    /// One step up or down to the next pickable row, wrapping around.
    pub fn step(&mut self, down: bool) {
        let n = self.rows.len();
        for k in 1..=n {
            let i = if down { (self.cursor + k) % n } else { (self.cursor + n - k) % n };
            if self.selectable(i) {
                self.cursor = i;
                return;
            }
        }
    }

    /// Jump `by` rows (a page), stopping at the nearest pickable row; no wrapping.
    pub fn jump(&mut self, by: usize, down: bool) {
        let target = if down { (self.cursor + by).min(self.rows.len().saturating_sub(1)) } else { self.cursor.saturating_sub(by) };
        let near = if down {
            (target..self.rows.len()).find(|&i| self.selectable(i)).or_else(|| self.last())
        } else {
            (0..=target).rev().find(|&i| self.selectable(i)).or_else(|| self.first())
        };
        if let Some(i) = near {
            self.cursor = i;
        }
    }

    /// Keep the cursor on a pickable row after items were enabled or disabled.
    pub fn settle(&mut self) {
        if self.rows.is_empty() || self.cursor < self.rows.len() && self.selectable(self.cursor) {
            return;
        }
        self.cursor = self.cursor.min(self.rows.len() - 1);
        let before = self.cursor;
        self.step(true);
        if !self.selectable(self.cursor) {
            self.cursor = before;
        }
    }

    /// Space: flip the item, or on a header check the whole group (uncheck it when all are).
    pub fn toggle(&mut self) {
        match self.rows.get(self.cursor) {
            Some(Row::Item(it)) if it.enabled => {
                if let Some(Row::Item(it)) = self.rows.get_mut(self.cursor) {
                    it.checked = !it.checked;
                }
            }
            Some(Row::Header(_)) => {
                let g = self.group(self.cursor);
                let all = self.rows[g.clone()].iter().all(|r| !matches!(r, Row::Item(it) if it.enabled && !it.checked));
                for r in &mut self.rows[g] {
                    if let Row::Item(it) = r
                        && it.enabled
                    {
                        it.checked = !all;
                    }
                }
            }
            _ => {}
        }
    }

    /// `a` / `n`: every item, including ones that can't be picked right now (an add-on whose
    /// app was unchecked comes back checked when the app is).
    pub fn set_all(&mut self, on: bool) {
        self.items_mut().for_each(|i| i.checked = on);
    }

    pub fn invert(&mut self) {
        self.items_mut().filter(|i| i.enabled).for_each(|i| i.checked = !i.checked);
    }

    /// (selected, all items)
    pub fn count(&self) -> (usize, usize) {
        (self.items().filter(|i| i.selected()).count(), self.items().count())
    }

    /// Keys of the selected items, in list order.
    pub fn selected_keys(&self) -> Vec<String> {
        self.items().filter(|i| i.selected()).map(|i| i.key.clone()).collect()
    }

    /// The navigation, toggle and bulk keys. `page` is the visible height.
    pub fn key(&mut self, k: &KeyEvent, page: usize) -> Outcome {
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.step(false),
            KeyCode::Down | KeyCode::Char('j') => self.step(true),
            KeyCode::Home | KeyCode::Char('g') => self.cursor = self.first().unwrap_or(0),
            KeyCode::End | KeyCode::Char('G') => self.cursor = self.last().unwrap_or(0),
            KeyCode::PageUp => self.jump(page.max(1), false),
            KeyCode::PageDown => self.jump(page.max(1), true),
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Char('a') => self.set_all(true),
            KeyCode::Char('n') => self.set_all(false),
            KeyCode::Char('i') => self.invert(),
            KeyCode::Enter => return Outcome::Confirm,
            KeyCode::Esc => return Outcome::Back,
            _ => return Outcome::Ignored,
        }
        Outcome::Handled
    }

    /// First row drawn in a list `height` rows tall (the list scrolls to keep the cursor in view).
    pub fn offset(&self, height: usize) -> usize {
        (self.cursor + 1).saturating_sub(height.max(1))
    }

    /// A click on screen row `y` of a list drawn in `area`: highlight and toggle that row.
    pub fn click(&mut self, area: Rect, y: u16) -> bool {
        let inner_h = area.height.saturating_sub(2) as usize;
        if y <= area.y || y >= area.bottom().saturating_sub(1) {
            return false;
        }
        let i = (y - area.y - 1) as usize + self.offset(inner_h);
        if i >= self.rows.len() || !self.selectable(i) {
            return false;
        }
        self.cursor = i;
        self.toggle();
        true
    }

    /// "[x] label  tag" rows in a bordered box titled "<title> · N of M selected".
    pub fn view(&self, f: &mut Frame, area: Rect, theme: &Theme, title: &str) {
        let (on, off) = (theme.sym.on, theme.sym.off);
        let partial = "[-]";
        let indent = if self.rows.iter().any(|r| matches!(r, Row::Header(_))) { "  " } else { "" };
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| match r {
                Row::Header(label) => {
                    let g: Vec<&Item> = self.rows[self.group(i)].iter().filter_map(|r| if let Row::Item(it) = r { Some(it) } else { None }).collect();
                    let n = g.iter().filter(|it| it.selected()).count();
                    let mark = if n == 0 {
                        off
                    } else if n == g.iter().filter(|it| it.enabled).count() {
                        on
                    } else {
                        partial
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{mark} {label}"), theme.bold()),
                        Span::styled(format!("  {n}/{}", g.len()), theme.dim()),
                    ]))
                }
                Row::Item(it) if !it.enabled => {
                    let mut spans = vec![Span::styled(format!("{indent}{off} {}", it.label), theme.dim())];
                    if let Some(n) = it.note.as_ref().or(it.tag.as_ref()) {
                        spans.push(Span::styled(format!("  {n}"), theme.dim()));
                    }
                    ListItem::new(Line::from(spans))
                }
                Row::Item(it) => {
                    let mut spans = vec![Span::raw(format!("{indent}{} {}", if it.checked { on } else { off }, it.label))];
                    if let Some(t) = &it.tag {
                        spans.push(Span::styled(format!("  {t}"), theme.dim()));
                    }
                    ListItem::new(Line::from(spans))
                }
            })
            .collect();
        let (n, m) = self.count();
        let dot = if theme.sym.ellipsis == "…" { "·" } else { "-" };
        let list = List::new(items)
            .block(block(theme, format!(" {title} {dot} {n} of {m} selected ")))
            .highlight_style(theme.selected())
            .highlight_symbol(format!("{} ", theme.sym.pointer));
        let mut st = ListState::default().with_selected(Some(self.cursor)).with_offset(self.offset(area.height.saturating_sub(2) as usize));
        f.render_stateful_widget(list, area, &mut st);
    }
}

/// The checklist keys, for the footer and `?` help.
pub const HELP: [(&str, &str); 6] = [
    ("↑/↓  j/k", "Move (skips items that can't be picked)"),
    ("Home/End  g/G  PgUp/PgDn", "First / last / a page"),
    ("Space", "Toggle; on a group header, the whole group"),
    ("a  n  i", "Check all / none / invert"),
    ("Mouse", "Click toggles, wheel scrolls"),
    ("Enter  Esc", "Continue / go back (choices are kept)"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;

    fn k(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn grouped() -> Checklist {
        let mut off = Item::new("c", "C", true);
        off.enabled = false;
        off.note = Some("needs a".into());
        Checklist::new(vec![
            Row::Header("One".into()),
            Row::Item(Item::new("a", "A", true)),
            Row::Item(off),
            Row::Item(Item::new("b", "B", false)),
            Row::Header("Two".into()),
            Row::Item(Item::new("d", "D", true)),
        ])
    }

    #[test]
    fn navigation_skips_disabled_rows_and_wraps() {
        let mut c = grouped();
        assert_eq!(c.cursor, 0, "starts on the first header");
        let mut seen = vec![];
        for _ in 0..6 {
            c.key(&k(KeyCode::Down), 10);
            seen.push(c.cursor);
        }
        assert_eq!(seen, [1, 3, 4, 5, 0, 1], "row 2 (disabled) is skipped, the end wraps");
        c.key(&k(KeyCode::Up), 10);
        c.key(&k(KeyCode::Up), 10);
        assert_eq!(c.cursor, 5);
        c.key(&k(KeyCode::Home), 10);
        assert_eq!(c.cursor, 0);
        c.key(&k(KeyCode::End), 10);
        assert_eq!(c.cursor, 5);
        c.key(&k(KeyCode::PageUp), 3);
        assert_eq!(c.cursor, 1, "a page up from 5 lands on 2, which is skipped back to 1");
        c.key(&k(KeyCode::PageDown), 1);
        assert_eq!(c.cursor, 3);
        // A header whose items are all disabled can't be reached either
        let mut c = Checklist::new(vec![
            Row::Header("x".into()),
            Row::Item(Item { enabled: false, ..Item::new("x", "X", true) }),
            Row::Header("y".into()),
            Row::Item(Item::new("y", "Y", true)),
        ]);
        assert_eq!(c.cursor, 2);
        c.step(true);
        c.step(true);
        assert_eq!(c.cursor, 2, "wraps past the unreachable header");
    }

    #[test]
    fn space_on_items_and_headers_and_bulk_keys() {
        let mut c = grouped();
        assert_eq!(c.count(), (2, 4), "the disabled item doesn't count, even when checked");
        // Header "One": a checked, b not → check the group
        c.toggle();
        assert_eq!(c.selected_keys(), ["a", "b", "d"]);
        assert!(c.item("c").unwrap().checked, "disabled items keep their state");
        // Now all checked → uncheck the group
        c.toggle();
        assert_eq!(c.selected_keys(), ["d"]);
        c.key(&k(KeyCode::Down), 10);
        c.key(&k(KeyCode::Char(' ')), 10);
        assert_eq!(c.selected_keys(), ["a", "d"]);
        assert_eq!(c.key(&k(KeyCode::Char('n')), 10), Outcome::Handled);
        assert_eq!(c.count(), (0, 4));
        c.key(&k(KeyCode::Char('i')), 10);
        assert_eq!(c.selected_keys(), ["a", "b", "d"]);
        c.key(&k(KeyCode::Char('n')), 10);
        c.key(&k(KeyCode::Char('a')), 10);
        assert_eq!(c.count(), (3, 4));
        assert_eq!(c.key(&k(KeyCode::Enter), 10), Outcome::Confirm);
        assert_eq!(c.key(&k(KeyCode::Esc), 10), Outcome::Back);
        assert_eq!(c.key(&k(KeyCode::Char('z')), 10), Outcome::Ignored);
        // Enabling the item again brings back its checked state
        c.item_mut("c").unwrap().enabled = true;
        assert_eq!(c.selected_keys(), ["a", "c", "b", "d"]);
    }

    #[test]
    fn clicks_toggle_and_scrolling_follows_the_cursor() {
        let mut c = grouped();
        let area = Rect::new(0, 5, 30, 5); // three rows inside the border
        assert!(c.click(area, 7), "row 1 is item a");
        assert_eq!((c.cursor, c.item("a").unwrap().checked), (1, false));
        assert!(!c.click(area, 8), "row 2 is disabled");
        assert!(!c.click(area, 5), "the border");
        c.key(&k(KeyCode::End), 10);
        assert_eq!(c.offset(3), 3);
        assert!(c.click(area, 6), "first visible row is now row 3 (b)");
        assert!(c.item("b").unwrap().checked);
        c.cursor = 1;
        c.settle();
        assert_eq!(c.cursor, 1);
        c.item_mut("a").unwrap().enabled = false;
        c.settle();
        assert_eq!(c.cursor, 3, "moves off an item that became disabled");
    }
}
