//! Every frame: the guard for a terminal too small to say anything in, and
//! the panes on top of it.

pub mod theme;

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use theme::theme;

/// Under this the panes cannot say anything worth reading, so the frame says
/// so instead.
pub const MIN_WIDTH: u16 = 36;
pub const MIN_HEIGHT: u16 = 11;

pub fn render(frame: &mut Frame) {
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
    frame.render_widget(
        Paragraph::new("Nothing read yet — press r").style(Style::default().fg(palette.muted)),
        inner,
    );
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
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(render).unwrap();
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
    fn a_small_terminal_says_so_instead_of_painting_a_frame() {
        let screen = drawn(20, 5);
        assert!(screen.contains("too small"), "{screen}");
        assert!(!screen.contains("Nothing read"), "{screen}");
    }
}
