pub struct TerminalEmulator {
    parser: vt100::Parser,
    scroll_offset: usize,
    rows: u16,
    cols: u16,
}

impl TerminalEmulator {
    pub fn new(rows: u16, cols: u16, scrollback_lines: usize) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, scrollback_lines),
            scroll_offset: 0,
            rows,
            cols,
        }
    }

    pub fn process(&mut self, data: &[u8]) {
        self.parser.process(data);
        // Sticky-bottom: only stay pinned to live output when the user is
        // already at the bottom. Scrolling up leaves scroll_offset alone so
        // streaming boards / keepalives don't yank the view back down.
    }

    /// Current cursor position as (row, col), 0-based. Used by the ANSI
    /// query responder to answer `ESC[6n` (CPR).
    pub fn cursor_position(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if rows > 0 && cols > 0 && (rows != self.rows || cols != self.cols) {
            self.rows = rows;
            self.cols = cols;
            self.parser.screen_mut().set_size(rows, cols);
        }
    }

    /// Apply the user's scrollback offset and return a guard that resets it
    /// on drop. Render through `guard.screen()` — when the guard goes out of
    /// scope the live view is restored, so any incoming network data lands
    /// at the right place.
    pub fn scroll_view(&mut self) -> ScrollGuard<'_> {
        self.parser.screen_mut().set_scrollback(self.scroll_offset);
        self.scroll_offset = self.parser.screen().scrollback();
        ScrollGuard { emulator: self }
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_offset += lines;
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }
}

pub struct ScrollGuard<'a> {
    emulator: &'a mut TerminalEmulator,
}

impl<'a> ScrollGuard<'a> {
    pub fn screen(&self) -> &vt100::Screen {
        self.emulator.parser.screen()
    }
}

impl Drop for ScrollGuard<'_> {
    fn drop(&mut self) {
        self.emulator.parser.screen_mut().set_scrollback(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vt_scrollback(e: &TerminalEmulator) -> usize {
        e.parser.screen().scrollback()
    }

    #[test]
    fn scroll_view_guard_resets_vt_scrollback_on_drop() {
        let mut e = TerminalEmulator::new(24, 80, 1000);
        // Push some scrollback content so the offset isn't clamped to 0.
        for _ in 0..50 {
            e.process(b"line\r\n");
        }
        e.scroll_up(5);
        {
            let _guard = e.scroll_view();
            // While the guard is alive, vt100 scrollback should be non-zero
            // (the renderer reads cells from scrollback through it).
        }
        assert_eq!(
            vt_scrollback(&e),
            0,
            "vt scrollback must reset to 0 when guard drops",
        );
    }

    #[test]
    fn scroll_view_guard_exposes_screen() {
        // The whole point of the guard is to render off scrolled-back cells.
        let mut e = TerminalEmulator::new(24, 80, 1000);
        e.process(b"hello");
        let guard = e.scroll_view();
        // Cursor should be at column 5 row 0 after "hello".
        let screen = guard.screen();
        let (row, col) = screen.cursor_position();
        assert_eq!((row, col), (0, 5));
    }

    #[test]
    fn process_does_not_reset_scroll_when_scrolled_up() {
        let mut e = TerminalEmulator::new(24, 80, 1000);
        for _ in 0..50 {
            e.process(b"line\r\n");
        }
        e.scroll_up(10);
        let offset = e.scroll_offset();
        assert!(offset > 0);
        e.process(b"more data\r\n");
        assert_eq!(
            e.scroll_offset(),
            offset,
            "inbound data must not snap scrollback to the bottom"
        );
    }

    #[test]
    fn process_keeps_offset_zero_when_at_bottom() {
        let mut e = TerminalEmulator::new(24, 80, 1000);
        e.process(b"hello\r\n");
        assert_eq!(e.scroll_offset(), 0);
        e.process(b"world\r\n");
        assert_eq!(e.scroll_offset(), 0);
    }

    #[test]
    fn resize_zero_dimensions_are_ignored() {
        let mut e = TerminalEmulator::new(24, 80, 100);
        e.resize(0, 80);
        e.resize(24, 0);
        e.resize(0, 0);
        // Still the original size — process should not panic.
        e.process(b"x");
        assert_eq!(e.cursor_position(), (0, 1));
    }

    #[test]
    fn resize_same_size_is_noop_different_size_applies() {
        let mut e = TerminalEmulator::new(24, 80, 100);
        e.resize(24, 80); // same
        e.process(b"\x1b[1;1Hab");
        assert_eq!(e.cursor_position(), (0, 2));
        e.resize(12, 40);
        // After resize the parser accepts the new geometry; writing still works.
        e.process(b"z");
        let (row, col) = e.cursor_position();
        assert!(row < 12, "row {row} should be within new height");
        assert!(col < 40, "col {col} should be within new width");
    }

    #[test]
    fn scroll_up_overshoot_clamps_via_scroll_view() {
        let mut e = TerminalEmulator::new(5, 40, 20);
        for _ in 0..10 {
            e.process(b"line\r\n");
        }
        e.scroll_up(10_000);
        {
            let _g = e.scroll_view();
        }
        // scroll_view reads vt100's clamped scrollback and stores it back.
        assert!(
            e.scroll_offset() <= 20,
            "offset {} must clamp to available scrollback",
            e.scroll_offset()
        );
    }

    #[test]
    fn size_probe_moves_cursor_before_cpr_sample() {
        // Common BBS pattern: CUP to far corner then CPR.
        let mut e = TerminalEmulator::new(24, 80, 100);
        e.process(b"\x1b[999;999H");
        let (row, col) = e.cursor_position();
        // vt100 clamps to bottom-right (23, 79) 0-based.
        assert_eq!((row, col), (23, 79));
    }
}
