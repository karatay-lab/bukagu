//! Reusable dashboard widgets (Steps 4–5), all themed via [`crate::theme`].

use ratatui::style::Color;

use crate::core::SyncAction;
use crate::theme;

/// The theme color used to render a given action in the Review list.
pub fn action_color(action: &SyncAction) -> Color {
    match action {
        SyncAction::CreateDir { .. } => theme::CREATE_DIR,
        SyncAction::Copy { .. } => theme::COPY,
        SyncAction::Overwrite { .. } => theme::OVERWRITE,
        SyncAction::Delete { .. } => theme::DELETE,
    }
}

/// Human-readable byte size (binary units), e.g. `1.5 MiB`.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
    }
}
