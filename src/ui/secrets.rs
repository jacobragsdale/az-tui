//! The Secrets tab's panes: the search row and the table under it.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;

use super::table::{Cell, TableSpec, render_list_table, table_geometry};
use super::theme::theme;
use super::widgets::{render_scrollbar, render_search_row};
use crate::app::screen::Target;
use crate::app::secrets::{Expiry, SecretsScreen};
use crate::app::shell::{Focus, Shell};
use crate::azure::SecretRow;
use crate::columns::{ColumnConfig, ColumnId, TableLayout};
use crate::search::Highlighter;
use crate::store::Store;
use crate::timestamp::Timestamp;

/// Draws the tab: one row for the search box, the table under it.
pub fn render(
    frame: &mut Frame,
    shell: &mut Shell,
    screen: &mut SecretsScreen,
    store: &Store,
    area: Rect,
) {
    let [search, table] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3)])
        .areas(area);
    render_search_row(
        frame,
        shell,
        search,
        &screen.input,
        shell.focus == Focus::Search,
    );
    render_table(frame, shell, screen, store, table);
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
    let mut highlighter = Highlighter::new(
        &crate::filter::Query::parse(screen.input.text(), crate::app::secrets::SCHEMA).words,
    );

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
        shell.region(rect, Target::Header(column.key()));
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

/// One row's cells, in the order the visible columns are in.
fn row_cells(
    row: &SecretRow,
    columns: &[ColumnConfig],
    store: &Store,
    highlighter: &mut Highlighter,
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

fn age(stamp: Option<Timestamp>, now: Timestamp) -> String {
    stamp.map_or_else(|| "—".to_owned(), |stamp| stamp.relative_age(now))
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
            subscription_id: "s".into(),
            resource_group: "rg".into(),
            location: "eastus".into(),
            sku: "standard".into(),
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
                Some(Target::Header(key)) => Some(*key),
                _ => None,
            })
            .expect("a header region");
        assert_eq!(header, "vault");
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
