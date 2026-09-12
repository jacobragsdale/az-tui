//! Every frame: the guard for a terminal too small to say anything in, and
//! the panes on top of it.

pub mod details;
pub mod registries;
pub mod secrets;
pub mod table;
pub mod theme;
pub mod widgets;

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use theme::theme;

use crate::store::Store;
use crate::timestamp::Timestamp;

/// Under this the panes cannot say anything worth reading, so the frame says
/// so instead.
pub const MIN_WIDTH: u16 = 36;
pub const MIN_HEIGHT: u16 = 11;

pub fn render(frame: &mut Frame, store: &Store) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        render_too_small(frame, area);
        return;
    }
    let palette = theme();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(palette.border))
        .title(Line::from(" az-tui ").style(Style::default().fg(palette.accent)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = vec![Line::from(counts(store)).style(Style::default().fg(palette.body))];
    if let Some(said) = &store.progress {
        lines.push(Line::from(said.clone()).style(Style::default().fg(palette.muted)));
    }
    if let Some((who, message)) = store.first_problem() {
        let said = if who.is_empty() {
            message.clone()
        } else {
            format!("{who}: {message}")
        };
        lines.push(Line::from(said).style(Style::default().fg(palette.error)));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// What the placeholder screen says until step 06 gives it tables: enough to
/// watch the worker work.
fn counts(store: &Store) -> String {
    if store.is_empty() && store.read_at.is_none() && !store.refreshing {
        return "Nothing read yet — press r".to_owned();
    }
    let age = store.read_at.map_or_else(
        || "never read".to_owned(),
        |read_at| match read_at.relative_age(Timestamp::now()).as_str() {
            "now" => "just now".to_owned(),
            age => format!("read {age} ago"),
        },
    );
    format!(
        "{} vaults · {} secrets · {} registries · {} repositories · {age}",
        store.inventory.vaults.len(),
        store.secrets.len(),
        store.inventory.registries.len(),
        store.repositories.len(),
    )
}

pub fn render_too_small(frame: &mut Frame, area: Rect) {
    let palette = theme();
    frame.render_widget(
        Paragraph::new(format!(
            "Terminal too small\nneeds {MIN_WIDTH} × {MIN_HEIGHT}"
        ))
        .alignment(Alignment::Center)
        .style(Style::default().fg(palette.error)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn drawn(width: u16, height: u16) -> String {
        drawn_with(width, height, &Store::default())
    }

    fn drawn_with(width: u16, height: u16, store: &Store) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, store)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_empty_screen_says_what_to_press() {
        let screen = drawn(60, 12);
        assert!(screen.contains("az-tui"), "{screen}");
        assert!(screen.contains("Nothing read yet"), "{screen}");
    }

    #[test]
    fn the_counts_say_what_the_worker_has_read() {
        let mut store = Store::default();
        store.apply(crate::worker::Event::Inventory(Ok(
            crate::azure::Inventory::default(),
        )));
        store.apply(crate::worker::Event::Progress("reading kv-a (1/2)…".into()));
        let screen = drawn_with(80, 12, &store);
        assert!(screen.contains("0 vaults · 0 secrets"), "{screen}");
        assert!(screen.contains("reading kv-a (1/2)"), "{screen}");
    }

    #[test]
    fn a_problem_is_painted_under_the_counts() {
        let mut store = Store::default();
        store.apply(crate::worker::Event::Inventory(Err(
            "not signed in — run `az login`".into(),
        )));
        let screen = drawn_with(80, 12, &store);
        assert!(screen.contains("not signed in"), "{screen}");
    }

    #[test]
    fn a_small_terminal_says_so_instead_of_painting_a_frame() {
        let screen = drawn(20, 5);
        assert!(screen.contains("too small"), "{screen}");
        assert!(!screen.contains("Nothing read"), "{screen}");
    }
}
