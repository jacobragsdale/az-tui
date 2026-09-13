//! The label-and-value rows the details pane is made of, and the section
//! rules between them.
//!
//! One place so both tabs' panes read the same: the label in `muted` at a
//! fixed width, the value in whatever colour it has earned.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::theme::theme;
use crate::app::cursor::ScrollState;
use crate::app::screen::Target;
use crate::app::shell::Shell;

/// How wide the label column is. Wide enough for `Content type`, which is
/// the longest label either tab has.
pub const LABEL: usize = 14;

/// How wide the lines inside a pane drawn in `area` may be: the area less
/// its border. What a caller builds its lines to before [`render_pane`].
#[must_use]
pub const fn pane_width(area: Rect) -> u16 {
    area.width.saturating_sub(2)
}

/// How many rows each of `lines` takes once wrapped to `width`: what the
/// scroll and the click regions count in, since the paragraph scrolls in
/// rows and not in lines.
#[must_use]
pub fn visual_rows(lines: &[Line<'static>], width: u16) -> Vec<usize> {
    lines
        .iter()
        .map(|line| {
            Paragraph::new(vec![line.clone()])
                .wrap(Wrap { trim: false })
                .line_count(width)
                .max(1)
        })
        .collect()
}

/// The pane itself: its frame, and `lines` scrolled inside it. Both tabs
/// draw theirs through here, so the pane looks the same whatever it says.
/// Answers with the rows each line took, for a caller placing regions.
pub fn render_pane(
    frame: &mut Frame,
    shell: &mut Shell,
    area: Rect,
    focused: bool,
    scroll: &mut ScrollState,
    lines: Vec<Line<'static>>,
) -> Vec<usize> {
    let palette = theme();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(if focused {
            palette.border_focused
        } else {
            palette.border
        }))
        .title(Line::from(" Details ").style(Style::default().fg(palette.accent)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    shell.region(area, Target::Details);

    let rows = visual_rows(&lines, inner.width);
    scroll.set_viewport(usize::from(inner.height), rows.iter().sum());
    let offset = u16::try_from(scroll.offset).unwrap_or(0);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((offset, 0)),
        inner,
    );
    rows
}

/// One line in `muted`, for a pane with nothing to show yet.
#[must_use]
pub fn quiet(said: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(
        said.into(),
        Style::default().fg(theme().muted),
    ))
}

/// One line in `error`, for a pane whose read was refused.
#[must_use]
pub fn refused(said: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(
        said.into(),
        Style::default().fg(theme().error),
    ))
}

/// `Label   value`, with the value in the colour it was given.
#[must_use]
pub fn field(label: &str, value: impl Into<String>) -> Line<'static> {
    coloured_field(label, value, theme().body)
}

#[must_use]
pub fn coloured_field(label: &str, value: impl Into<String>, colour: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label:<LABEL$}"),
            Style::default().fg(theme().muted),
        ),
        Span::styled(value.into(), Style::default().fg(colour)),
    ])
}

/// A field whose value is a URL.
///
/// Cut to the width rather than wrapped: a portal link is three lines of
/// percent-encoded resource id, and `o` is what anybody actually does with
/// it. The pane is for reading, not for copying by eye.
#[must_use]
pub fn link_field(label: &str, url: impl Into<String>, width: u16) -> Line<'static> {
    let room = usize::from(width).saturating_sub(LABEL);
    coloured_field(label, elide(&url.into(), room), theme().link)
}

/// `text` in at most `room` cells, with an ellipsis where the middle was
/// taken out — a URL's two ends say more than its first half does.
#[must_use]
pub fn elide(text: &str, room: usize) -> String {
    let count = text.chars().count();
    if count <= room || room < 8 {
        return text.chars().take(room.max(1)).collect();
    }
    let keep = room - 1;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let mut out: String = text.chars().take(head).collect();
    out.push('…');
    out.extend(text.chars().skip(count - tail));
    out
}

/// The pane's first line: what this row is.
#[must_use]
pub fn title(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(
        text.into(),
        Style::default()
            .fg(theme().text)
            .add_modifier(Modifier::BOLD),
    ))
}

/// The line under the title: where it lives and what state it is in.
#[must_use]
pub fn subtitle(parts: &[&str]) -> Line<'static> {
    Line::from(Span::styled(
        parts.join(" · "),
        Style::default().fg(theme().muted),
    ))
}

/// `── Versions ────────────`: a heading with a rule running off it.
#[must_use]
pub fn section(label: &str, width: u16) -> Line<'static> {
    let palette = theme();
    let used = label.chars().count() + 4;
    let rule = usize::from(width).saturating_sub(used);
    Line::from(vec![
        Span::styled("── ", Style::default().fg(palette.border)),
        Span::styled(
            label.to_owned(),
            Style::default()
                .fg(palette.header)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {}", "─".repeat(rule)),
            Style::default().fg(palette.border),
        ),
    ])
}

/// One tag as a chip, hashed onto a stable colour so `env=prod` reads the
/// same wherever it appears.
#[must_use]
pub fn chip(key: &str, value: &str) -> Span<'static> {
    let palette = theme();
    let text = if value.is_empty() {
        key.to_owned()
    } else {
        format!("{key}={value}")
    };
    Span::styled(text, Style::default().fg(palette.tag_palette[hash(key)]))
}

/// Which of the six chip colours a tag key lands on. FNV-1a, because it is
/// eight lines and the only requirement is that one key always hashes the
/// same way.
fn hash(key: &str) -> usize {
    let mut held: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.as_bytes() {
        held ^= u64::from(*byte);
        held = held.wrapping_mul(0x1000_0000_01b3);
    }
    (held % 6) as usize
}

/// A key hint at the right-hand end of a line, for the two keys that act on
/// what the line shows.
#[must_use]
pub fn with_hint(line: Line<'static>, hint: &str, width: u16) -> Line<'static> {
    let used: usize = line
        .spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum();
    let room = usize::from(width).saturating_sub(used + hint.chars().count() + 1);
    let mut line = line;
    line.spans.push(Span::raw(" ".repeat(room + 1)));
    line.spans.push(Span::styled(
        hint.to_owned(),
        Style::default().fg(theme().muted),
    ));
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn a_field_lines_its_values_up_in_one_column() {
        assert_eq!(text(&field("Value", "••••••••")), "Value         ••••••••");
        assert_eq!(
            text(&field("Content type", "text/plain")),
            "Content type  text/plain",
            "the longest label still leaves a gap"
        );
    }

    #[test]
    fn a_section_rule_fills_the_pane_and_never_runs_past_it() {
        let line = section("Versions", 30);
        assert_eq!(text(&line).chars().count(), 30);
        assert!(text(&line).starts_with("── Versions ─"));

        let line = section("Versions", 4);
        assert_eq!(text(&line), "── Versions ", "too narrow is still legible");
    }

    #[test]
    fn one_tag_key_always_hashes_to_the_same_colour() {
        let palette = theme();
        let first = chip("env", "prod");
        let again = chip("env", "qa");
        assert_eq!(first.style.fg, again.style.fg);
        assert!(palette.tag_palette.contains(&first.style.fg.unwrap()));
        assert_eq!(first.content.as_ref(), "env=prod");
        assert_eq!(chip("managed", "").content.as_ref(), "managed");
    }

    #[test]
    fn a_link_is_cut_in_the_middle_so_both_its_ends_still_read() {
        let url = "https://portal.azure.com/#@/resource/subscriptions/s/rg/acrdev";
        let line = text(&link_field("Portal", url, 44));
        assert_eq!(line.chars().count(), 44);
        assert!(line.starts_with("Portal        https://portal"), "{line}");
        assert!(line.ends_with("acrdev"), "{line}");
        assert!(line.contains('…'), "{line}");

        let short = text(&link_field("Portal", "https://x/y", 44));
        assert!(!short.contains('…'), "{short}");
        assert_eq!(elide("abcdefghij", 20), "abcdefghij");
        assert_eq!(elide("abcdefghij", 5), "abcde", "too narrow to elide");
    }

    #[test]
    fn a_hint_sits_at_the_right_hand_end_of_its_line() {
        let line = with_hint(field("Value", "••••••••"), "v · y", 40);
        let rendered = text(&line);
        assert_eq!(rendered.chars().count(), 40);
        assert!(rendered.ends_with("v · y"), "{rendered}");
        assert!(rendered.starts_with("Value"), "{rendered}");
    }

    #[test]
    fn a_line_already_too_long_for_its_hint_still_ends_with_it() {
        let line = with_hint(
            field("Id", "https://kv-prod.vault.azure.net/secrets/x"),
            "o",
            20,
        );
        assert!(text(&line).ends_with('o'));
    }

    #[test]
    fn the_title_and_subtitle_read_as_one_thing_in_two_weights() {
        assert_eq!(text(&title("db-password")), "db-password");
        assert_eq!(
            text(&subtitle(&["kv-prod", "secret", "enabled"])),
            "kv-prod · secret · enabled"
        );
        assert_eq!(title("x").spans[0].style.add_modifier, Modifier::BOLD);
    }

    #[test]
    fn a_wrapped_line_counts_its_rows_so_the_tail_can_be_reached() {
        use crate::app::shell::Shell;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let lines: Vec<Line<'static>> = std::iter::once(Line::from("x".repeat(100)))
            .chain((1..7).map(|n| Line::from(format!("line {n}"))))
            .chain(std::iter::once(Line::from("LAST")))
            .collect();
        let mut scroll = ScrollState::default();
        let mut shell = Shell::default();
        let mut terminal = Terminal::new(TestBackend::new(30, 6)).unwrap();
        let mut draw = |scroll: &mut ScrollState| {
            terminal
                .draw(|frame| {
                    shell.begin_frame();
                    render_pane(frame, &mut shell, frame.area(), true, scroll, lines.clone());
                })
                .unwrap();
            crate::ui::screen_text(terminal.backend().buffer())
        };
        draw(&mut scroll);
        assert_eq!(scroll.content, 4 + 7, "the long line is four rows of 28");
        scroll.scroll_to(usize::MAX);
        let drawn = draw(&mut scroll);
        assert!(drawn.contains("LAST"), "{drawn}");
    }
}
