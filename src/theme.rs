//! bukagu color theme — an ember gradient running from deep navy through red
//! and orange up to gold. Defined once here so every screen (onboarding browser,
//! sync dashboard, summaries) shares one palette.
//!
//! The ten swatches come from the project's reference palette:
//! `03071E 370617 6A040F 9D0208 D00000 DC2F02 E85D04 F48C06 FAA307 FFBA08`.

use ratatui::style::Color;

/// Build a `Color` from a packed `0xRRGGBB` value.
const fn hex(c: u32) -> Color {
    Color::Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

// --- Raw palette (the ten reference swatches, dark → light) ---
pub const RICH_BLACK: Color = hex(0x03071E);
pub const DARK_MAROON: Color = hex(0x370617);
pub const ROSEWOOD: Color = hex(0x6A040F);
pub const DARK_RED: Color = hex(0x9D0208);
pub const RED: Color = hex(0xD00000);
pub const VERMILION: Color = hex(0xDC2F02);
pub const PERSIMMON: Color = hex(0xE85D04);
pub const ORANGE: Color = hex(0xF48C06);
pub const AMBER: Color = hex(0xFAA307);
pub const GOLD: Color = hex(0xFFBA08);

/// The palette in display order — handy for banners and gradient bars.
pub const PALETTE: [Color; 10] = [
    RICH_BLACK,
    DARK_MAROON,
    ROSEWOOD,
    DARK_RED,
    RED,
    VERMILION,
    PERSIMMON,
    ORANGE,
    AMBER,
    GOLD,
];

// --- Semantic roles (what the UI actually references) ---
// Neutral text colors supplement the palette (it ships no light/neutral tone).
pub const BG: Color = RICH_BLACK;
pub const TEXT: Color = Color::Rgb(0xF5, 0xF3, 0xF0); // warm off-white body text
pub const TEXT_DIM: Color = Color::Rgb(0x9A, 0x8C, 0x88); // muted captions/help
pub const PANEL_BORDER: Color = ROSEWOOD;
pub const HEADER: Color = VERMILION;
pub const ACCENT: Color = GOLD;
pub const SELECTION: Color = GOLD; // highlighted row / active item

// Diff action colors (used in the Review list).
pub const COPY: Color = AMBER; // new file added to a destination
pub const OVERWRITE: Color = ORANGE; // changed file updated in place
pub const DELETE: Color = RED; // extra removed (only with --delete)
pub const CREATE_DIR: Color = DARK_RED; // directory created

// Status colors.
pub const SUCCESS: Color = AMBER;
pub const ERROR: Color = RED;

/// Wrap `text` in a truecolor ANSI foreground escape for plain stdout output
/// (used before the TUI takes over the screen). Non-RGB colors pass through.
pub fn ansi_fg(c: Color, text: &str) -> String {
    if let Color::Rgb(r, g, b) = c {
        format!("\x1b[38;2;{r};{g};{b}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Print a small ember banner to stdout: the name in gold above a bar showing
/// every palette swatch. Truecolor terminals render the full gradient.
pub fn print_banner() {
    let mut bar = String::new();
    for c in PALETTE {
        if let Color::Rgb(r, g, b) = c {
            bar.push_str(&format!("\x1b[48;2;{r};{g};{b}m  \x1b[0m"));
        }
    }
    println!("{}", ansi_fg(GOLD, "bukagu"));
    println!("{bar}");
}
