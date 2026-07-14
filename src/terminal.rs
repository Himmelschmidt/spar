//! Embedded terminal widget (W3/W8).
//!
//! A [`TerminalPane`] owns a [`vt100::Parser`] fed by the raw output bytes of a
//! real `tmux -L spar attach` client running in a pseudo-terminal (PTY). The parsed
//! screen renders through tui-term's `PseudoTerminal`, and raw input (keys, mouse,
//! paste) is forwarded straight into the PTY, so the panel is a genuine tmux client:
//! the prefix key, copy-mode/scroll, splits, search and session switch are all real
//! because it IS tmux. The parser, rendering, and the input [`encode_key`] /
//! [`encode_mouse`] encoders are pure and unit-tested; the live path drives them
//! from the PTY.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver};
use std::sync::Mutex;
use std::thread;

/// Lines of scrollback the parser retains behind the visible screen.
const SCROLLBACK: usize = 1000;

/// A live `tmux attach` client hosted in a PTY: the master side (for resize/IO),
/// the client child (killed on drop), a writer for raw input, and a channel of
/// output bytes drained by [`TerminalPane::pump`].
struct Pty {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Mutex<Box<dyn Write + Send>>,
    rx: Receiver<Vec<u8>>,
}

impl Drop for Pty {
    fn drop(&mut self) {
        // Kill only our tmux CLIENT (this PTY's `attach` child). The tmux SESSION is
        // persistent and deliberately outlives the TUI, so we never kill it here —
        // dropping the client just detaches; the session (and any dev server in it)
        // keeps running.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A vt100 screen buffer fed from a real tmux client's PTY output, plus the live
/// PTY that produces it. Absent-PTY is the pure (test/offline) path.
pub struct TerminalPane {
    parser: vt100::Parser,
    rows: u16,
    cols: u16,
    /// Session this pane is attached to, for match/rebind decisions by the caller.
    session: Option<String>,
    /// Live PTY client, if attached.
    pty: Option<Pty>,
}

impl TerminalPane {
    pub fn new(rows: u16, cols: u16) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self {
            parser: vt100::Parser::new(rows, cols, SCROLLBACK),
            rows,
            cols,
            session: None,
            pty: None,
        }
    }

    /// Attach a real tmux client to `session` on the spar socket inside a PTY sized
    /// to this widget. Idempotent for the same session.
    pub fn attach(&mut self, session: &str) -> Result<()> {
        if self.session.as_deref() == Some(session) && self.pty.is_some() {
            return Ok(());
        }
        let pair = native_pty_system().openpty(PtySize {
            rows: self.rows,
            cols: self.cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut cmd = CommandBuilder::new("tmux");
        cmd.args(["-L", "spar", "attach", "-t", session]);
        let child = pair.slave.spawn_command(cmd)?;
        // The child owns the slave; drop our handle so the reader sees EOF when the
        // client exits.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        self.pty = Some(Pty {
            master: pair.master,
            child,
            writer: Mutex::new(writer),
            rx,
        });
        self.session = Some(session.to_string());
        Ok(())
    }

    /// The session this pane is bound to, if any.
    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// Whether the tmux `attach` client child is still running. A `Ctrl+b d`
    /// detach or the session ending makes the child exit, so this flips to `false`
    /// and lets the caller hand focus back to spar. With no PTY attached there is no
    /// client to be dead, so this reports alive.
    pub fn is_alive(&mut self) -> bool {
        let Some(pty) = self.pty.as_mut() else {
            return true;
        };
        matches!(pty.child.try_wait(), Ok(None))
    }

    /// Drain the PTY's output channel into the parser. Cheap to call every frame.
    pub fn pump(&mut self) {
        let mut chunks = Vec::new();
        if let Some(pty) = self.pty.as_ref() {
            while let Ok(bytes) = pty.rx.try_recv() {
                chunks.push(bytes);
            }
        }
        for bytes in chunks {
            self.parser.process(&bytes);
        }
    }

    /// Write raw bytes to the PTY (keys, mouse, paste). Returns whether the write
    /// succeeded; a no-op returning `false` when nothing is attached.
    pub fn write_input(&self, bytes: &[u8]) -> bool {
        let Some(pty) = self.pty.as_ref() else {
            return false;
        };
        let Ok(mut w) = pty.writer.lock() else {
            return false;
        };
        w.write_all(bytes).and_then(|()| w.flush()).is_ok()
    }

    /// Feed raw bytes straight into the parser (pure path; also used by tests).
    #[allow(dead_code)]
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Resize the screen buffer and, if attached, resize the PTY so tmux reflows the
    /// client to match the widget. No-op when unchanged.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if (rows, cols) == (self.rows, self.cols) {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.parser.set_size(rows, cols);
        if let Some(pty) = self.pty.as_ref() {
            let _ = pty.master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
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

/// Encode one crossterm key event into the terminal byte sequence to forward into
/// the PTY, or `None` for keys we don't model. Pure; exhaustively unit-tested.
pub fn encode_key(code: KeyCode, mods: KeyModifiers) -> Option<Vec<u8>> {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let alt = mods.contains(KeyModifiers::ALT);
    match code {
        KeyCode::Char(c) => {
            let mut out = Vec::new();
            // ALT sends ESC then the key.
            if alt {
                out.push(0x1b);
            }
            if ctrl {
                // Control byte: fold to uppercase, mask the low 5 bits. Covers
                // @(0x00) A-Z(0x01..0x1a) [(0x1b) \(0x1c) ](0x1d) ^(0x1e) _(0x1f);
                // Space and @ both fold to 0x00.
                out.push((c.to_ascii_uppercase() as u8) & 0x1f);
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            Some(out)
        }
        KeyCode::Enter => Some(vec![0x0d]),
        KeyCode::Tab => Some(vec![0x09]),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::F(n) => encode_function_key(n),
        _ => None,
    }
}

/// Encode a function key `F(n)` into its terminal sequence, or `None` past F12.
fn encode_function_key(n: u8) -> Option<Vec<u8>> {
    let seq: &[u8] = match n {
        1 => b"\x1bOP",
        2 => b"\x1bOQ",
        3 => b"\x1bOR",
        4 => b"\x1bOS",
        5 => b"\x1b[15~",
        6 => b"\x1b[17~",
        7 => b"\x1b[18~",
        8 => b"\x1b[19~",
        9 => b"\x1b[20~",
        10 => b"\x1b[21~",
        11 => b"\x1b[23~",
        12 => b"\x1b[24~",
        _ => return None,
    };
    Some(seq.to_vec())
}

/// Encode a mouse event as an SGR mouse sequence `\x1b[<{cb};{x};{y}{M|m}` for the
/// PTY. `col`/`row` are pane-relative and 0-based (the caller translates); the
/// emitted coords are 1-based. `None` for kinds we don't model. Pure; unit-tested.
pub fn encode_mouse(
    kind: MouseEventKind,
    col: u16,
    row: u16,
    mods: KeyModifiers,
) -> Option<Vec<u8>> {
    let mut cb: u16 = match kind {
        MouseEventKind::ScrollUp => 64,
        MouseEventKind::ScrollDown => 65,
        MouseEventKind::Down(MouseButton::Left) => 0,
        MouseEventKind::Down(MouseButton::Middle) => 1,
        MouseEventKind::Down(MouseButton::Right) => 2,
        MouseEventKind::Up(_) => 0,
        // Left-drag: button 0 plus the 32 motion bit.
        MouseEventKind::Drag(MouseButton::Left) => 32,
        _ => return None,
    };
    if mods.contains(KeyModifiers::SHIFT) {
        cb += 4;
    }
    if mods.contains(KeyModifiers::ALT) {
        cb += 8;
    }
    if mods.contains(KeyModifiers::CONTROL) {
        cb += 16;
    }
    let x = col + 1;
    let y = row + 1;
    // A button release is reported with a trailing `m`; everything else with `M`.
    let terminator = if matches!(kind, MouseEventKind::Up(_)) {
        'm'
    } else {
        'M'
    };
    Some(format!("\x1b[<{cb};{x};{y}{terminator}").into_bytes())
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

    // ── encode_key ────────────────────────────────────────────────────────────

    #[test]
    fn plain_char_is_its_utf8() {
        assert_eq!(
            encode_key(KeyCode::Char('a'), KeyModifiers::NONE),
            Some(vec![b'a'])
        );
        // Multibyte passes through verbatim.
        assert_eq!(
            encode_key(KeyCode::Char('é'), KeyModifiers::NONE),
            Some("é".as_bytes().to_vec())
        );
    }

    #[test]
    fn ctrl_letter_is_control_byte() {
        assert_eq!(
            encode_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(vec![0x03])
        );
        // Ctrl folds case: Ctrl+Shift+C is still 0x03.
        assert_eq!(
            encode_key(
                KeyCode::Char('C'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            Some(vec![0x03])
        );
    }

    #[test]
    fn ctrl_punctuation_and_space() {
        assert_eq!(
            encode_key(KeyCode::Char('['), KeyModifiers::CONTROL),
            Some(vec![0x1b])
        );
        assert_eq!(
            encode_key(KeyCode::Char('\\'), KeyModifiers::CONTROL),
            Some(vec![0x1c])
        );
        assert_eq!(
            encode_key(KeyCode::Char(']'), KeyModifiers::CONTROL),
            Some(vec![0x1d])
        );
        assert_eq!(
            encode_key(KeyCode::Char('^'), KeyModifiers::CONTROL),
            Some(vec![0x1e])
        );
        assert_eq!(
            encode_key(KeyCode::Char('_'), KeyModifiers::CONTROL),
            Some(vec![0x1f])
        );
        // Ctrl+Space and Ctrl+@ both encode NUL.
        assert_eq!(
            encode_key(KeyCode::Char(' '), KeyModifiers::CONTROL),
            Some(vec![0x00])
        );
        assert_eq!(
            encode_key(KeyCode::Char('@'), KeyModifiers::CONTROL),
            Some(vec![0x00])
        );
    }

    #[test]
    fn alt_char_is_esc_prefixed() {
        assert_eq!(
            encode_key(KeyCode::Char('b'), KeyModifiers::ALT),
            Some(vec![0x1b, b'b'])
        );
    }

    #[test]
    fn ctrl_alt_char_is_esc_then_control_byte() {
        assert_eq!(
            encode_key(
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            ),
            Some(vec![0x1b, 0x18])
        );
    }

    #[test]
    fn editing_keys() {
        assert_eq!(
            encode_key(KeyCode::Enter, KeyModifiers::NONE),
            Some(vec![0x0d])
        );
        assert_eq!(
            encode_key(KeyCode::Tab, KeyModifiers::NONE),
            Some(vec![0x09])
        );
        assert_eq!(
            encode_key(KeyCode::BackTab, KeyModifiers::NONE),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            encode_key(KeyCode::Backspace, KeyModifiers::NONE),
            Some(vec![0x7f])
        );
        assert_eq!(
            encode_key(KeyCode::Esc, KeyModifiers::NONE),
            Some(vec![0x1b])
        );
    }

    #[test]
    fn arrows_and_navigation() {
        assert_eq!(
            encode_key(KeyCode::Up, KeyModifiers::NONE),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key(KeyCode::Down, KeyModifiers::NONE),
            Some(b"\x1b[B".to_vec())
        );
        assert_eq!(
            encode_key(KeyCode::Right, KeyModifiers::NONE),
            Some(b"\x1b[C".to_vec())
        );
        assert_eq!(
            encode_key(KeyCode::Left, KeyModifiers::NONE),
            Some(b"\x1b[D".to_vec())
        );
        assert_eq!(
            encode_key(KeyCode::Home, KeyModifiers::NONE),
            Some(b"\x1b[H".to_vec())
        );
        assert_eq!(
            encode_key(KeyCode::End, KeyModifiers::NONE),
            Some(b"\x1b[F".to_vec())
        );
        assert_eq!(
            encode_key(KeyCode::PageUp, KeyModifiers::NONE),
            Some(b"\x1b[5~".to_vec())
        );
        assert_eq!(
            encode_key(KeyCode::PageDown, KeyModifiers::NONE),
            Some(b"\x1b[6~".to_vec())
        );
        assert_eq!(
            encode_key(KeyCode::Insert, KeyModifiers::NONE),
            Some(b"\x1b[2~".to_vec())
        );
        assert_eq!(
            encode_key(KeyCode::Delete, KeyModifiers::NONE),
            Some(b"\x1b[3~".to_vec())
        );
    }

    #[test]
    fn function_keys() {
        assert_eq!(
            encode_key(KeyCode::F(1), KeyModifiers::NONE),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            encode_key(KeyCode::F(4), KeyModifiers::NONE),
            Some(b"\x1bOS".to_vec())
        );
        assert_eq!(
            encode_key(KeyCode::F(5), KeyModifiers::NONE),
            Some(b"\x1b[15~".to_vec())
        );
        assert_eq!(
            encode_key(KeyCode::F(12), KeyModifiers::NONE),
            Some(b"\x1b[24~".to_vec())
        );
        assert_eq!(encode_key(KeyCode::F(13), KeyModifiers::NONE), None);
    }

    #[test]
    fn unmapped_key_is_none() {
        assert_eq!(encode_key(KeyCode::Null, KeyModifiers::NONE), None);
    }

    // ── encode_mouse ──────────────────────────────────────────────────────────

    #[test]
    fn mouse_scroll_up_and_down() {
        assert_eq!(
            encode_mouse(MouseEventKind::ScrollUp, 0, 0, KeyModifiers::NONE),
            Some(b"\x1b[<64;1;1M".to_vec())
        );
        assert_eq!(
            encode_mouse(MouseEventKind::ScrollDown, 0, 0, KeyModifiers::NONE),
            Some(b"\x1b[<65;1;1M".to_vec())
        );
    }

    #[test]
    fn mouse_left_down_and_release() {
        assert_eq!(
            encode_mouse(
                MouseEventKind::Down(MouseButton::Left),
                0,
                0,
                KeyModifiers::NONE
            ),
            Some(b"\x1b[<0;1;1M".to_vec())
        );
        // Release is reported with a lowercase `m` terminator.
        assert_eq!(
            encode_mouse(
                MouseEventKind::Up(MouseButton::Left),
                0,
                0,
                KeyModifiers::NONE
            ),
            Some(b"\x1b[<0;1;1m".to_vec())
        );
    }

    #[test]
    fn mouse_coords_are_one_based() {
        // Pane-relative col=4,row=2 -> SGR x=5,y=3.
        assert_eq!(
            encode_mouse(
                MouseEventKind::Down(MouseButton::Left),
                4,
                2,
                KeyModifiers::NONE
            ),
            Some(b"\x1b[<0;5;3M".to_vec())
        );
    }

    #[test]
    fn mouse_modifiers_add_to_button_code() {
        // Left-down 0 + CTRL(16) = 16.
        assert_eq!(
            encode_mouse(
                MouseEventKind::Down(MouseButton::Left),
                0,
                0,
                KeyModifiers::CONTROL
            ),
            Some(b"\x1b[<16;1;1M".to_vec())
        );
    }

    #[test]
    fn mouse_unmodeled_kind_is_none() {
        assert_eq!(
            encode_mouse(MouseEventKind::Moved, 0, 0, KeyModifiers::NONE),
            None
        );
        assert_eq!(
            encode_mouse(
                MouseEventKind::Drag(MouseButton::Right),
                0,
                0,
                KeyModifiers::NONE
            ),
            None
        );
    }
}
