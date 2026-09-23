//! The text pane under the details: a pod's log, tailed; what describe or
//! `get -o yaml` said; or one key of a configmap or a secret.

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};

use super::theme::theme;
use super::widgets::render_scrollbar;
use crate::app::scope::{PaneText, ScopeScreen};
use crate::app::screen::Target;
use crate::app::shell::{Focus, Shell};
use crate::kube::{Kind, TextKind};
use crate::store::ScopeData;

/// The pane. Following keeps the tail in view; scrolling up by any means
/// leaves it, and `End` goes back.
pub fn render_text_pane(
    frame: &mut Frame,
    shell: &mut Shell,
    screen: &mut ScopeScreen,
    data: &ScopeData,
    area: Rect,
) {
    let palette = theme();
    let focused = matches!(shell.focus, Focus::Details | Focus::PaneSearch);
    screen.sync_value(data);
    let key = screen.shown_key(data);
    let (title, lines, empty, refused) = pane_content(screen, data);

    // The filter narrows what is painted, never what is held. Indices rather
    // than references, so the scroll state can move while they are held.
    let filter = screen.pane_filter.text().to_owned();
    let (head, tail) = screen.pane_filter.split_at_cursor();
    let (head, tail) = (head.to_owned(), tail.to_owned());
    let total = lines.len();
    let fresh = (screen.shown_for.as_ref() != Some(&key)).then(|| {
        if filter.is_empty() {
            (0..total).collect()
        } else {
            let words: Vec<String> = filter.split_whitespace().map(str::to_owned).collect();
            let query = crate::search::Query::new(&words);
            (0..total).filter(|at| query.matches(&lines[*at])).collect()
        }
    });
    if let Some(fresh) = fresh {
        screen.shown = fresh;
        screen.shown_for = Some(key);
    }
    let shown = &screen.shown;

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(if focused {
            palette.border_focused
        } else {
            palette.border
        }))
        .padding(Padding::horizontal(1))
        .title(Line::from(title).style(Style::default().fg(palette.accent)));
    if shell.focus == Focus::PaneSearch || !filter.is_empty() {
        let caret = if shell.focus == Focus::PaneSearch {
            "\u{258f}"
        } else {
            ""
        };
        block = block.title_bottom(
            Line::from(format!(" / {head}{caret}{tail} · {}/{total} ", shown.len())).style(
                Style::default().fg(if shell.focus == Focus::PaneSearch {
                    palette.accent
                } else {
                    palette.muted
                }),
            ),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    shell.region(area, Target::TextPane);

    if shown.is_empty() {
        let empty = if total > 0 {
            "No line matches the filter".to_owned()
        } else {
            empty
        };
        frame.render_widget(
            Paragraph::new(empty).style(Style::default().fg(if refused {
                palette.error
            } else {
                palette.muted
            })),
            inner,
        );
        return;
    }
    let viewport = usize::from(inner.height).max(1);
    screen.pane_scroll.set_viewport(viewport, shown.len());
    if screen.pane == PaneText::Log && screen.log_following() {
        screen
            .pane_scroll
            .scroll_to(shown.len().saturating_sub(viewport));
    }
    let offset = screen.pane_scroll.offset;
    // Only the lines on screen are painted: the buffer runs to twenty thousand.
    let (_, lines, _, _) = pane_content(screen, data);
    let painted: Vec<Line<'static>> = shown
        .iter()
        .skip(offset)
        .take(viewport)
        .map(|at| &lines[*at])
        .map(|line| match (screen.pane, refused) {
            (PaneText::Log, _) => log_line(line),
            (_, true) => Line::styled(line.clone(), Style::default().fg(palette.error)),
            (_, false) => Line::raw(line.clone()),
        })
        .collect();
    frame.render_widget(Paragraph::new(painted), inner);
    render_scrollbar(
        frame,
        Rect::new(area.right().saturating_sub(1), inner.y, 1, inner.height),
        offset,
        viewport,
        shown.len(),
    );
}

/// What the pane shows: its title, its lines, what it says when there are
/// none, and whether the lines are a refusal.
fn pane_content<'a>(
    screen: &'a ScopeScreen,
    data: &ScopeData,
) -> (String, &'a [String], String, bool) {
    static NONE: [String; 0] = [];
    let object = screen.selected_object(data);
    let name = object
        .as_ref()
        .map_or("nothing chosen", |object| object.name.as_str());
    match screen.pane {
        PaneText::Log => {
            let Some(target) = screen.following() else {
                return (
                    " Log ".to_owned(),
                    &NONE,
                    "Opening the log\u{2026}".to_owned(),
                    false,
                );
            };
            let pod = screen.selected_pod(data);
            let container = target
                .container
                .clone()
                .or_else(|| pod.and_then(|pod| pod.first_container().map(str::to_owned)))
                .unwrap_or_else(|| "\u{2014}".to_owned());
            // A stream that has ended has nothing left to wait for and says so
            // plainly; a quiet pod sends nothing to repaint on, so no spinner.
            let state = if screen.log_ended() {
                "ended"
            } else if screen.log_following() {
                "following"
            } else {
                "scrolled"
            };
            let title = format!(
                " Log \u{00b7} {state} \u{00b7} {} \u{00b7} {container}{} \u{00b7} {} lines ",
                target.key.name,
                if target.previous { " (previous)" } else { "" },
                screen.log_lines().len(),
            );
            (title, screen.log_lines(), "No log yet".to_owned(), false)
        }
        PaneText::Describe | PaneText::Yaml => {
            let kind = if screen.pane == PaneText::Describe {
                TextKind::Describe
            } else {
                TextKind::Yaml
            };
            let word = match kind {
                TextKind::Describe => "Describe",
                TextKind::Yaml => "YAML",
            };
            let title = format!(" {word} \u{00b7} {name} ");
            let Some(object) = object else {
                return (title, &NONE, "Nothing chosen".to_owned(), false);
            };
            match screen.text(kind, &object) {
                Some(Ok(lines)) => (title, lines, "Nothing came back".to_owned(), false),
                Some(Err(message)) => (title, &NONE, message.clone(), true),
                None if screen.text_pending(kind, &object) => {
                    (title, &NONE, format!("{word}\u{2026}"), false)
                }
                None => (
                    title,
                    &NONE,
                    match kind {
                        TextKind::Describe => "d describes it".to_owned(),
                        TextKind::Yaml => "v shows its YAML".to_owned(),
                    },
                    false,
                ),
            }
        }
        PaneText::Value => {
            let key = screen.selected_key(data).unwrap_or_default();
            let mut title = format!(" Value \u{00b7} {name} \u{00b7} {key} ");
            let (empty, refused) = match screen.kind {
                Kind::Secrets => {
                    if let Some(held) = screen.revealed_here(data) {
                        title = format!(
                            " Value \u{00b7} {name} \u{00b7} {key} \u{00b7} clears in {}s ",
                            held.clears_in(Instant::now())
                        );
                        ("(empty)".to_owned(), false)
                    } else if let Some(refusal) = screen.refusal() {
                        (refusal.clone(), true)
                    } else if screen.reading_here(data) {
                        ("Reading\u{2026}".to_owned(), false)
                    } else {
                        (
                            "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}   \
                             Enter or v reveals it for 60 s \u{00b7} y copies it unseen"
                                .to_owned(),
                            false,
                        )
                    }
                }
                _ => ("(empty)".to_owned(), false),
            };
            (title, screen.pane_value(), empty, refused)
        }
    }
}

/// One log line, with the timestamp `--timestamps` puts in front dimmed and
/// the line painted by what its own words say about it.
// ponytail: token heuristic, no log-format parsing.
fn log_line(raw: &str) -> Line<'static> {
    let palette = theme();
    let (stamp, rest) = split_timestamp(raw);
    let mut spans = Vec::new();
    if let Some(stamp) = stamp {
        spans.push(Span::styled(stamp, Style::default().fg(palette.muted)));
    }
    spans.push(Span::styled(rest.to_owned(), severity_style(rest)));
    Line::from(spans)
}

/// `2026-09-12T12:00:01.123456789Z ` off the front, when it is there:
/// shortened to the clock, since the date is the same for every line worth
/// reading together.
fn split_timestamp(raw: &str) -> (Option<String>, &str) {
    let Some((stamp, rest)) = raw.split_once(' ') else {
        return (None, raw);
    };
    let looks_like_rfc3339 = stamp.len() >= 20
        && stamp.as_bytes()[4] == b'-'
        && stamp.as_bytes()[10] == b'T'
        && stamp.ends_with('Z');
    if !looks_like_rfc3339 {
        return (None, raw);
    }
    let clock: String = stamp.chars().skip(11).take(8).collect();
    (Some(format!("{clock} ")), rest)
}

fn severity_style(line: &str) -> Style {
    let palette = theme();
    if line.contains("ERROR")
        || line.contains("FATAL")
        || line.contains("level=error")
        || line.contains("\"level\":\"error\"")
        || line.starts_with('\u{2026}')
    {
        Style::default().fg(palette.error)
    } else if line.contains("WARN") || line.contains("level=warn") {
        Style::default().fg(palette.warning)
    } else if line.contains("panic") {
        Style::default()
            .fg(palette.error)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::screen_text;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// A pod's log pane with two lines in it.
    fn log_pane() -> (ScopeScreen, ScopeData) {
        let mut data = ScopeData::default();
        data.pods.rows = vec![crate::kube::tests::pod(
            "qa",
            "dev",
            "orders-api-7d9f5b-abc12",
            "Running",
        )];
        let mut screen = ScopeScreen::new(false);
        screen.refilter(&data);
        screen.toggle_log();
        let target = screen.log_target(0, &data).unwrap();
        screen.begin_follow(Some(target.clone()));
        screen.append_log(&target, vec!["starting".into(), "listening".into()], false);
        (screen, data)
    }

    fn draw(screen: &mut ScopeScreen, data: &ScopeData) -> String {
        let mut shell = Shell::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
        terminal
            .draw(|frame| {
                shell.begin_frame();
                render_text_pane(frame, &mut shell, screen, data, frame.area());
            })
            .unwrap();
        screen_text(terminal.backend().buffer())
    }

    #[test]
    fn a_filter_that_hides_every_line_says_so_rather_than_no_log_yet() {
        let (mut screen, data) = log_pane();
        screen.pane_filter.set_text("panic");
        let drawn = draw(&mut screen, &data);
        assert!(drawn.contains("No line matches the filter"), "{drawn}");
        assert!(!drawn.contains("No log yet"), "{drawn}");
    }

    #[test]
    fn the_filters_matches_are_worked_out_again_only_when_the_lines_or_the_filter_change() {
        let (mut screen, data) = log_pane();
        screen.pane_filter.set_text("listen");
        let drawn = draw(&mut screen, &data);
        assert!(
            drawn.contains("listening") && !drawn.contains("starting"),
            "{drawn}"
        );
        assert_eq!(screen.shown, [1]);

        // A stale answer planted in the cache is painted: nothing changed,
        // so nothing was worked out again.
        screen.shown = vec![0];
        let drawn = draw(&mut screen, &data);
        assert!(drawn.contains("starting"), "{drawn}");

        let target = screen.following().cloned().unwrap();
        screen.append_log(&target, vec!["listening again".into()], false);
        draw(&mut screen, &data);
        assert_eq!(screen.shown, [1, 2], "a new line: worked out again");
        screen.pane_filter.set_text("again");
        draw(&mut screen, &data);
        assert_eq!(screen.shown, [2], "a new filter: worked out again");
    }

    #[test]
    fn a_timestamped_line_is_cut_to_its_clock_and_painted_by_its_level() {
        let (stamp, rest) =
            split_timestamp("2026-09-12T12:04:02.123456789Z ERROR upstream timeout");
        assert_eq!(stamp.as_deref(), Some("12:04:02 "));
        assert_eq!(rest, "ERROR upstream timeout");
        assert_eq!(split_timestamp("plain line"), (None, "plain line"));
        assert_eq!(split_timestamp("2026 x"), (None, "2026 x"));

        assert_eq!(severity_style("ERROR x").fg, Some(theme().error));
        assert_eq!(severity_style("WARN x").fg, Some(theme().warning));
        assert_eq!(severity_style("INFO x").fg, None);
        assert_eq!(
            severity_style("\u{2026} kubectl refused").fg,
            Some(theme().error),
            "the stream's own complaint reads as an error"
        );
        let line = log_line("2026-09-12T12:04:02Z WARN slow");
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].content.as_ref(), "12:04:02 ");
    }
}
