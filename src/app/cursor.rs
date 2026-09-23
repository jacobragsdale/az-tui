//! Where a list's cursor is and how far the list is scrolled.
//!
//! Every table and every pane keeps one, so moving a cursor and keeping it on
//! screen is written once. Lifted from ticket-tui, with its `ScrollState`
//! brought along rather than a whole `pointer.rs`.

/// How much of a list is on screen, and which part.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScrollState {
    pub offset: usize,
    pub content: usize,
    pub viewport: usize,
}

impl ScrollState {
    #[must_use]
    pub const fn max_offset(self) -> usize {
        self.content.saturating_sub(self.viewport)
    }

    /// Records the rendered geometry and re-clamps the offset to the new
    /// maximum.
    pub const fn set_viewport(&mut self, viewport: usize, content: usize) {
        self.viewport = viewport;
        self.content = content;
        self.clamp();
    }

    pub const fn scroll_to(&mut self, offset: usize) {
        self.offset = offset;
        self.clamp();
    }

    /// Scrolls by `delta` rows, clamped to the content, and reports whether
    /// the offset moved.
    pub const fn scroll_by(&mut self, delta: i32) -> bool {
        let before = self.offset;
        self.offset = if delta < 0 {
            self.offset.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.offset.saturating_add(delta as usize)
        };
        self.clamp();
        self.offset != before
    }

    /// Scrolls the smallest amount that brings `index` inside the viewport.
    pub const fn ensure_visible(&mut self, index: usize) {
        let viewport = if self.viewport == 0 { 1 } else { self.viewport };
        if index < self.offset {
            self.offset = index;
        } else if index >= self.offset.saturating_add(viewport) {
            self.offset = index.saturating_add(1).saturating_sub(viewport);
        }
    }

    /// One screenful less a row of overlap, as `PageUp` and `PageDown` move.
    #[must_use]
    pub const fn page_step(self) -> usize {
        let step = self.viewport.saturating_sub(1);
        if step == 0 { 1 } else { step }
    }

    const fn clamp(&mut self) {
        let maximum = self.max_offset();
        if self.offset > maximum {
            self.offset = maximum;
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ListCursor {
    /// Which row the cursor is on, counted over whatever the list is showing
    /// now — a filtered table counts the rows its query left.
    pub index: usize,
    pub scroll: ScrollState,
}

impl ListCursor {
    /// Puts the cursor on one row and scrolls it into view.
    pub const fn focus(&mut self, index: usize) {
        self.index = index;
        self.scroll.ensure_visible(index);
    }

    /// Moves the cursor by `delta` rows, stopping at either end of a list of
    /// `count` rows. An empty list puts it back at the top.
    pub const fn move_by(&mut self, delta: isize, count: usize) {
        if count == 0 {
            self.index = 0;
            self.scroll.scroll_to(0);
            return;
        }
        let index = self.index.saturating_add_signed(delta);
        self.focus(if index > count - 1 { count - 1 } else { index });
    }

    /// The same, a screenful at a time.
    pub const fn page(&mut self, direction: isize, count: usize) {
        let step = self.scroll.page_step() as isize;
        self.move_by(direction * step, count);
    }

    /// Back to the first row, as reopening a list does.
    pub const fn reset(&mut self) {
        self.index = 0;
        self.scroll.scroll_to(0);
    }

    /// The wheel over a list of `count` rows: the viewport moves and the
    /// cursor follows it rather than being left behind, so what a key acts
    /// on is always something on screen. Says whether the cursor moved.
    pub fn wheel(&mut self, delta: i32, count: usize) -> bool {
        let before = self.index;
        // The scroll state is from the last draw, which a refresh may have
        // shortened the list under since; measured again here so the window
        // below cannot come out inside out.
        self.scroll.set_viewport(self.scroll.viewport, count);
        self.scroll.scroll_by(delta);
        let last = (self.scroll.offset + self.scroll.viewport.saturating_sub(1))
            .min(count.saturating_sub(1));
        let first = self.scroll.offset.min(last);
        self.index = self.index.clamp(first, last);
        self.index != before
    }

    /// Re-clamps the cursor after the list under it has changed length.
    pub const fn clamp(&mut self, count: usize) {
        if count == 0 {
            self.reset();
        } else if self.index > count - 1 {
            self.focus(count - 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(viewport: usize) -> ListCursor {
        let mut cursor = ListCursor::default();
        cursor.scroll.viewport = viewport;
        cursor.scroll.content = 20;
        cursor
    }

    #[test]
    fn a_cursor_stops_at_both_ends_and_scrolls_itself_into_view() {
        let mut list = cursor(5);
        list.move_by(-1, 20);
        assert_eq!((list.index, list.scroll.offset), (0, 0), "the top holds");

        list.move_by(7, 20);
        assert_eq!(list.index, 7);
        assert_eq!(list.scroll.offset, 3, "the row is the last one on screen");

        list.move_by(50, 20);
        assert_eq!(list.index, 19, "and the bottom holds");

        list.page(-1, 20);
        assert_eq!(
            list.index, 15,
            "a page is a screenful less the row of overlap"
        );
        list.reset();
        assert_eq!((list.index, list.scroll.offset), (0, 0));
    }

    #[test]
    fn a_shorter_list_pulls_the_cursor_back_onto_it() {
        let mut list = cursor(5);
        list.move_by(9, 20);
        assert_eq!(list.index, 9);

        list.clamp(4);
        assert_eq!(list.index, 3, "the last row of the shorter list");

        list.clamp(0);
        assert_eq!((list.index, list.scroll.offset), (0, 0), "and none of it");
    }

    #[test]
    fn the_wheel_drags_the_cursor_along_with_the_viewport() {
        let mut list = cursor(5);
        assert!(
            list.wheel(3, 20),
            "row 0 scrolled off, so the cursor follows"
        );
        assert_eq!((list.index, list.scroll.offset), (3, 3));
        list.focus(5);
        assert!(!list.wheel(-1, 20), "row 5 is still on screen");
        assert_eq!((list.index, list.scroll.offset), (5, 2));
        assert!(list.wheel(1, 4), "a list shortened since the last draw");
        assert_eq!((list.index, list.scroll.offset), (3, 0));
    }

    #[test]
    fn a_viewport_that_shrinks_pulls_the_offset_back_with_it() {
        let mut scroll = ScrollState {
            offset: 10,
            content: 20,
            viewport: 5,
        };
        scroll.set_viewport(5, 12);
        assert_eq!(scroll.offset, 7, "the last screenful of the shorter list");
        assert!(scroll.scroll_by(-3));
        assert_eq!(scroll.offset, 4);
        assert!(scroll.scroll_by(-10), "it moves as far as it can");
        assert_eq!(scroll.offset, 0);
        assert!(!scroll.scroll_by(-1), "and then says it did not move");
    }
}
