//! The questions asked on top of the table: restart this pod? scale this
//! deployment to how many?
//!
//! A modal takes the whole pointer: a click anywhere but on it closes it, and
//! nothing underneath — least of all the toolbar button that opened it — can
//! be reached through it.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::theme::theme;
use super::widgets::render_modal_frame;
use crate::app::scope::Modal;
use crate::app::screen::Target;
use crate::app::shell::Shell;

pub fn render_modal(frame: &mut Frame, shell: &mut Shell, modal: &Modal, area: Rect) {
    let palette = theme();
    // Drawn first, so the modal's own regions sit on top of it.
    shell.region(area, Target::Dismiss);
    let (title, body, yes, hint) = match modal {
        Modal::Confirm {
            title, body, verb, ..
        } => (
            title.as_str(),
            body.iter()
                .map(|line| Line::from(line.clone()))
                .collect::<Vec<_>>(),
            verb.button(),
            format!("{} · Esc to leave it", verb.hint()),
        ),
        Modal::Scale {
            object,
            input,
            current,
        } => (
            "Scale",
            vec![
                Line::from(format!("{} {}", object.kind, object.name)),
                Line::from(""),
                Line::from(vec![
                    Span::styled("Replicas  ", Style::default().fg(palette.muted)),
                    Span::styled(
                        {
                            let (head, tail) = input.split_at_cursor();
                            format!("[ {head}\u{258f}{tail}]")
                        },
                        Style::default()
                            .fg(palette.text)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        current.map_or_else(String::new, |held| {
                            format!("   now {} desired · {} ready", held.desired, held.ready)
                        }),
                        Style::default().fg(palette.muted),
                    ),
                ]),
            ],
            "Scale",
            "Enter to scale · Esc to leave it".to_owned(),
        ),
    };
    let height = u16::try_from(body.len())
        .unwrap_or(u16::MAX)
        .saturating_add(5);
    let inner = render_modal_frame(frame, area, title, 60, height);
    shell.region(inner, Target::Modal);
    let [text, buttons, keys] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), text);

    let yes_label = format!(" {yes} ");
    let yes_width = u16::try_from(yes_label.chars().count()).unwrap_or(9);
    let yes_area = Rect::new(buttons.x, buttons.y, yes_width.min(buttons.width), 1);
    frame.render_widget(
        Paragraph::new(Span::styled(
            yes_label,
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        )),
        yes_area,
    );
    shell.region(yes_area, Target::Confirm);
    let no_area = Rect::new(
        buttons.x.saturating_add(yes_width + 1),
        buttons.y,
        10.min(buttons.width.saturating_sub(yes_width + 1)),
        1,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            " Leave it ",
            Style::default()
                .fg(palette.text)
                .bg(palette.selected_background),
        )),
        no_area,
    );
    shell.region(no_area, Target::Dismiss);
    frame.render_widget(
        Paragraph::new(Span::styled(hint, Style::default().fg(palette.muted))),
        keys,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube::ObjectRef;
    use crate::text_input::TextInput;
    use crate::ui::screen_text;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn the_scale_field_draws_the_caret_where_it_is() {
        let mut input = TextInput::new("12");
        input.move_left();
        input.insert_char('3');
        assert_eq!(input.text(), "132");
        let modal = Modal::Scale {
            object: ObjectRef {
                kind: "deployment".into(),
                namespace: "dev".into(),
                name: "orders".into(),
            },
            input,
            current: None,
        };
        let mut shell = Shell::default();
        let mut terminal = Terminal::new(TestBackend::new(70, 12)).unwrap();
        terminal
            .draw(|frame| {
                shell.begin_frame();
                render_modal(frame, &mut shell, &modal, frame.area());
            })
            .unwrap();
        let drawn = screen_text(terminal.backend().buffer());
        assert!(drawn.contains("[ 13\u{258f}2]"), "{drawn}");
    }
}
