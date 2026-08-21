//! Design tokens for the TUI (U12).
//!
//! Two rules hold this file together:
//!
//! 1. **The background belongs to the terminal.** Nothing paints a page background,
//!    so spar composites onto whatever theme (or transparency) the operator runs.
//!    The only backgrounds we set are chips, washes and overlays — small, deliberate,
//!    and always paired with [`INK`] so their text stays legible on any host theme.
//! 2. **One accent, one alert.** [`ACCENT`] is the only "this is spar" colour and
//!    [`ALERT`] the only "this is broken" one. Everything else is a text weight.
//!
//! The three text weights are fixed neutral greys rather than ANSI slots: ANSI 7/8
//! vary too much between themes to build a hierarchy on. They are picked to stay
//! readable against both a near-black and a near-white terminal; on a light theme
//! [`FG_DIM`] and [`FG_MUTED`] sit closer together than they do on a dark one.

use ratatui::style::{Color, Modifier, Style};

/// Text on top of a filled chip. Chips bring their own background, so this is the
/// one colour that must NOT follow the terminal theme.
pub const INK: Color = Color::Rgb(16, 18, 24);

/// Primary text: the terminal's own foreground.
pub const FG: Color = Color::Reset;
/// Secondary text: labels, metadata, inactive tabs.
pub const FG_DIM: Color = Color::Rgb(150, 158, 172);
/// Tertiary text: separators, hints, anything the eye should skip.
pub const FG_MUTED: Color = Color::Rgb(120, 128, 142);
/// Rules, seams, scrollbar tracks.
pub const RULE: Color = Color::Rgb(78, 86, 100);

/// The one accent: focus, selection, active tab, spar's own marks.
pub const ACCENT: Color = Color::Rgb(88, 166, 255);
/// Accent at rest — scrollbar thumbs, palette selection.
pub const ACCENT_SOFT: Color = Color::Rgb(56, 110, 180);

/// The one alert: failures, stalls, orphans.
pub const ALERT: Color = Color::Rgb(248, 81, 73);
/// Gates and anything waiting on the operator.
pub const WARN: Color = Color::Rgb(210, 168, 70);
/// Finished, healthy, green tests.
pub const OK: Color = Color::Rgb(63, 185, 80);
/// Live work in flight.
pub const INFO: Color = Color::Rgb(57, 190, 200);
/// Agent identity (models, slot ids).
pub const HINT: Color = Color::Rgb(188, 120, 240);

/// Full-row wash behind a broken or abandoned run. Deliberately loud.
pub const ALERT_WASH: Color = Color::Rgb(60, 26, 26);
/// Full-row wash behind Driving mode.
pub const DRIVE_WASH: Color = Color::Rgb(20, 60, 40);
/// Full-row wash behind a gate.
pub const GATE_WASH: Color = Color::Rgb(48, 36, 14);

/// A filled chip: `INK` text on a semantic fill, bold. The only place we set a
/// background on content.
pub fn chip(bg: Color) -> Style {
    Style::default().fg(INK).bg(bg).add_modifier(Modifier::BOLD)
}

/// Secondary text.
pub fn dim() -> Style {
    Style::default().fg(FG_DIM)
}

/// Tertiary text.
pub fn muted() -> Style {
    Style::default().fg(FG_MUTED)
}

/// Rules and seams.
pub fn rule() -> Style {
    Style::default().fg(RULE)
}

/// Selected-row emphasis: bold at full strength when the pane has focus, plain
/// otherwise. The accent bar in the lead column carries the rest of the signal.
pub fn selected(focused: bool) -> Style {
    if focused {
        Style::default().fg(FG).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(FG)
    }
}
