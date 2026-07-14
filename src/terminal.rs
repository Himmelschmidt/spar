//! Embedded terminal widget (W3).
//!
//! A [`TerminalPane`] owns a [`vt100::Parser`] fed by the control-mode `%output`
//! bytes of one tmux pane (Stage 8). The parsed screen buffer renders through
//! tui-term's `PseudoTerminal`, so the TUI can show a live agent pane without
//! polling `capture-pane`. The parser and rendering are pure and unit-tested; the
//! live attach path drives them from a [`ControlClient`].

use crate::tmux::{ControlClient, ControlEvent};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};

/// Lines of scrollback the parser retains behind the visible screen.
const SCROLLBACK: usize = 1000;

/// A vt100 screen buffer fed from a tmux pane's control-mode output, plus the
/// optional live control client that feeds it.
pub struct TerminalPane {
    parser: vt100::Parser,
    rows: u16,
    cols: u16,
    /// The tmux pane id (`%N`) we render. Bound to the first pane we see output
    /// from, so a single-pane session "just works" without prior discovery.
    pane_id: Option<String>,
    /// Live control client, if attached. Absent for the pure (test/offline) path.
    client: Option<ControlClient>,
    /// Session this pane is bound to, for match/rebind decisions by the caller.
    session: Option<String>,
}

impl TerminalPane {
    pub fn new(rows: u16, cols: u16) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self {
            parser: vt100::Parser::new(rows, cols, SCROLLBACK),
            rows,
            cols,
            pane_id: None,
            client: None,
            session: None,
        }
    }

    /// Attach a control client to a session on the spar socket and set its client
    /// size so tmux sizes the pane to match this widget. Idempotent for the same
    /// session; re-binds the tracked pane on (re)attach.
    pub fn attach(&mut self, session: &str) -> Result<()> {
        if self.session.as_deref() == Some(session) && self.client.is_some() {
            return Ok(());
        }
        let mut client = ControlClient::attach(session)?;
        let _ = client.send_command(&format!("refresh-client -C {}x{}", self.cols, self.rows));
        self.client = Some(client);
        self.session = Some(session.to_string());
        self.pane_id = None;
        Ok(())
    }

    /// The session this pane is bound to, if any.
    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// The `tmux send-keys` target for this pane: the bound pane id (`%N`) once we
    /// have seen output, else the session (whose active pane is the single agent
    /// pane). `None` before any binding exists.
    fn key_target(&self) -> Option<&str> {
        self.pane_id.as_deref().or(self.session.as_deref())
    }

    /// Forward one crossterm key event to the live pane via `tmux -L spar send-keys`.
    /// Returns `false` (a no-op) when the key isn't forwardable or nothing is
    /// attached yet — the caller still consumes it so it never leaks into TUI nav.
    pub fn send_key(&self, code: KeyCode, mods: KeyModifiers) -> bool {
        let Some(target) = self.key_target() else {
            return false;
        };
        let Some(key) = crate::tmux::map_key(code, mods) else {
            return false;
        };
        crate::tmux::send_key(target, &key).is_ok()
    }

    /// Drain all pending control events, feeding matching pane output into the
    /// parser. Cheap to call every frame; keeps the event channel from backing up.
    pub fn pump(&mut self) {
        let mut drained = Vec::new();
        if let Some(client) = self.client.as_ref() {
            while let Ok(ev) = client.events().try_recv() {
                drained.push(ev);
            }
        }
        for ev in drained {
            self.apply(&ev);
        }
    }

    /// Apply one control event: `%output` for the tracked pane advances the parser;
    /// everything else is ignored here (lifecycle is the caller's concern).
    pub fn apply(&mut self, ev: &ControlEvent) {
        if let ControlEvent::PaneOutput { pane_id, bytes } = ev {
            if self.pane_id.is_none() {
                self.pane_id = Some(pane_id.clone());
            }
            if self.pane_id.as_deref() == Some(pane_id.as_str()) {
                self.parser.process(bytes);
            }
        }
    }

    /// Feed raw bytes straight into the parser (pure path; also used by tests).
    #[allow(dead_code)]
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Resize the screen buffer and, if attached, tell tmux to resize the pane on
    /// the spar socket so its output matches the widget. No-op when unchanged.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if (rows, cols) == (self.rows, self.cols) {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.parser.set_size(rows, cols);
        if let Some(client) = self.client.as_mut() {
            let _ = client.send_command(&format!("refresh-client -C {cols}x{rows}"));
        }
    }

    #[allow(dead_code)]
    pub fn dims(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    /// The current parsed screen, for rendering.
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echoes_plain_text() {
        let mut pane = TerminalPane::new(4, 20);
        pane.feed(b"hello");
        assert_eq!(pane.screen().contents(), "hello");
    }

    #[test]
    fn handles_clear_home_and_newlines() {
        // ESC[2J clears, ESC[H homes the cursor, CR/LF split rows.
        let mut pane = TerminalPane::new(4, 20);
        pane.feed(b"\x1b[2J\x1b[HAB\r\nCD");
        let screen = pane.screen();
        assert_eq!(screen.cell(0, 0).unwrap().contents(), "A");
        assert_eq!(screen.cell(0, 1).unwrap().contents(), "B");
        assert_eq!(screen.cell(1, 0).unwrap().contents(), "C");
        assert_eq!(screen.cell(1, 1).unwrap().contents(), "D");
        assert_eq!(screen.contents(), "AB\nCD");
    }

    #[test]
    fn cursor_addressing_places_text() {
        // ESC[2;3H moves the cursor to row 2, col 3 (1-based) before printing.
        let mut pane = TerminalPane::new(5, 10);
        pane.feed(b"\x1b[2;3HX");
        assert_eq!(pane.screen().cell(1, 2).unwrap().contents(), "X");
    }

    #[test]
    fn sgr_color_is_parsed_into_cells() {
        // ESC[31m -> red foreground; the cell keeps the styled glyph.
        let mut pane = TerminalPane::new(2, 10);
        pane.feed(b"\x1b[31mR\x1b[0m");
        let cell = pane.screen().cell(0, 0).unwrap();
        assert_eq!(cell.contents(), "R");
        assert_eq!(cell.fgcolor(), vt100::Color::Idx(1));
    }

    #[test]
    fn apply_binds_first_pane_and_feeds_it() {
        let mut pane = TerminalPane::new(3, 10);
        pane.apply(&ControlEvent::PaneOutput {
            pane_id: "%7".to_string(),
            bytes: b"hi".to_vec(),
        });
        // A second, different pane's output is ignored once bound to %7.
        pane.apply(&ControlEvent::PaneOutput {
            pane_id: "%8".to_string(),
            bytes: b"XX".to_vec(),
        });
        pane.apply(&ControlEvent::PaneOutput {
            pane_id: "%7".to_string(),
            bytes: b"!".to_vec(),
        });
        assert_eq!(pane.screen().contents(), "hi!");
    }

    #[test]
    fn non_output_events_are_ignored() {
        let mut pane = TerminalPane::new(3, 10);
        pane.apply(&ControlEvent::WindowClose {
            window_id: "@1".to_string(),
        });
        assert_eq!(pane.screen().contents(), "");
    }

    #[test]
    fn resize_updates_screen_dimensions() {
        let mut pane = TerminalPane::new(4, 20);
        assert_eq!(pane.screen().size(), (4, 20));
        pane.resize(10, 40);
        assert_eq!(pane.dims(), (10, 40));
        assert_eq!(pane.screen().size(), (10, 40));
    }

    #[test]
    fn resize_clamps_zero_to_one() {
        let mut pane = TerminalPane::new(4, 20);
        pane.resize(0, 0);
        assert_eq!(pane.dims(), (1, 1));
    }
}
