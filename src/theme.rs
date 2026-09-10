//! Design tokens for the TUI (U12, amended U29).
//!
//! Three rules hold this file together:
//!
//! 1. **spar paints its own page.** [`BG`] is the ground every other surface is
//!    mixed against, so a raised band, a sunken output block and a fading gutter
//!    have something to be raised or sunken *from*. This replaces U12's "the
//!    background belongs to the terminal": that rule bought host-theme
//!    transparency and cost every layering effect the record views need (U29).
//! 2. **One accent, one alert.** [`ACCENT`] is the only "this is spar" colour and
//!    [`ALERT`] the only "this is broken" one. Everything else is a text weight or
//!    a semantic state.
//! 3. **Depth is background, hierarchy is foreground.** A surface says what kind of
//!    thing you are looking at; a text weight says how much it matters. Never use
//!    one to do the other's job. The raised and sunken surfaces the record views
//!    need arrive with their first caller (feature 010), not ahead of it.
//!
//! Colours are 24-bit. The three text weights are fixed rather than ANSI slots:
//! ANSI 7/8 vary too much between themes to build a hierarchy on, and now that we
//! own the ground there is nothing to inherit from anyway.

use ratatui::style::{Color, Modifier, Style};

// ---------------------------------------------------------------------------
// Surfaces
// ---------------------------------------------------------------------------

/// The page. Everything composites onto this.
pub const BG: Color = Color::Rgb(13, 14, 18);
/// Modals, the palette, the help window.
pub const BG_OVERLAY: Color = Color::Rgb(36, 40, 49);

/// Text on top of a filled chip. Chips bring their own saturated background, so
/// this is the one colour that must not follow the surface stack.
pub const INK: Color = Color::Rgb(13, 14, 18);

// ---------------------------------------------------------------------------
// Text weights
// ---------------------------------------------------------------------------

/// Primary text.
pub const FG: Color = Color::Rgb(228, 232, 240);
/// Secondary text: labels, metadata, inactive tabs.
pub const FG_DIM: Color = Color::Rgb(150, 158, 172);
/// Tertiary text: separators, hints, anything the eye should skip.
pub const FG_MUTED: Color = Color::Rgb(105, 113, 128);
/// Rules, seams, scrollbar tracks.
pub const RULE: Color = Color::Rgb(58, 64, 76);

// ---------------------------------------------------------------------------
// Accent and semantics
// ---------------------------------------------------------------------------

/// The one accent: focus, selection, active tab, spar's own marks. Periwinkle
/// rather than the GitHub-dark blue U12 shipped with, so spar reads as itself.
pub const ACCENT: Color = Color::Rgb(124, 141, 255);
/// Accent at rest: scrollbar thumbs, palette selection.
pub const ACCENT_SOFT: Color = Color::Rgb(78, 90, 168);

/// The one alert: failures, stalls, orphans.
pub const ALERT: Color = Color::Rgb(255, 99, 94);
/// Gates and anything waiting on the operator.
pub const WARN: Color = Color::Rgb(232, 181, 74);
/// Finished, healthy, green tests.
pub const OK: Color = Color::Rgb(87, 200, 108);
/// Live work in flight. Pushed off [`ACCENT`]'s hue so the two never read as one.
pub const INFO: Color = Color::Rgb(72, 200, 214);
/// Agent identity (models, slot ids).
pub const HINT: Color = Color::Rgb(196, 133, 246);

// ---------------------------------------------------------------------------
// Washes: a full row that has to be loud, mixed against [`BG`]
// ---------------------------------------------------------------------------

/// Full-row wash behind a broken or abandoned run. Deliberately loud.
pub const ALERT_WASH: Color = Color::Rgb(56, 24, 26);
/// Full-row wash behind Driving mode.
pub const DRIVE_WASH: Color = Color::Rgb(18, 54, 38);
/// Full-row wash behind a gate.
pub const GATE_WASH: Color = Color::Rgb(52, 40, 16);

// ---------------------------------------------------------------------------
// Motion endpoints
// ---------------------------------------------------------------------------

/// The breathing gutter's floor: a rail beside live work at its dimmest.
pub const PULSE_LO: Color = Color::Rgb(40, 44, 54);
/// The breathing gutter's peak.
pub const PULSE_HI: Color = Color::Rgb(122, 134, 162);

/// How far down a live block's gutter fades before it stops. The head of the
/// block breathes between [`PULSE_LO`] and [`PULSE_HI`]; each row below it is
/// mixed this much further toward [`BG`], so a streaming block reads as having a
/// direction without any row needing to know how tall the block is.
pub const TRAIL_FALLOFF: f32 = 0.22;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Linear blend between two colours. `t` is clamped to `0.0..=1.0`; `0.0` is all
/// `a`. Non-RGB inputs (the ANSI slots, `Reset`) have no channels to mix, so they
/// snap at the midpoint rather than pretending to interpolate.
pub fn lerp(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let mix = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t) as u8;
            Color::Rgb(mix(ar, br), mix(ag, bg), mix(ab, bb))
        }
        _ if t < 0.5 => a,
        _ => b,
    }
}

/// A colour faded toward the page. `amount` of `1.0` is [`BG`] itself.
pub fn toward_bg(c: Color, amount: f32) -> Color {
    lerp(c, BG, amount)
}

/// The page style. Painted once over the whole frame; every band draws over it.
pub fn page() -> Style {
    Style::default().fg(FG).bg(BG)
}

/// A filled chip: [`INK`] text on a semantic fill, bold.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lerp_hits_both_endpoints_exactly() {
        let a = Color::Rgb(0, 10, 20);
        let b = Color::Rgb(100, 110, 120);
        assert_eq!(lerp(a, b, 0.0), a);
        assert_eq!(lerp(a, b, 1.0), b);
    }

    #[test]
    fn lerp_clamps_out_of_range_t() {
        let a = Color::Rgb(0, 0, 0);
        let b = Color::Rgb(200, 200, 200);
        assert_eq!(lerp(a, b, -5.0), a);
        assert_eq!(lerp(a, b, 5.0), b);
    }

    /// The trail fade has to reach the page, or a long block's gutter never
    /// resolves and reads as a second rule.
    #[test]
    fn toward_bg_resolves_to_the_page() {
        assert_eq!(toward_bg(PULSE_HI, 1.0), BG);
    }
}
