//! The Secrets tab's panes: the search row and the table under it.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::details::{
    LABEL, chip, coloured_field, field, link_field, pane_width, quiet, refused, render_pane,
    section, title, with_hint,
};
use super::table::{Cell, TableSpec, render_list_table, table_geometry};
use super::theme::theme;
use super::widgets::{Pane, SECRETS_PLACEHOLDER, render_panes, render_scrollbar};
use crate::app::screen::Target;
use crate::app::secrets::{Expiry, SCHEMA, SecretsScreen};
use crate::app::shell::{Focus, Shell};
use crate::azure::SecretRow;
use crate::columns::{ColumnConfig, ColumnId, TableLayout};
use crate::search::Query;
use crate::store::Store;
use crate::timestamp::{Timestamp, age};

/// Draws the tab: one row for the search box, then the table and the details
/// pane, laid out to fit.
pub fn render(
    frame: &mut Frame,
    shell: &mut Shell,
    screen: &mut SecretsScreen,
    store: &Store,
    area: Rect,
) {
    let input = screen.input.clone();
    render_panes(
        frame,
        shell,
        area,
        &input,
        SECRETS_PLACEHOLDER,
        |frame, shell, pane, rect| match pane {
            Pane::Table => render_table(frame, shell, screen, store, rect),
            Pane::Details => render_details(frame, shell, screen, store, rect),
        },
    );
}

/// The table itself. The screen has already decided which rows are shown and
/// in what order; this turns the window of them that fits into cells.
pub fn render_table(
    frame: &mut Frame,
    shell: &mut Shell,
    screen: &mut SecretsScreen,
    store: &Store,
    area: Rect,
) {
    let geometry = table_geometry(area);
    let available = TableLayout::available_width(geometry.inner.width);
    screen.note_width(available);
    let columns = screen.layout.visible_columns(available);

    let now = Timestamp::now();
    let mut highlighter =
        Query::new(&crate::filter::Query::parse(screen.input.text(), SCHEMA).words);

    // `window` is what records the viewport on the cursor, so a page and an
    // End know how far to move; taking a slice by hand would leave the
    // cursor thinking the list was one row tall.
    let total = screen.visible().len();
    let window = geometry.window(&mut screen.cursor, total);
    let first = window.start;
    let shown: Vec<Vec<Cell>> = screen.visible()[window]
        .iter()
        .map(|at| row_cells(&store.secrets[*at], &columns, store, &mut highlighter, now))
        .collect();

    let hovered = None;
    let mut spec = TableSpec {
        title: " Secrets ".to_owned(),
        status: screen.status(store),
        focused: shell.focus == Focus::Table,
        columns: &columns,
        sorted: Some((screen.sort, if screen.descending { "↓" } else { "↑" })),
        rows: &shown,
        total,
        cursor: &mut screen.cursor,
        hovered,
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

/// Eight dots, always. A mask that was as long as the value would be telling
/// people how long the value is.
const MASK: &str = "••••••••";

/// The details pane for the row under the cursor.
pub fn render_details(
    frame: &mut Frame,
    shell: &mut Shell,
    screen: &mut SecretsScreen,
    store: &Store,
    area: Rect,
) {
    let focused = shell.focus == Focus::Details;
    let lines = match screen.selected(store).cloned() {
        Some(row) => detail_lines(screen, store, &row, pane_width(area), Timestamp::now()),
        None => vec![quiet("Nothing selected")],
    };
    render_pane(
        frame,
        shell,
        area,
        focused,
        &mut screen.details_scroll,
        lines,
    );
}

/// Everything the pane says about one secret, top to bottom.
fn detail_lines(
    screen: &SecretsScreen,
    store: &Store,
    row: &SecretRow,
    width: u16,
    now: Timestamp,
) -> Vec<Line<'static>> {
    let palette = theme();
    let versions = store.versions.get(&(row.vault.clone(), row.name.clone()));
    let mut lines = vec![title(row.name.clone())];

    let mut said = vec![row.vault.clone(), "secret".to_owned()];
    said.push(if row.enabled { "enabled" } else { "disabled" }.to_owned());
    if let Some(Ok(versions)) = versions {
        said.push(format!("{} versions", versions.len()));
    }
    let parts: Vec<&str> = said.iter().map(String::as_str).collect();
    let mut subtitle = super::details::subtitle(&parts);
    if !row.enabled {
        // "disabled" is the one word in that line worth a colour.
        subtitle = Line::from(
            subtitle
                .spans
                .into_iter()
                .map(|span| span.style(Style::default().fg(palette.error)))
                .collect::<Vec<_>>(),
        );
    }
    lines.push(subtitle);
    lines.push(Line::from(""));
    lines.extend(value_lines(screen, row, width));
    lines.push(field(
        "Content type",
        row.content_type.clone().unwrap_or_else(|| "—".to_owned()),
    ));
    lines.push(stamp_line("Expires", row.expires, now, true));
    lines.push(stamp_line("Not before", row.not_before, now, false));
    lines.push(stamp_line("Created", row.created, now, false));
    lines.push(stamp_line("Updated", row.updated, now, false));
    if !row.tags.is_empty() {
        let mut spans = vec![Span::styled(
            format!("{:<LABEL$}", "Tags"),
            Style::default().fg(palette.muted),
        )];
        for (at, (key, value)) in row.tags.iter().enumerate() {
            if at > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(chip(key, value));
        }
        lines.push(Line::from(spans));
    }
    if row.managed {
        lines.push(field("Managed", "yes — a certificate's backing secret"));
    }
    if let Some(vault) = store.vault(&row.vault) {
        lines.push(link_field(
            "Id",
            format!("{}secrets/{}", vault.uri, row.name),
            width,
        ));
    }

    lines.push(Line::from(""));
    lines.push(section("Versions", width));
    match versions {
        None => lines.push(quiet("reading…")),
        Some(Err(message)) => lines.push(refused(message.clone())),
        Some(Ok(versions)) if versions.is_empty() => lines.push(quiet("none")),
        Some(Ok(versions)) => {
            let current = screen.revealed_here(row).map(|held| held.version.clone());
            for version in versions {
                let short: String = version.version.chars().take(8).collect();
                let mut said = format!(
                    "{short}…  {:<5} {}",
                    age(version.created, now),
                    if version.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    }
                );
                if current.as_deref() == Some(version.version.as_str()) {
                    said.push_str("  current");
                }
                lines.push(Line::from(Span::styled(
                    said,
                    Style::default().fg(if version.enabled {
                        palette.body
                    } else {
                        palette.muted
                    }),
                )));
            }
        }
    }
    lines
}

/// The Value line, and whatever has to go under it.
fn value_lines(screen: &SecretsScreen, row: &SecretRow, width: u16) -> Vec<Line<'static>> {
    let palette = theme();
    let mut lines = Vec::new();
    if let Some(held) = screen.revealed_here(row) {
        let value = held.expose();
        // A multi-line value — a PEM, a JSON blob — shows its first line and
        // says how much more there is; `y` copies all of it.
        let first = value.lines().next().unwrap_or("");
        let extra = held.line_count().saturating_sub(1);
        let shown = if extra > 0 {
            format!("{first}  (+{extra} lines)")
        } else {
            first.to_owned()
        };
        lines.push(with_hint(
            coloured_field("Value", shown, palette.text),
            &format!("clears in {}s", held.clears_in(std::time::Instant::now())),
            width,
        ));
        return lines;
    }
    if let Some(refusal) = screen.refusal() {
        lines.push(coloured_field("Value", refusal.clone(), palette.error));
        return lines;
    }
    if screen.is_reading(row) {
        lines.push(coloured_field("Value", "reading…", palette.muted));
        return lines;
    }
    lines.push(with_hint(
        coloured_field("Value", MASK, palette.muted),
        "v · y",
        width,
    ));
    lines
}

/// `Expires   2027-01-01 · in 12d`, coloured when it is worth a colour.
fn stamp_line(
    label: &str,
    stamp: Option<Timestamp>,
    now: Timestamp,
    expiry: bool,
) -> Line<'static> {
    let palette = theme();
    let Some(stamp) = stamp else {
        return coloured_field(label, "—", palette.muted);
    };
    let age = stamp.relative_age(now);
    if !expiry {
        return field(label, format!("{} · {age}", stamp.calendar_date()));
    }
    match Expiry::of(Some(stamp), now) {
        Expiry::Expired => coloured_field(
            label,
            format!("{} · expired {age} ago", stamp.calendar_date()),
            palette.error,
        ),
        Expiry::Soon => coloured_field(
            label,
            format!("{} · in {age}", stamp.calendar_date()),
            palette.warning,
        ),
        _ => field(label, format!("{} · in {age}", stamp.calendar_date())),
    }
}

/// One row's cells, in the order the visible columns are in.
fn row_cells(
    row: &SecretRow,
    columns: &[ColumnConfig],
    store: &Store,
    highlighter: &mut Query,
    now: Timestamp,
) -> Vec<Cell> {
    let palette = theme();
    // A vault whose last read failed shows yesterday's rows; saying so in
    // one colour is cheaper to read than a column that says "stale".
    let stale = store.stale.contains(&row.vault);
    let base = if stale {
        Style::default().fg(palette.muted)
    } else {
        Style::default()
    };
    columns
        .iter()
        .map(|column| match column.id {
            ColumnId::Vault => {
                Cell::styled(row.vault.clone(), base).matched(highlighter.indices(&row.vault))
            }
            ColumnId::Name => {
                Cell::styled(row.name.clone(), base).matched(highlighter.indices(&row.name))
            }
            ColumnId::Enabled => Cell::styled(
                if row.enabled { "✓" } else { "✗" }.to_owned(),
                if row.enabled {
                    base
                } else {
                    Style::default().fg(palette.muted)
                },
            ),
            ColumnId::Expires => {
                let expiry = Expiry::of(row.expires, now);
                let style = match expiry {
                    Expiry::Expired if !stale => Style::default().fg(palette.error),
                    Expiry::Soon if !stale => Style::default().fg(palette.warning),
                    _ => base,
                };
                Cell::styled(expiry.cell(row.expires, now), style)
            }
            ColumnId::Updated => Cell::styled(age(row.updated, now), base),
            ColumnId::Created => Cell::styled(age(row.created, now), base),
            ColumnId::Type => Cell::styled(
                row.content_type.clone().unwrap_or_else(|| "—".to_owned()),
                base,
            ),
            // The Registries tab's columns never reach this table.
            _ => Cell::styled(String::new(), base),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::shell::Shell;
    use crate::azure::{Inventory, Vault};
    use crate::worker::Event;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn vault(name: &str) -> Vault {
        Vault {
            id: format!("/vaults/{name}"),
            name: name.to_owned(),
            resource_group: "rg".into(),
            location: "eastus".into(),
            uri: format!("https://{name}.vault.azure.net/"),
        }
    }

    fn secret(vault: &str, name: &str) -> SecretRow {
        SecretRow {
            vault: vault.to_owned(),
            name: name.to_owned(),
            enabled: true,
            created: None,
            updated: None,
            expires: None,
            not_before: None,
            content_type: None,
            tags: Vec::new(),
            managed: false,
        }
    }

    fn stocked() -> Store {
        let mut store = Store::default();
        store.apply(Event::Inventory(Ok(Inventory {
            vaults: vec![vault("kv-dev"), vault("kv-prod")],
            registries: Vec::new(),
        })));
        store.apply(Event::Secrets {
            vault: "kv-dev".into(),
            result: Ok(vec![
                secret("kv-dev", "db-password"),
                secret("kv-dev", "api-key"),
            ]),
        });
        store.apply(Event::Secrets {
            vault: "kv-prod".into(),
            result: Ok(vec![secret("kv-prod", "db-password")]),
        });
        store
    }

    fn draw(width: u16, height: u16, screen: &mut SecretsScreen, store: &Store) -> (String, Shell) {
        let mut shell = Shell::default();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                shell.begin_frame();
                screen.refilter(store);
                render(frame, &mut shell, screen, store, frame.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        (text, shell)
    }

    #[test]
    fn the_table_shows_every_vaults_rows_with_one_secret_adjacent() {
        let store = stocked();
        let mut screen = SecretsScreen::default();
        let (drawn, _) = draw(120, 14, &mut screen, &store);
        assert!(drawn.contains("Vault"), "{drawn}");
        assert!(drawn.contains("Name"), "{drawn}");
        assert!(drawn.contains("3 · Name ↑"), "{drawn}");
        let lines: Vec<&str> = drawn.lines().collect();
        let db = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.contains("db-password"))
            .map(|(at, _)| at)
            .collect::<Vec<_>>();
        assert_eq!(db.len(), 2);
        assert_eq!(db[1], db[0] + 1, "dev and prod on adjacent lines: {drawn}");
    }

    #[test]
    fn a_query_narrows_the_table_and_the_border_counts_what_is_left() {
        let store = stocked();
        let mut screen = SecretsScreen::default();
        screen.input.set_text("api");
        let (drawn, _) = draw(120, 14, &mut screen, &store);
        assert!(drawn.contains("1/3 · Name ↑"), "{drawn}");
        assert!(drawn.contains("api-key"), "{drawn}");
        assert!(!drawn.contains("db-password"), "{drawn}");
    }

    #[test]
    fn a_stale_vaults_rows_are_painted_muted() {
        let mut store = stocked();
        store.apply(Event::Secrets {
            vault: "kv-prod".into(),
            result: Err("no permission".into()),
        });
        let mut screen = SecretsScreen::default();
        let mut shell = Shell::default();
        let mut terminal = Terminal::new(TestBackend::new(120, 14)).unwrap();
        terminal
            .draw(|frame| {
                shell.begin_frame();
                screen.refilter(&store);
                render(frame, &mut shell, &mut screen, &store, frame.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let muted = theme().muted;
        let row = (0..buffer.area.height)
            .find(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("kv-prod")
            })
            .expect("a kv-prod row");
        let painted: Vec<_> = (3..12).map(|x| buffer[(x, row)].fg).collect();
        assert!(
            painted.contains(&muted),
            "a vault that would not answer reads as stale: {painted:?}"
        );
    }

    #[test]
    fn a_click_lands_on_the_row_and_the_header_under_it() {
        let store = stocked();
        let mut screen = SecretsScreen::default();
        let (_, shell) = draw(120, 14, &mut screen, &store);
        // Row 0 is two lines below the search row and the header.
        let row = (0..14)
            .find_map(|y| match shell.hit(4, y) {
                Some(Target::Row(index)) => Some((y, *index)),
                _ => None,
            })
            .expect("a row region");
        assert_eq!(row.1, 0);
        let header = (0..14)
            .find_map(|y| match shell.hit(4, y) {
                Some(Target::Header(column)) => Some(*column),
                _ => None,
            })
            .expect("a header region");
        assert_eq!(header, ColumnId::Vault);
    }

    #[test]
    fn the_details_pane_masks_the_value_until_it_is_revealed() {
        let store = stocked();
        let mut screen = SecretsScreen::default();
        let (drawn, _) = draw(120, 20, &mut screen, &store);
        assert!(drawn.contains("Details"), "{drawn}");
        assert!(drawn.contains(MASK), "eight dots, always: {drawn}");
        assert!(drawn.contains("v · y"), "{drawn}");
        assert!(drawn.contains("Content type"), "{drawn}");
        assert!(drawn.contains("── Versions"), "{drawn}");

        // Reveal it, and the dots give way to the value and a countdown.
        let mut shell = Shell::default();
        screen.refilter(&store);
        screen.handle_key(
            &mut shell,
            &store,
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('v'),
                crossterm::event::KeyModifiers::NONE,
            ),
        );
        screen.on_value(
            &mut shell,
            &store,
            "kv-dev",
            "api-key",
            Ok((
                crate::azure::Secret::new("s3cr3t-value"),
                "8f3a2c1d".to_owned(),
            )),
            std::time::Instant::now(),
        );
        let (drawn, _) = draw(120, 20, &mut screen, &store);
        assert!(drawn.contains("s3cr3t-value"), "{drawn}");
        assert!(!drawn.contains(MASK), "{drawn}");
        assert!(drawn.contains("clears in"), "{drawn}");
    }

    #[test]
    fn a_refusal_takes_the_value_lines_place_and_reads_as_an_error() {
        let store = stocked();
        let mut screen = SecretsScreen::default();
        let mut shell = Shell::default();
        screen.refilter(&store);
        screen.handle_key(
            &mut shell,
            &store,
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('v'),
                crossterm::event::KeyModifiers::NONE,
            ),
        );
        screen.on_value(
            &mut shell,
            &store,
            "kv-dev",
            "api-key",
            Err("kv-dev: no permission to read secrets".to_owned()),
            std::time::Instant::now(),
        );
        let (drawn, _) = draw(120, 20, &mut screen, &store);
        assert!(drawn.contains("no permission to read secrets"), "{drawn}");
        assert!(!drawn.contains(MASK), "{drawn}");
        assert!(
            drawn.contains("api-key"),
            "the table is otherwise intact: {drawn}"
        );
    }

    #[test]
    fn a_versions_read_that_failed_says_so_rather_than_reading_for_ever() {
        let mut store = stocked();
        store.apply(Event::Versions {
            vault: "kv-dev".into(),
            name: "api-key".into(),
            result: Err("kv-dev: blocked by the vault firewall".into()),
        });
        let mut screen = SecretsScreen::default();
        let (drawn, _) = draw(120, 20, &mut screen, &store);
        assert!(drawn.contains("blocked by the vault firewall"), "{drawn}");
        assert!(!drawn.contains("reading…"), "{drawn}");
    }

    #[test]
    fn under_seventy_columns_the_details_pane_waits_for_tab() {
        let store = stocked();
        let mut screen = SecretsScreen::default();
        let (drawn, _) = draw(60, 16, &mut screen, &store);
        assert!(
            !drawn.contains("Details"),
            "the table gets the room: {drawn}"
        );

        let mut shell = Shell::default();
        shell.focus = crate::app::shell::Focus::Details;
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal
            .draw(|frame| {
                shell.begin_frame();
                screen.refilter(&store);
                render(frame, &mut shell, &mut screen, &store, frame.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let drawn: String = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(drawn.contains("Details"), "{drawn}");
        assert!(
            !drawn.contains("Enabled"),
            "and the table steps aside: {drawn}"
        );
    }

    #[test]
    fn a_narrow_table_keeps_the_vault_and_the_name() {
        let store = stocked();
        let mut screen = SecretsScreen::default();
        let (drawn, _) = draw(46, 12, &mut screen, &store);
        assert!(drawn.contains("Vault"), "{drawn}");
        assert!(drawn.contains("Name"), "{drawn}");
        assert!(drawn.contains("db-password"), "{drawn}");
    }
}
