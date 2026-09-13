//! The pieces of the frame that are not a table: the tab bar, the search
//! row, the status bar, the scrollbar, and the help modal.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::theme::theme;
use crate::app::keys;
use crate::app::screen::{TabId, Target};
use crate::app::shell::{Focus, Level, Panes, Shell};
use crate::filter::{ENV_CHOICES, Env};
use crate::store::{AzureStore, problem_line};
use crate::text_input::{TextInput, field_window};
use crate::timestamp::Timestamp;

/// The frames of the spinner that turns while a refresh runs.
const SPINNER: [char; 4] = ['◐', '◓', '◑', '◒'];

/// What each table's search row says before anything is typed. The grammar
/// differs per table, and a placeholder naming another table's keys is a
/// worked example of something that will not match.
pub const SECRETS_PLACEHOLDER: &str = "Type / to search, or env:prod enabled:no expires:<30d";
pub const REPOSITORIES_PLACEHOLDER: &str = "Type / to search, or env:prod updated:<30d";
pub const TAGS_PLACEHOLDER: &str = "Type / to search, or tag:1.4 digest:ab12 updated:<30d";

/// The `key:value` filters each table takes, for the help. The README is
/// otherwise the only place the grammar is written down.
const FILTERS: &[(TabId, &str, &str)] = &[
    (
        TabId::Secrets,
        "Filters",
        "env:dev|qa|prod vault: name: type: enabled:yes|no managed:yes|no tag:key=value expires:<30d|>30d|none|expired",
    ),
    (
        TabId::Registries,
        "Filters",
        "env:dev|qa|prod registry: repo: updated:<30d|>30d created:<30d|>30d",
    ),
    (
        TabId::Registries,
        "Inside one",
        "tag: digest: updated:<30d|>30d created:<30d|>30d",
    ),
];

/// Which pane a frame is asking a screen to draw, at the width it has.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Pane {
    Table,
    Details,
}

/// A tab's body: the search row, then the table and the details pane laid
/// out to fit — side by side, stacked, or one at a time with `Tab` saying
/// which. The screen draws each pane it is asked for; this decides where.
pub fn render_panes(
    frame: &mut Frame,
    shell: &mut Shell,
    area: Rect,
    input: &TextInput,
    placeholder: &str,
    mut draw: impl FnMut(&mut Frame, &mut Shell, Pane, Rect),
) {
    let [search, body] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3)])
        .areas(area);
    let focused = shell.focus == Focus::Search;
    render_search_row(frame, shell, search, input, focused, placeholder);

    match Shell::panes(area.width) {
        Panes::SideBySide => {
            let [table, details] = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                .areas(body);
            draw(frame, shell, Pane::Table, table);
            draw(frame, shell, Pane::Details, details);
        }
        Panes::Stacked => {
            let [table, details] = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(body);
            draw(frame, shell, Pane::Table, table);
            draw(frame, shell, Pane::Details, details);
        }
        Panes::One if shell.focus == Focus::Details => draw(frame, shell, Pane::Details, body),
        Panes::One => draw(frame, shell, Pane::Table, body),
    }
}

/// Which spinner frame this instant shows. Driven by the clock rather than by
/// a counter, so a slow frame does not make the spinner stutter.
#[must_use]
pub fn spinner_frame(millis: u128) -> char {
    SPINNER[(millis / 120) as usize % SPINNER.len()]
}

/// The tab bar: the numbers, the names, and a badge where a screen has
/// something to say. Names shorten before any of them is dropped.
pub fn render_tab_bar(
    frame: &mut Frame,
    shell: &mut Shell,
    area: Rect,
    active: TabId,
    badges: &[(TabId, Option<String>)],
) {
    let palette = theme();
    let short = area.width < 44;
    let mut spans = Vec::new();
    let mut column = area.x;
    for tab in TabId::ALL {
        let badge = badges
            .iter()
            .find(|(held, _)| *held == tab)
            .and_then(|(_, badge)| badge.clone());
        let name = if short {
            tab.short_label()
        } else {
            tab.label()
        };
        let label = format!(" {} {name}", tab.number());
        let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
        let style = if tab == active {
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.muted)
        };
        spans.push(Span::styled(label, style));
        let mut hit_width = width;
        if let Some(badge) = badge {
            let badge = format!(" {badge}");
            hit_width += u16::try_from(badge.chars().count()).unwrap_or(0);
            spans.push(Span::styled(badge, Style::default().fg(palette.warning)));
        }
        if column < area.right() {
            shell.region(
                Rect::new(column, area.y, hit_width.min(area.right() - column), 1),
                Target::Tab(tab),
            );
        }
        column += hit_width;
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);

    // The `?` sits at the right end of the same row.
    if area.width > 2 {
        let help = Rect::new(area.right() - 2, area.y, 1, 1);
        frame.render_widget(
            Paragraph::new(Span::styled("?", Style::default().fg(palette.muted))),
            help,
        );
        shell.region(help, Target::Help);
    }
}

/// The one-line search field: the `/` glyph, the text, and a `×` to clear it.
pub fn render_search_row(
    frame: &mut Frame,
    shell: &mut Shell,
    area: Rect,
    input: &TextInput,
    focused: bool,
    placeholder: &str,
) {
    let palette = theme();
    // `/ ` on the left, ` × ` on the right when there is anything to clear.
    let clearable = !input.is_empty();
    let right = if clearable { 3 } else { 0 };
    let [glyph, field, clear] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(right),
        ])
        .areas(area);

    frame.render_widget(
        Paragraph::new(Span::styled(
            "/ ",
            Style::default().fg(if focused {
                palette.accent
            } else {
                palette.muted
            }),
        )),
        glyph,
    );

    if input.is_empty() && !focused {
        frame.render_widget(
            Paragraph::new(Span::styled(
                placeholder.to_owned(),
                Style::default().fg(palette.muted),
            )),
            field,
        );
    } else {
        let (first, caret) = field_window(input.text(), input.cursor(), field.width);
        let shown: String = input.text().chars().skip(first).collect();
        frame.render_widget(
            Paragraph::new(Span::styled(shown, Style::default().fg(palette.text))),
            field,
        );
        if focused {
            frame.set_cursor_position((field.x + caret, field.y));
        }
    }
    shell.region(field, Target::SearchField);

    if clearable {
        frame.render_widget(
            Paragraph::new(Span::styled(" × ", Style::default().fg(palette.muted))),
            clear,
        );
        shell.region(clear, Target::ClearSearch);
    }
}

/// The bottom row: what just happened, or what the keys do, and on the right
/// what the store holds.
pub fn render_status_bar(
    frame: &mut Frame,
    shell: &mut Shell,
    area: Rect,
    hint: &str,
    store: &AzureStore,
    tab: TabId,
    millis: u128,
) {
    let palette = theme();
    let (left, style) = match shell.notification() {
        Some((said, Level::Error)) => (said.to_owned(), Style::default().fg(palette.error)),
        Some((said, Level::Info)) => (said.to_owned(), Style::default().fg(palette.success)),
        None => (hint.to_owned(), Style::default().fg(palette.muted)),
    };
    let (right, right_style) = store_state(store, tab, millis);
    let right_width = u16::try_from(right.chars().count()).unwrap_or(0);

    // The right-hand end is the one that cannot be guessed from the keys, so
    // it keeps its room and the hint is cut to what is left. Two spaces of
    // gap, so the two never read as one sentence.
    let (left, right) = if right_width + 3 >= area.width {
        // No room for both: what is happening beats what the keys do.
        (String::new(), right)
    } else {
        let room = usize::from(area.width - right_width - 3);
        (truncate(&left, room), right)
    };

    frame.render_widget(Paragraph::new(Span::styled(left, style)), area);
    if right_width < area.width {
        frame.render_widget(
            Paragraph::new(Span::styled(right, right_style)).alignment(Alignment::Right),
            Rect::new(area.x, area.y, area.width.saturating_sub(1), 1),
        );
    }
}

/// `text` in at most `room` cells, with an ellipsis where something was cut.
fn truncate(text: &str, room: usize) -> String {
    if text.chars().count() <= room {
        return text.to_owned();
    }
    if room <= 1 {
        return String::new();
    }
    text.chars().take(room - 1).chain(['…']).collect()
}

/// The right-hand end of the status bar: what is happening, or what is
/// wrong, or what this tab holds and when it was read.
fn store_state(store: &AzureStore, tab: TabId, millis: u128) -> (String, Style) {
    let palette = theme();
    if store.refreshing {
        let said = store.progress.clone().unwrap_or_else(|| "reading…".into());
        return (
            format!("{} {said}", spinner_frame(millis)),
            Style::default().fg(palette.info),
        );
    }
    if let Some(problem) = store.first_problem() {
        return (
            format!("! {}", problem_line(problem)),
            Style::default().fg(palette.error),
        );
    }
    let age = store.read_at.map_or_else(
        || "never read".to_owned(),
        |read_at| match read_at.relative_age(Timestamp::now()).as_str() {
            "now" => "just now".to_owned(),
            age => format!("{age} ago"),
        },
    );
    let counts = match tab {
        TabId::Secrets => format!(
            "{} vaults · {} secrets",
            store.inventory.vaults.len(),
            store.secrets.len()
        ),
        TabId::Registries => format!(
            "{} registries · {} repositories",
            store.inventory.registries.len(),
            store.repositories.len()
        ),
    };
    (
        format!("● {counts} · {age}"),
        Style::default().fg(palette.muted),
    )
}

/// A centred box for a modal, with the screen behind it washed out where the
/// palette says to.
pub fn render_modal_frame(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    width: u16,
    height: u16,
) -> Rect {
    let palette = theme();
    if palette.dim_behind_modals {
        dim_behind(frame, area);
    }
    let width = width.min(area.width.saturating_sub(2)).max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    let modal = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(palette.border_focused))
        .title(Line::from(format!(" {title} ")).style(Style::default().fg(palette.accent)));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    inner
}

/// Washes out what is behind a modal, so the modal is obviously the thing
/// taking keys.
pub fn dim_behind(frame: &mut Frame, area: Rect) {
    let buffer = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buffer[(x, y)].set_style(Style::default().add_modifier(Modifier::DIM));
        }
    }
}

/// The help: every key this tab has, the filters its search box takes, then
/// whatever is wrong.
pub fn render_help(
    frame: &mut Frame,
    shell: &mut Shell,
    area: Rect,
    tab: TabId,
    store: &AzureStore,
) {
    const WIDTH: u16 = 74;
    let palette = theme();
    let entry = |keys: &str, does: &str| {
        Line::from(vec![
            Span::styled(format!("{keys:<12}"), Style::default().fg(palette.accent)),
            Span::styled(does.to_owned(), Style::default().fg(palette.body)),
        ])
    };
    let mut lines: Vec<Line> = keys::for_tab(tab)
        .map(|key| entry(key.keys, key.does))
        .collect();
    lines.push(Line::from(""));
    lines.extend(
        FILTERS
            .iter()
            .filter(|(held, _, _)| *held == tab)
            .map(|(_, label, grammar)| entry(label, grammar)),
    );
    if !store.problems.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Problems",
            Style::default()
                .fg(palette.header)
                .add_modifier(Modifier::BOLD),
        )));
        for problem in &store.problems {
            lines.push(Line::from(Span::styled(
                problem_line(problem),
                Style::default().fg(palette.error),
            )));
        }
    }
    // Wrapped, and sized to what the wrapping makes of it: a refusal names
    // the role that would fix it in its second half, which a cut line lost.
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let height = u16::try_from(paragraph.line_count(WIDTH - 2) + 2).unwrap_or(u16::MAX);
    let inner = render_modal_frame(frame, area, "Keys", WIDTH, height);
    shell.region(area, Target::Help);
    frame.render_widget(paragraph, inner);
}

/// The Env header's menu, under the header that was clicked: every
/// environment and `All`, with a tick on the one the query has now and the
/// keys' line lit. Drawn last so it sits over whichever pane is under it.
pub fn render_env_menu(
    frame: &mut Frame,
    shell: &mut Shell,
    anchor: Rect,
    current: Option<Env>,
    highlighted: usize,
) {
    let palette = theme();
    let lines = u16::try_from(ENV_CHOICES.len()).unwrap_or(u16::MAX);
    // One cell left of the header, so the labels line up under it.
    let area = Rect::new(
        anchor.x.saturating_sub(1),
        anchor.y.saturating_add(1),
        8,
        lines.saturating_add(2),
    )
    .intersection(frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(palette.border_focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    for (at, choice) in ENV_CHOICES.iter().enumerate() {
        let y = inner
            .y
            .saturating_add(u16::try_from(at).unwrap_or(u16::MAX));
        if y >= inner.bottom() {
            break;
        }
        let line = Rect::new(inner.x, y, inner.width, 1);
        let mark = if *choice == current {
            "\u{2713} "
        } else {
            "  "
        };
        let label = choice.map_or("All", Env::label);
        let mut style = Style::default().fg(palette.text);
        if at == highlighted {
            style = style
                .bg(palette.selected_background)
                .add_modifier(Modifier::BOLD);
        }
        frame.render_widget(
            Paragraph::new(Span::styled(format!("{mark}{label}"), style)),
            line,
        );
        shell.region(line, Target::EnvOption(*choice));
    }
}

/// A one-character scrollbar down the right edge of a pane, drawn only when
/// there is more content than viewport.
pub fn render_scrollbar(
    frame: &mut Frame,
    area: Rect,
    offset: usize,
    viewport: usize,
    content: usize,
) {
    if content <= viewport || area.height == 0 || area.width == 0 {
        return;
    }
    let palette = theme();
    let track = area.height as usize;
    let thumb = ((track * viewport) / content).clamp(1, track);
    let travel = track.saturating_sub(thumb);
    let furthest = content - viewport;
    let at = if travel == 0 || furthest == 0 {
        0
    } else {
        (offset * travel + furthest / 2) / furthest
    };
    let buffer = frame.buffer_mut();
    for row in 0..track {
        let inside = row >= at && row < at + thumb;
        buffer[(area.x, area.y + u16::try_from(row).unwrap_or(0))]
            .set_symbol(if inside { "█" } else { "│" })
            .set_style(Style::default().fg(palette.scrollbar));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::Inventory;
    use crate::worker::Event;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn screen(width: u16, height: u16, draw: impl FnOnce(&mut Frame, &mut Shell)) -> String {
        let mut shell = Shell::default();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                shell.begin_frame();
                draw(frame, &mut shell);
            })
            .unwrap();
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
    fn the_tab_bar_names_both_tabs_and_paints_a_badge() {
        let drawn = screen(80, 1, |frame, shell| {
            render_tab_bar(
                frame,
                shell,
                Rect::new(0, 0, 80, 1),
                TabId::Secrets,
                &[(TabId::Secrets, Some("⚠ 3".into()))],
            );
        });
        assert!(drawn.contains("1 Secrets"), "{drawn}");
        assert!(drawn.contains("⚠ 3"), "{drawn}");
        assert!(drawn.contains("2 Registries"), "{drawn}");
        assert!(drawn.trim_end().ends_with('?'), "{drawn}");
    }

    #[test]
    fn a_narrow_tab_bar_shortens_the_names_rather_than_dropping_one() {
        let drawn = screen(40, 1, |frame, shell| {
            render_tab_bar(frame, shell, Rect::new(0, 0, 40, 1), TabId::Secrets, &[]);
        });
        assert!(drawn.contains("1 Sec"), "{drawn}");
        assert!(drawn.contains("2 Reg"), "{drawn}");
    }

    #[test]
    fn a_click_on_a_tab_lands_on_that_tab() {
        let mut shell = Shell::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|frame| {
                shell.begin_frame();
                render_tab_bar(
                    frame,
                    &mut shell,
                    Rect::new(0, 0, 80, 1),
                    TabId::Secrets,
                    &[],
                );
            })
            .unwrap();
        assert_eq!(shell.hit(3, 0), Some(&Target::Tab(TabId::Secrets)));
        assert_eq!(shell.hit(12, 0), Some(&Target::Tab(TabId::Registries)));
        assert_eq!(shell.hit(78, 0), Some(&Target::Help));
    }

    #[test]
    fn the_search_row_shows_the_placeholder_until_something_is_typed() {
        let drawn = screen(80, 1, |frame, shell| {
            render_search_row(
                frame,
                shell,
                Rect::new(0, 0, 80, 1),
                &TextInput::default(),
                false,
                SECRETS_PLACEHOLDER,
            );
        });
        assert!(drawn.starts_with("/ Type / to search"), "{drawn}");
        assert!(!drawn.contains('×'), "nothing to clear yet: {drawn}");

        let drawn = screen(80, 1, |frame, shell| {
            render_search_row(
                frame,
                shell,
                Rect::new(0, 0, 80, 1),
                &TextInput::new("db-pass"),
                true,
                SECRETS_PLACEHOLDER,
            );
        });
        assert!(drawn.contains("db-pass"), "{drawn}");
        assert!(drawn.contains('×'), "{drawn}");

        // Each tab names its own grammar; the other tab's keys are a worked
        // example of something that will not match.
        let drawn = screen(80, 1, |frame, shell| {
            render_search_row(
                frame,
                shell,
                Rect::new(0, 0, 80, 1),
                &TextInput::default(),
                false,
                REPOSITORIES_PLACEHOLDER,
            );
        });
        assert!(drawn.contains("env:prod updated:"), "{drawn}");
        assert!(!drawn.contains("enabled:"), "{drawn}");
    }

    #[test]
    fn the_status_bar_says_the_hint_then_the_notification_then_the_error() {
        let store = AzureStore::default();
        let drawn = screen(100, 1, |frame, shell| {
            render_status_bar(
                frame,
                shell,
                Rect::new(0, 0, 100, 1),
                "↑↓ move",
                &store,
                TabId::Secrets,
                0,
            );
        });
        assert!(drawn.contains("↑↓ move"), "{drawn}");
        assert!(drawn.contains("never read"), "{drawn}");

        let mut shell = Shell::default();
        shell.set_status("Copied value of db-password (kv-prod)");
        let mut terminal = Terminal::new(TestBackend::new(100, 1)).unwrap();
        terminal
            .draw(|frame| {
                shell.begin_frame();
                render_status_bar(
                    frame,
                    &mut shell,
                    Rect::new(0, 0, 100, 1),
                    "↑↓ move",
                    &store,
                    TabId::Secrets,
                    0,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let drawn: String = (0..100).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(drawn.contains("Copied value of db-password"), "{drawn}");
        assert!(!drawn.contains("↑↓ move"), "the notification wins: {drawn}");
    }

    #[test]
    fn the_two_halves_of_the_status_bar_never_run_into_each_other() {
        let mut store = AzureStore::default();
        store.apply(Event::Inventory(Err(
            "not signed in — run `az login`".into()
        )));
        for width in [40, 60, 80, 100, 120] {
            let drawn = screen(width, 1, |frame, shell| {
                render_status_bar(
                    frame,
                    shell,
                    Rect::new(0, 0, width, 1),
                    "↑↓/jk move  / search  s sort  y copy value  v reveal  r refresh  ? help",
                    &store,
                    TabId::Secrets,
                    0,
                );
            });
            assert!(
                drawn.contains("not signed in"),
                "what is wrong survives every width: {width} {drawn}"
            );
            assert!(
                !drawn.contains("sor!") && !drawn.contains("movе!"),
                "the hint is cut rather than written over: {width} {drawn}"
            );
        }
    }

    #[test]
    fn a_hint_is_cut_with_an_ellipsis_rather_than_in_the_middle_of_nothing() {
        assert_eq!(truncate("↑↓/jk move", 20), "↑↓/jk move");
        assert_eq!(truncate("↑↓/jk move", 6), "↑↓/jk…");
        assert_eq!(truncate("↑↓/jk move", 1), "");
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn the_status_bar_shows_the_spinner_while_reading_and_the_problem_after() {
        let mut store = AzureStore::default();
        store.apply(Event::Inventory(Ok(Inventory::default())));
        store.apply(Event::Progress("reading kv-prod (2/3)…".into()));
        let drawn = screen(100, 1, |frame, shell| {
            render_status_bar(
                frame,
                shell,
                Rect::new(0, 0, 100, 1),
                "",
                &store,
                TabId::Secrets,
                0,
            );
        });
        assert!(drawn.contains("reading kv-prod (2/3)"), "{drawn}");
        assert!(drawn.contains(SPINNER[0]), "{drawn}");

        store.apply(Event::Secrets {
            vault: "kv-prod".into(),
            result: Err("no permission to read secrets".into()),
        });
        store.apply(Event::Idle);
        let drawn = screen(100, 1, |frame, shell| {
            render_status_bar(
                frame,
                shell,
                Rect::new(0, 0, 100, 1),
                "",
                &store,
                TabId::Secrets,
                0,
            );
        });
        assert!(drawn.contains("! kv-prod: no permission"), "{drawn}");
    }

    #[test]
    fn the_help_lists_this_tabs_keys_and_the_problems_under_them() {
        let mut store = AzureStore::default();
        store.apply(Event::Inventory(Ok(Inventory::default())));
        store.apply(Event::Secrets {
            vault: "kv-prod".into(),
            result: Err("blocked by the vault firewall".into()),
        });
        let drawn = screen(100, 30, |frame, shell| {
            render_help(
                frame,
                shell,
                Rect::new(0, 0, 100, 30),
                TabId::Secrets,
                &store,
            );
        });
        assert!(drawn.contains("Keys"), "{drawn}");
        assert!(drawn.contains("copy the value"), "{drawn}");
        assert!(!drawn.contains("repository's tags"), "{drawn}");
        assert!(drawn.contains("Filters"), "{drawn}");
        assert!(drawn.contains("expires:<30d"), "{drawn}");
        assert!(drawn.contains("Problems"), "{drawn}");
        assert!(drawn.contains("blocked by the vault firewall"), "{drawn}");
    }

    #[test]
    fn a_long_problem_wraps_in_the_help_so_the_fix_is_readable() {
        let mut store = AzureStore::default();
        store.apply(Event::Inventory(Ok(Inventory::default())));
        store.apply(Event::Secrets {
            vault: "kv-prod".into(),
            result: Err("no permission to read secrets (needs the Key Vault Secrets User role or a `list` access policy)".into()),
        });
        let drawn = screen(100, 40, |frame, shell| {
            render_help(
                frame,
                shell,
                Rect::new(0, 0, 100, 40),
                TabId::Secrets,
                &store,
            );
        });
        assert!(
            drawn.contains("access policy"),
            "the second half of the line is the actionable half: {drawn}"
        );
    }

    #[test]
    fn the_status_bar_counts_what_the_open_tab_holds() {
        let store = AzureStore::default();
        let drawn = screen(100, 1, |frame, shell| {
            render_status_bar(
                frame,
                shell,
                Rect::new(0, 0, 100, 1),
                "",
                &store,
                TabId::Registries,
                0,
            );
        });
        assert!(drawn.contains("registries"), "{drawn}");
        assert!(!drawn.contains("secrets"), "{drawn}");
    }

    #[test]
    fn the_env_menu_lists_every_choice_under_the_header_and_each_takes_a_click() {
        let mut shell = Shell::default();
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal
            .draw(|frame| {
                shell.begin_frame();
                render_env_menu(frame, &mut shell, Rect::new(3, 2, 6, 1), Some(Env::Qa), 3);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let drawn: String = (0..12)
            .map(|y| (0..40).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(drawn.contains("  All"), "{drawn}");
        assert!(
            drawn.contains("\u{2713} qa"),
            "the one the query has: {drawn}"
        );
        assert!(drawn.contains("  prod"), "{drawn}");
        assert_eq!(shell.hit(4, 4), Some(&Target::EnvOption(None)));
        assert_eq!(shell.hit(4, 7), Some(&Target::EnvOption(Some(Env::Prod))));
        assert_eq!(
            buffer[(4, 7)].bg,
            theme().selected_background,
            "the keys' line is lit"
        );
    }

    #[test]
    fn the_scrollbar_appears_only_when_there_is_more_than_fits() {
        let drawn = screen(3, 10, |frame, _| {
            render_scrollbar(frame, Rect::new(2, 0, 1, 10), 0, 10, 10);
        });
        assert!(!drawn.contains('█') && !drawn.contains('│'), "{drawn}");

        let drawn = screen(3, 10, |frame, _| {
            render_scrollbar(frame, Rect::new(2, 0, 1, 10), 0, 10, 40);
        });
        assert!(drawn.contains('█'), "{drawn}");
        assert_eq!(drawn.lines().next().unwrap().trim_start(), "█", "{drawn}");
        assert_eq!(drawn.lines().last().unwrap().trim_start(), "│", "{drawn}");
    }

    #[test]
    fn the_spinner_turns_with_the_clock() {
        assert_eq!(spinner_frame(0), SPINNER[0]);
        assert_eq!(spinner_frame(120), SPINNER[1]);
        assert_eq!(spinner_frame(480), SPINNER[0], "and comes round again");
    }
}
