/// Number of off-screen rows kept in the scrollback buffer per session.
/// Trade-off: more rows = more useful history at the cost of memory.
const SCROLLBACK_LINES: usize = 1000;

pub struct TerminalEmulator {
    parser: vt100::Parser,
    scroll_offset: usize,
    rows: u16,
    cols: u16,
}

impl TerminalEmulator {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, SCROLLBACK_LINES),
            scroll_offset: 0,
            rows,
            cols,
        }
    }

    pub fn process(&mut self, data: &[u8]) {
        self.parser.process(data);
        // Auto-scroll to bottom on new data
        self.scroll_offset = 0;
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
        let mut e = TerminalEmulator::new(24, 80);
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
            vt_scrollback(&e), 0,
            "vt scrollback must reset to 0 when guard drops",
        );
    }

    #[test]
    fn scroll_view_guard_exposes_screen() {
        // The whole point of the guard is to render off scrolled-back cells.
        let mut e = TerminalEmulator::new(24, 80);
        e.process(b"hello");
        let guard = e.scroll_view();
        // Cursor should be at column 5 row 0 after "hello".
        let screen = guard.screen();
        let (row, col) = screen.cursor_position();
        assert_eq!((row, col), (0, 5));
    }
}
