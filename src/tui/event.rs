//! Everything the UI thread reacts to arrives as one `Event` on one channel.

use ratatui::crossterm::event::{KeyEvent, MouseEvent};

#[derive(Debug)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    Resize(u16, u16),
    /// Redraw tick (only while something animates)
    Tick,
}
