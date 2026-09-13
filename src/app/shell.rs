//! What every screen shares and no screen owns: where focus is, what the
//! status bar is saying, where the panes are, and what a click just landed
//! on.

use std::time::{Duration, Instant};

use ratatui::layout::Rect;

use super::screen::Target;

/// How long a notification stays in the status bar before the footer hint
/// comes back.
const NOTIFICATION: Duration = Duration::from_secs(4);
/// Below this the details pane is hidden until `Tab` asks for it.
const DETAILS_HIDDEN_BELOW: u16 = 70;
/// At or above this the details pane sits beside the table rather than under
/// it.
const SIDE_BY_SIDE_AT: u16 = 110;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Focus {
    #[default]
    Table,
    /// The details pane, or the text pane under it when that is open.
    Details,
    Search,
    /// The filter inside the text pane.
    PaneSearch,
}

/// Which drop-down is open: the Env header's on an Azure tab, the kind
/// pill's on a scope tab. Only one table shows, so only one can be.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Menu {
    Env,
    Kind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Level {
    Info,
    Error,
}

/// How the two panes are laid out at this width.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Panes {
    /// Table left, details right.
    SideBySide,
    /// Table above, details below.
    Stacked,
    /// Only one fits; `Tab` swaps which.
    One,
}

#[derive(Default)]
pub struct Shell {
    pub focus: Focus,
    pub help_open: bool,
    /// The open menu and which line of it the keys are on. One per app
    /// rather than per screen: only one table shows.
    pub menu: Option<(Menu, usize)>,
    /// What the status bar says instead of the footer hint, until it expires.
    notification: Option<(String, Instant, Level)>,
    /// Rebuilt every frame; a click resolves against the last region that
    /// contains the point.
    regions: Vec<(Rect, Target)>,
}

impl Shell {
    /// Says something in the status bar for a few seconds.
    pub fn set_status(&mut self, said: impl Into<String>) {
        self.notification = Some((said.into(), Instant::now(), Level::Info));
    }

    pub fn set_error(&mut self, said: impl Into<String>) {
        self.notification = Some((said.into(), Instant::now(), Level::Error));
    }

    /// What the status bar should say now, if anything. Expired
    /// notifications are dropped as they are read rather than on a timer.
    pub fn notification(&mut self) -> Option<(&str, Level)> {
        if self
            .notification
            .as_ref()
            .is_some_and(|(_, at, _)| at.elapsed() >= NOTIFICATION)
        {
            self.notification = None;
        }
        self.notification
            .as_ref()
            .map(|(said, _, level)| (said.as_str(), *level))
    }

    /// Starts a frame: whatever was clickable last frame is gone.
    pub fn begin_frame(&mut self) {
        self.regions.clear();
    }

    /// Registers something a click can land on. Called in drawing order, so
    /// later calls sit on top.
    pub fn region(&mut self, area: Rect, target: Target) {
        self.regions.push((area, target));
    }

    /// What is under this point, or nothing. The last match wins: what was
    /// drawn last is what the pointer is over.
    #[must_use]
    pub fn hit(&self, column: u16, row: u16) -> Option<&Target> {
        self.regions
            .iter()
            .rev()
            .find(|(area, _)| area.contains((column, row).into()))
            .map(|(_, target)| target)
    }

    /// Where something was drawn this frame, for a menu that opens under it.
    #[must_use]
    pub fn find(&self, target: &Target) -> Option<Rect> {
        self.regions
            .iter()
            .find(|(_, held)| held == target)
            .map(|(area, _)| *area)
    }

    /// How the panes sit at this width.
    ///
    // ponytail: a fixed 55/45 split with no drag. A draggable divider wants
    // pointer capture, a stored ratio in the session and a hit region a
    // character wide; nobody has asked, and the two useful widths are
    // "both" and "one".
    #[must_use]
    pub const fn panes(width: u16) -> Panes {
        if width >= SIDE_BY_SIDE_AT {
            Panes::SideBySide
        } else if width >= DETAILS_HIDDEN_BELOW {
            Panes::Stacked
        } else {
            Panes::One
        }
    }

    /// `Tab`: between the table and the details pane, and out of the search
    /// box either way.
    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Table => Focus::Details,
            Focus::Details | Focus::Search | Focus::PaneSearch => Focus::Table,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_region_drawn_is_the_one_a_click_lands_on() {
        let mut shell = Shell::default();
        shell.begin_frame();
        shell.region(Rect::new(0, 0, 40, 10), Target::Details);
        shell.region(Rect::new(5, 2, 10, 2), Target::Row(3));
        assert_eq!(shell.hit(6, 3), Some(&Target::Row(3)));
        assert_eq!(shell.hit(30, 8), Some(&Target::Details));
        assert_eq!(shell.hit(90, 3), None);

        shell.begin_frame();
        assert_eq!(shell.hit(6, 3), None, "a new frame clears the old regions");
    }

    #[test]
    fn the_panes_follow_the_width() {
        assert_eq!(Shell::panes(120), Panes::SideBySide);
        assert_eq!(Shell::panes(110), Panes::SideBySide);
        assert_eq!(Shell::panes(109), Panes::Stacked);
        assert_eq!(Shell::panes(70), Panes::Stacked);
        assert_eq!(Shell::panes(69), Panes::One);
    }

    #[test]
    fn a_notification_is_read_back_with_its_level() {
        let mut shell = Shell::default();
        assert!(shell.notification().is_none());
        shell.set_status("Copied value of db-password (kv-prod)");
        let (said, level) = shell.notification().unwrap();
        assert!(said.contains("db-password"));
        assert_eq!(level, Level::Info);

        shell.set_error("kv-prod: no permission");
        assert_eq!(shell.notification().unwrap().1, Level::Error);
    }

    #[test]
    fn tab_moves_between_the_panes_and_always_leaves_the_search_box() {
        let mut shell = Shell::default();
        shell.toggle_focus();
        assert_eq!(shell.focus, Focus::Details);
        shell.toggle_focus();
        assert_eq!(shell.focus, Focus::Table);
        shell.focus = Focus::Search;
        shell.toggle_focus();
        assert_eq!(shell.focus, Focus::Table);
    }
}
