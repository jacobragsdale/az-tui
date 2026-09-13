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
use ratatui::widgets::Paragraph;

use theme::theme;

/// Under this the panes cannot say anything worth reading, so the frame says
/// so instead.
pub const MIN_WIDTH: u16 = 36;
pub const MIN_HEIGHT: u16 = 11;

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

    #[test]
    fn a_small_terminal_says_so_instead_of_painting_a_frame() {
        let mut terminal = Terminal::new(TestBackend::new(20, 5)).unwrap();
        terminal
            .draw(|frame| render_too_small(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let screen: String = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("too small"), "{screen}");
    }
}
