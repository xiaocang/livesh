use vt100::Parser;

/// Hard reset painted before the reconstructed screen so the attaching terminal
/// starts from a known base: leave the alternate screen, home the cursor, clear
/// the primary screen and its scrollback.
const RESET_SCREEN: &[u8] = b"\x1b[?1049l\x1b[H\x1b[2J\x1b[3J";

/// A live, in-memory render of the shell's screen.
///
/// Earlier revisions stored a raw byte ring and replayed it verbatim on
/// reattach. That re-emitted every terminal *query* the inner program had ever
/// written — Primary DA (`CSI c`), DSR (`CSI 6 n`), DECRQM (`CSI ? … $ p`),
/// XTVERSION (`CSI > q`), OSC color queries (`OSC 1x ; ? …`) — so the
/// reattaching terminal answered them all and the replies (`…c`, `…$y`, `>|…`,
/// `rgb:…`) landed on the idle shell prompt as garbage.
///
/// We now parse output into a real VT grid and, on reattach, replay the
/// *rendered* screen plus the input modes. Query requests do not survive into
/// the grid, so there is nothing for the attaching terminal to answer.
pub struct TerminalModel {
    parser: Parser,
}

impl TerminalModel {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            // Scrollback stays at 0: the reattach snapshot only needs the
            // visible screen (`contents_formatted`). Per-shell scrollback
            // history is tracked separately in `ShellState::scrollback`.
            parser: Parser::new(rows.max(1), cols.max(1), 0),
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    pub fn set_size(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows.max(1), cols.max(1));
    }

    /// Escape sequences that reproduce the current screen: re-enter the
    /// alternate screen if the inner program is using it, paint the rendered
    /// grid, then restore the input modes (bracketed paste, application
    /// cursor/keypad, mouse) the inner program had enabled. `contents_formatted`
    /// and `input_mode_formatted` never emit query *requests*.
    fn reconstruct(&self) -> Vec<u8> {
        let screen = self.parser.screen();
        let contents = screen.contents_formatted();
        let modes = screen.input_mode_formatted();
        let mut bytes = Vec::with_capacity(8 + contents.len() + modes.len());
        if screen.alternate_screen() {
            // Entering the alternate screen clears it; `contents` then paints it.
            bytes.extend_from_slice(b"\x1b[?1049h");
        }
        bytes.extend_from_slice(&contents);
        bytes.extend_from_slice(&modes);
        bytes
    }

    /// Bytes that repaint a freshly-attached terminal to match the shell's
    /// current screen. Begins with a hard reset so any prior contents on the
    /// attaching terminal are cleared.
    pub fn snapshot_bytes(&self) -> Vec<u8> {
        let reconstructed = self.reconstruct();
        let mut bytes = Vec::with_capacity(RESET_SCREEN.len() + reconstructed.len());
        bytes.extend_from_slice(RESET_SCREEN);
        bytes.extend_from_slice(&reconstructed);
        bytes
    }

    /// Replayable state persisted across daemon hot-upgrades. Re-feeding it into
    /// a fresh parser reconstructs the grid, the alternate-screen flag, and the
    /// input modes.
    pub fn raw_snapshot(&self) -> Vec<u8> {
        self.reconstruct()
    }

    pub fn clear(&mut self) {
        let (rows, cols) = self.parser.screen().size();
        self.parser = Parser::new(rows, cols, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// Regression: a prompt that probes the terminal must not have those probes
    /// replayed on reattach. Replaying them made the reattaching terminal answer
    /// each query, and the answers (`…c`, `…$y`, `>|…`, `rgb:…`) showed up as
    /// garbage on the idle shell. The rendered grid contains no query requests.
    #[test]
    fn snapshot_omits_terminal_query_requests() {
        let mut model = TerminalModel::new(24, 80);
        model.process(b"\x1b[c"); // Primary DA request
        model.process(b"\x1b[6n"); // cursor position report request (DSR)
        model.process(b"\x1b[?2026$p"); // DECRQM: synchronized output
        model.process(b"\x1b[>q"); // XTVERSION
        model.process(b"\x1b]11;?\x07"); // OSC 11 background-color query
        model.process(b"prompt$ "); // visible text

        let snap = model.snapshot_bytes();

        assert!(contains(&snap, b"prompt$"), "visible text must be replayed");
        for query in [
            &b"\x1b[c"[..],
            b"\x1b[6n",
            b"$p",
            b"\x1b[>q",
            b"]11;?",
        ] {
            assert!(
                !contains(&snap, query),
                "snapshot must not replay terminal query {query:x?}; got {snap:x?}"
            );
        }
    }

    /// The old raw-byte replay restored input modes and the alternate screen as
    /// a side effect. The grid replay must preserve that behavior explicitly.
    #[test]
    fn snapshot_restores_input_modes_and_alt_screen() {
        let mut model = TerminalModel::new(24, 80);
        model.process(b"\x1b[?1049h"); // enter alternate screen
        model.process(b"\x1b[?2004h"); // enable bracketed paste
        model.process(b"editing");

        let snap = model.snapshot_bytes();
        assert!(
            contains(&snap, b"\x1b[?1049h"),
            "alternate screen must be restored"
        );
        assert!(
            contains(&snap, b"\x1b[?2004h"),
            "bracketed paste must be restored"
        );
        assert!(contains(&snap, b"editing"));
    }
}
