//! The sign-in URL as a QR code, drawn with half blocks: two modules per character cell, a
//! 4-module quiet zone, dark modules as the foreground on a light background. Callers paint it
//! black on white whatever the theme: an inverted code often doesn't scan.

use qrcode::{Color, EcLevel, QrCode};

/// The code's lines (every line the same width), or None if the text doesn't fit a QR code.
pub fn half_blocks(text: &str) -> Option<Vec<String>> {
    let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::L).ok()?;
    let n = code.width();
    let colors = code.to_colors();
    let quiet = 4;
    let size = n + 2 * quiet;
    let dark =
        |x: usize, y: usize| -> bool { x >= quiet && y >= quiet && x < n + quiet && y < n + quiet && colors[(y - quiet) * n + (x - quiet)] == Color::Dark };
    let mut lines = Vec::new();
    let mut y = 0;
    while y < size {
        let line: String = (0..size)
            .map(|x| match (dark(x, y), y + 1 < size && dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            })
            .collect();
        lines.push(line);
        y += 2;
    }
    Some(lines)
}

/// For a plain terminal: black on bright white, reset at each line end.
pub fn ansi(text: &str) -> Option<String> {
    let lines = half_blocks(text)?;
    Some(lines.iter().map(|l| format!("\x1b[30;107m{l}\x1b[0m\n")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_zone_and_shape() {
        let lines = half_blocks("https://account.proton.me/sso#key=SECRETKEY").unwrap();
        let w = lines[0].chars().count();
        assert!(lines.iter().all(|l| l.chars().count() == w));
        // Version 2 or 3 at level L: 25 or 29 modules, plus 8 of quiet zone
        assert!(w == 33 || w == 37, "width {w}");
        assert_eq!(lines.len(), w.div_ceil(2));
        // The quiet zone is blank: first two rows and the first four columns
        assert!(lines[..2].iter().all(|l| l.trim().is_empty()));
        assert!(lines.iter().all(|l| l.chars().take(4).all(|c| c == ' ')));
        // The top-left finder pattern starts right after it
        assert_eq!(lines[2].chars().nth(4), Some('█'));
        assert_eq!(half_blocks("x").unwrap()[0].chars().count(), 21 + 8);
        assert!(ansi("x").unwrap().starts_with("\x1b[30;107m"));
    }
}
