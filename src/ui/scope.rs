//! One tab's panes: the search row, the table of whichever kind shows, and
//! the details pane with the text pane under it.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::details::{quiet, refused, render_pane};
use super::table::{Cell, TableSpec, render_list_table, table_geometry};
use super::textpane::render_text_pane;
use super::theme::theme;
use super::widgets::{Pane, placeholder, render_panes, render_scrollbar};
use super::{config, events, pods};
use crate::app::list::Row;
use crate::app::scope::ScopeScreen;
use crate::app::screen::{Button, Target};
use crate::app::shell::{Focus, Shell};
use crate::columns::TableLayout;
use crate::config::Tab;
use crate::kube::{ConfigMap, K8sEvent, Kind, Pod, SecretMeta};
use crate::search::Query;
use crate::store::ScopeData;
use crate::timestamp::Timestamp;

/// Draws the tab: one row for the search box, then the table and the details
/// pane, laid out to fit.
pub fn render(
    frame: &mut Frame,
    shell: &mut Shell,
    screen: &mut ScopeScreen,
    tab: &Tab,
    data: &ScopeData,
    area: Rect,
) {
    let input = screen.list().input.clone();
    render_panes(
        frame,
        shell,
        area,
        &input,
        placeholder(screen.kind),
        |frame, shell, pane, rect| match pane {
            Pane::Table => render_table(frame, shell, screen, data, rect),
            Pane::Details => render_details(frame, shell, screen, tab, data, rect),
        },
    );
}

/// The table itself. The list has already decided which rows are shown and
/// in what order; this turns the window of them that fits into cells.
fn render_table(
    frame: &mut Frame,
    shell: &mut Shell,
    screen: &mut ScopeScreen,
    data: &ScopeData,
    area: Rect,
) {
    let geometry = table_geometry(area);
    let available = TableLayout::available_width(geometry.inner.width);
    let kind = screen.kind;
    let status = screen.status(data);
    let list = screen.list_mut();
    list.note_width(available);
    let columns = list.layout.visible_columns(available);

    let now = Timestamp::now();
    let schema = match kind {
        Kind::Pods => Pod::SCHEMA,
        Kind::Events => K8sEvent::SCHEMA,
        Kind::ConfigMaps => ConfigMap::SCHEMA,
        Kind::Secrets => SecretMeta::SCHEMA,
    };
    let highlighter = Query::new(&crate::filter::Query::parse(list.input.text(), schema).words);

    // `window` is what records the viewport on the cursor, so a page and an
    // End know how far to move.
    let total = list.visible().len();
    let window = geometry.window(&mut list.cursor, total);
    let first = window.start;
    let shown: Vec<Vec<Cell>> = list.visible()[window]
        .iter()
        .map(|at| match kind {
            Kind::Pods => pods::cells(&data.pods.rows[*at], &columns, &highlighter, now),
            Kind::Events => events::cells(&data.events.rows[*at], &columns, &highlighter, now),
            Kind::ConfigMaps => {
                config::configmap_cells(&data.configmaps.rows[*at], &columns, &highlighter, now)
            }
            Kind::Secrets => {
                config::secret_cells(&data.secrets.rows[*at], &columns, &highlighter, now)
            }
        })
        .collect();

    let mut spec = TableSpec {
        title: format!(" {} ", kind.label()),
        status,
        focused: shell.focus == Focus::Table,
        columns: &columns,
        sorted: Some((list.sort, if list.descending { "↓" } else { "↑" })),
        rows: &shown,
        total,
        cursor: &mut list.cursor,
        hovered: None,
    };
    let hits = render_list_table(frame, area, &mut spec);

    for (index, rect) in hits.rows {
        shell.region(rect, Target::Row(index));
    }
    for (column, rect) in hits.headers {
        shell.region(rect, Target::Header(column));
    }
    render_scrollbar(
        frame,
        Rect::new(
            geometry.inner.right().saturating_sub(1),
            geometry.body.y,
            1,
            geometry.body.height,
        ),
        first,
        geometry.visible_rows,
        total,
    );
}

/// The details pane for the row under the cursor, with the text pane under
/// it when that is open; `z` gives the text pane the whole area.
fn render_details(
    frame: &mut Frame,
    shell: &mut Shell,
    screen: &mut ScopeScreen,
    tab: &Tab,
    data: &ScopeData,
    area: Rect,
) {
    if screen.pane_open && screen.pane_zoom {
        render_text_pane(frame, shell, screen, data, area);
        return;
    }
    let focused = shell.focus == Focus::Details && !screen.pane_open;
    let width = super::details::pane_width(area);
    let now = Timestamp::now();
    let kind = screen.kind;
    let listing = data.listing(kind);

    // What the pane says, and where its clickable lines are.
    let mut key_rows: Option<usize> = None;
    let have_row = screen.selected_object(data).is_some();
    let mut lines = if have_row {
        vec![toolbar_line(kind)]
    } else {
        Vec::new()
    };
    match kind {
        Kind::Pods => match screen.selected_pod(data) {
            Some(pod) => lines.extend(pods::detail_lines(
                pod,
                screen.owner_of(pod),
                listing.error,
                width,
                now,
            )),
            None => lines = nothing_selected(screen, tab, data),
        },
        Kind::Events => match screen.selected_event(data) {
            Some(event) => lines.extend(events::detail_lines(event, width, now)),
            None => lines = nothing_selected(screen, tab, data),
        },
        Kind::ConfigMaps | Kind::Secrets => {
            let keys = screen.keys(data);
            let head = match (
                screen.selected_configmap(data),
                screen.selected_secret(data),
            ) {
                _ if kind == Kind::ConfigMaps => screen.selected_configmap(data).map(|held| {
                    (
                        held.name.clone(),
                        held.namespace.clone(),
                        None,
                        held.created,
                    )
                }),
                (_, Some(held)) => Some((
                    held.name.clone(),
                    held.namespace.clone(),
                    Some(held.kind.clone()),
                    held.created,
                )),
                _ => None,
            };
            match head {
                Some((name, namespace, kind_word, created)) => {
                    let (more, first_key) = config::detail_lines(
                        &config::Head {
                            name: &name,
                            what: if kind == Kind::ConfigMaps {
                                "configmap"
                            } else {
                                "secret"
                            },
                            namespace: &namespace,
                            kind_word: kind_word.as_deref(),
                            created,
                        },
                        &keys,
                        screen.key_cursor,
                        width,
                        now,
                    );
                    key_rows = Some(lines.len() + first_key);
                    lines.extend(more);
                }
                None => lines = nothing_selected(screen, tab, data),
            }
        }
    }
    if have_row
        && let Some(message) = listing.error
        && kind != Kind::Pods
    {
        lines.push(Line::from(""));
        lines.push(refused(format!("last read failed: {message}")));
    }

    if !screen.pane_open {
        let rows = render_pane(
            frame,
            shell,
            area,
            focused,
            &mut screen.details_scroll,
            lines,
        );
        register_regions(shell, area, screen, have_row, key_rows, &rows);
        return;
    }
    // The details take what they need up to just under half; the pane
    // takes the rest and never less than a few lines.
    let count = lines.len();
    let wanted = u16::try_from(count).unwrap_or(u16::MAX).saturating_add(2);
    let top = wanted.min(area.height * 45 / 100).max(5.min(area.height));
    let [details, pane] =
        Layout::vertical([Constraint::Length(top), Constraint::Min(4)]).areas(area);
    let rows = render_pane(
        frame,
        shell,
        details,
        focused,
        &mut screen.details_scroll,
        lines,
    );
    register_regions(shell, details, screen, have_row, key_rows, &rows);
    render_text_pane(frame, shell, screen, data, pane);
}

/// The toolbar's buttons and the key rows, as regions: each button stands
/// for the key it names; each key row puts the details cursor on it. Each
/// line is placed at the row it starts on once the lines above have
/// wrapped, less the scroll, so a long name above the keys does not put
/// every region a row off.
fn register_regions(
    shell: &mut Shell,
    area: Rect,
    screen: &ScopeScreen,
    have_row: bool,
    key_rows: Option<usize>,
    rows: &[usize],
) {
    if !have_row || area.height < 3 {
        return;
    }
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    let offset = screen.details_scroll.offset;
    let starts: Vec<usize> = rows
        .iter()
        .scan(0, |start, height| {
            let here = *start;
            *start += height;
            Some(here)
        })
        .collect();
    let row_of = |line: usize| -> Option<u16> {
        let visible = starts.get(line)?.checked_sub(offset)?;
        let y = inner.y.saturating_add(u16::try_from(visible).ok()?);
        (y < inner.bottom()).then_some(y)
    };
    if let Some(y) = row_of(0)
        && usize::from(inner.width) >= toolbar_width(screen.kind)
    {
        let mut column = inner.x;
        for button in Button::for_kind(screen.kind) {
            let width = u16::try_from(button.label().chars().count() + 2).unwrap_or(0);
            shell.region(Rect::new(column, y, width, 1), Target::Button(*button));
            column = column.saturating_add(width + 1);
        }
    }
    if let Some(first) = key_rows {
        for at in 0..rows.len().saturating_sub(first) {
            if let Some(y) = row_of(first + at) {
                shell.region(Rect::new(inner.x, y, inner.width, 1), Target::KeyRow(at));
            }
        }
    }
}

/// `[Logs] [Bash] …`, with a space after each.
fn toolbar_width(kind: Kind) -> usize {
    Button::for_kind(kind)
        .iter()
        .map(|button| button.label().chars().count() + 3)
        .sum::<usize>()
        .saturating_sub(1)
}

fn toolbar_line(kind: Kind) -> Line<'static> {
    let palette = theme();
    let mut spans = Vec::new();
    for (at, button) in Button::for_kind(kind).iter().enumerate() {
        if at > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled("[", Style::default().fg(palette.muted)));
        spans.push(Span::styled(
            button.label(),
            Style::default()
                .fg(palette.link)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled("]", Style::default().fg(palette.muted)));
    }
    Line::from(spans)
}

/// What the pane says with nothing under the cursor: nothing has come back
/// yet, what went wrong, or nothing matches.
fn nothing_selected(screen: &ScopeScreen, tab: &Tab, data: &ScopeData) -> Vec<Line<'static>> {
    let scope = tab.scope.describe();
    let kind = screen.kind;
    let listing = data.listing(kind);
    let noun = kind.noun();
    if listing.count == 0 {
        return match listing.error {
            Some(message) => vec![refused(format!("{scope} {noun}: {message}"))],
            None if listing.reads == 0 => vec![quiet(format!("Reading {scope} {noun}…"))],
            None => vec![quiet(format!("No {noun} in {scope}"))],
        };
    }
    vec![quiet(format!("No {noun} match"))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config as configuration;
    use crate::kube::tests::{crashing, pod};
    use crate::ui::screen_text;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn tab() -> Tab {
        configuration::parse(configuration::tests::TWO_CLUSTERS)
            .unwrap()
            .tabs()
            .remove(0)
    }

    fn data() -> ScopeData {
        let mut data = ScopeData::default();
        data.pods.rows = vec![
            pod("qa", "dev", "orders-api-7d9f5b-k9x2p", "Running"),
            crashing("qa", "dev", "orders-worker-5c4d3e-q8zt"),
        ];
        data.pods.reads = 1;
        data.events.rows = vec![
            K8sEvent::from_json(&serde_json::json!({
                "metadata": {"name": "w1", "namespace": "dev"},
                "lastTimestamp": "2026-09-12T12:00:00Z", "type": "Warning", "reason": "BackOff", "count": 17,
                "involvedObject": {"kind": "Pod", "name": "orders-worker-5c4d3e-q8zt", "namespace": "dev"},
                "message": "Back-off restarting failed container", "source": {"component": "kubelet"}
            }))
            .unwrap(),
        ];
        data.events.reads = 1;
        data.configmaps.rows = vec![
            ConfigMap::from_json(&serde_json::json!({
                "metadata": {"name": "orders-config", "namespace": "dev"},
                "data": {"LOG_LEVEL": "info"}
            }))
            .unwrap(),
        ];
        data.configmaps.reads = 1;
        data.secrets.rows = vec![
            SecretMeta::from_json(&serde_json::json!({
                "metadata": {"name": "db", "namespace": "dev"}, "type": "Opaque",
                "data": {"password": "aHVudGVyMg=="}
            }))
            .unwrap(),
        ];
        data.secrets.reads = 1;
        data
    }

    fn draw(
        width: u16,
        height: u16,
        screen: &mut ScopeScreen,
        data: &ScopeData,
    ) -> (String, Shell, ratatui::buffer::Buffer) {
        let mut shell = Shell::default();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                shell.begin_frame();
                screen.refilter(data);
                render(frame, &mut shell, screen, &tab(), data, frame.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (screen_text(&buffer), shell, buffer)
    }

    #[test]
    fn the_table_paints_a_pod_in_trouble_in_the_error_colour_from_end_to_end() {
        let data = data();
        let mut screen = ScopeScreen::new(false);
        let (drawn, _, buffer) = draw(120, 16, &mut screen, &data);
        assert!(drawn.contains("Ready"), "{drawn}");
        assert!(drawn.contains("\u{2717} CrashLoopBackOff"), "{drawn}");
        assert!(drawn.contains("\u{25cf} Running"), "{drawn}");
        assert!(drawn.contains("2 · Name ↑"), "{drawn}");
        let row = (0..buffer.area.height)
            .find(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("orders-worker")
            })
            .expect("the crashing pod's row");
        let painted: Vec<_> = (4..12).map(|x| buffer[(x, row)].fg).collect();
        assert!(
            painted.iter().all(|colour| *colour == theme().error),
            "the whole row reads as trouble: {painted:?}"
        );
    }

    #[test]
    fn the_details_pane_names_the_pod_its_owner_and_its_containers_under_the_toolbar() {
        let data = data();
        let mut screen = ScopeScreen::new(false);
        let (drawn, shell, _) = draw(120, 20, &mut screen, &data);
        assert!(
            drawn.contains("[Logs] [Bash] [Restart] [Scale] [Describe] [YAML]"),
            "{drawn}"
        );
        assert!(
            drawn.contains("orders-api-7d9f5b-k9x2p  Running"),
            "{drawn}"
        );
        assert!(drawn.contains("qa/dev · Deployment/orders-api"), "{drawn}");
        assert!(drawn.contains("── Containers"), "{drawn}");
        assert!(drawn.contains("✓ api  Running  ↻0"), "{drawn}");
        assert!(shell.find(&Target::Button(Button::Bash)).is_some());
        screen.list_mut().cursor.focus(1);
        let (drawn, _, _) = draw(120, 20, &mut screen, &data);
        assert!(drawn.contains("✗ api  CrashLoopBackOff  ↻9"), "{drawn}");
        assert!(drawn.contains("last exit: Error (1)"), "{drawn}");
    }

    #[test]
    fn each_kind_has_its_own_table_details_and_toolbar() {
        let data = data();
        let mut screen = ScopeScreen::new(false);
        screen.set_kind(Kind::Events);
        let (drawn, shell, _) = draw(120, 20, &mut screen, &data);
        assert!(drawn.contains(" Events "), "{drawn}");
        assert!(drawn.contains("Reason"), "{drawn}");
        assert!(drawn.contains("BackOff"), "{drawn}");
        assert!(drawn.contains("pod/orders-worker-5c4d3e-q8zt"), "{drawn}");
        assert!(drawn.contains("[Pod] [Describe] [YAML]"), "{drawn}");
        assert!(drawn.contains("17×"), "{drawn}");
        assert!(
            drawn.contains("Back-off restarting failed container"),
            "{drawn}"
        );
        assert!(shell.find(&Target::Button(Button::Pod)).is_some());

        screen.set_kind(Kind::ConfigMaps);
        let (drawn, shell, _) = draw(120, 20, &mut screen, &data);
        assert!(drawn.contains(" ConfigMaps "), "{drawn}");
        assert!(drawn.contains("orders-config"), "{drawn}");
        assert!(drawn.contains("── Keys"), "{drawn}");
        assert!(drawn.contains("› LOG_LEVEL  4 bytes"), "{drawn}");
        assert!(drawn.contains("[Value] [Describe]"), "{drawn}");
        let key = shell.find(&Target::KeyRow(0)).expect("a key row");
        assert_eq!(shell.hit(key.x + 3, key.y), Some(&Target::KeyRow(0)));

        screen.set_kind(Kind::Secrets);
        let (drawn, _, _) = draw(120, 20, &mut screen, &data);
        assert!(drawn.contains(" Secrets "), "{drawn}");
        assert!(drawn.contains("Opaque"), "{drawn}");
        assert!(drawn.contains("› password  7 bytes"), "{drawn}");
        assert!(drawn.contains("[Value] [Copy] [Describe]"), "{drawn}");
        assert!(
            !drawn.contains("hunter2"),
            "never on screen unasked: {drawn}"
        );
    }

    #[test]
    fn a_query_narrows_the_table_and_the_pane_says_what_it_is_waiting_for() {
        let data = data();
        let mut screen = ScopeScreen::new(false);
        screen.list_mut().input.set_text("worker");
        let (drawn, _, _) = draw(120, 14, &mut screen, &data);
        assert!(drawn.contains("1/2 · Name ↑"), "{drawn}");
        screen.list_mut().input.set_text("nothing");
        let (drawn, _, _) = draw(120, 14, &mut screen, &data);
        assert!(drawn.contains("No pods match"), "{drawn}");

        let mut screen = ScopeScreen::new(false);
        let (drawn, _, _) = draw(120, 14, &mut screen, &ScopeData::default());
        assert!(drawn.contains("Reading qa/dev pods…"), "{drawn}");
        screen.set_kind(Kind::Secrets);
        let mut failed = ScopeData::default();
        failed.secrets.error = Some("Error from server (Forbidden): secrets is forbidden".into());
        failed.secrets.reads = 1;
        let (drawn, _, _) = draw(120, 14, &mut screen, &failed);
        assert!(
            drawn.contains("qa/dev secrets: Error from server (Forbidden)"),
            "{drawn}"
        );
        let mut empty = ScopeData::default();
        empty.secrets.reads = 1;
        let (drawn, _, _) = draw(120, 14, &mut screen, &empty);
        assert!(drawn.contains("No secrets in qa/dev"), "{drawn}");
    }

    #[test]
    fn under_seventy_columns_the_details_pane_waits_for_tab() {
        let data = data();
        let mut screen = ScopeScreen::new(false);
        let (drawn, _, _) = draw(60, 16, &mut screen, &data);
        assert!(
            !drawn.contains("Details"),
            "the table gets the room: {drawn}"
        );
        assert!(drawn.contains("orders-api"), "{drawn}");
    }

    #[test]
    fn a_key_row_under_a_name_that_wraps_is_where_the_key_is_drawn() {
        let mut data = data();
        let long = format!("sh.helm.release.v1.{}.v3", "orders-".repeat(12));
        data.configmaps.rows = vec![
            ConfigMap::from_json(&serde_json::json!({
                "metadata": {"name": long, "namespace": "dev"},
                "data": {"LOG_LEVEL": "info"}
            }))
            .unwrap(),
        ];
        let mut screen = ScopeScreen::new(false);
        screen.set_kind(Kind::ConfigMaps);
        let (drawn, shell, buffer) = draw(120, 24, &mut screen, &data);
        let key_line = (0..buffer.area.height)
            .find(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("› LOG_LEVEL")
            })
            .unwrap_or_else(|| panic!("the key row: {drawn}"));
        let region = shell.find(&Target::KeyRow(0)).expect("a key row region");
        assert_eq!(region.y, key_line, "{drawn}");
    }
}
