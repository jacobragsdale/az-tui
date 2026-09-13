//! The Registries tab's panes: the search row, the table — repositories, or
//! one repository's tags — and the details pane beside it.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::details::{
    field, link_field, pane_width, quiet, refused, render_pane, section, subtitle, title, with_hint,
};
use super::secrets::env_cell;
use super::table::{Cell, TableSpec, render_list_table, table_geometry};
use super::theme::theme;
use super::widgets::{
    Pane, REPOSITORIES_PLACEHOLDER, TAGS_PLACEHOLDER, render_panes, render_scrollbar,
};
use crate::app::registries::{
    Level, REPOSITORY_SCHEMA, RegistriesScreen, TAG_SCHEMA, TAGS_IN_PANE,
};
use crate::app::screen::Target;
use crate::app::shell::{Focus, Shell};
use crate::azure::acr::{human_size, short_digest};
use crate::columns::{ColumnConfig, ColumnId, TableLayout};
use crate::search::Query;
use crate::store::Store;
use crate::timestamp::{Timestamp, age};

pub fn render(
    frame: &mut Frame,
    shell: &mut Shell,
    screen: &mut RegistriesScreen,
    store: &Store,
    area: Rect,
) {
    // Each level keeps its own box, so going down and back restores the
    // query that was typed at each — and each names its own grammar.
    let input = screen.input().clone();
    let placeholder = match screen.level {
        Level::Repositories => REPOSITORIES_PLACEHOLDER,
        Level::Tags { .. } => TAGS_PLACEHOLDER,
    };
    render_panes(
        frame,
        shell,
        area,
        &input,
        placeholder,
        |frame, shell, pane, rect| match pane {
            Pane::Table => render_table(frame, shell, screen, store, rect),
            Pane::Details => render_details(frame, shell, screen, store, rect),
        },
    );
}

fn render_table(
    frame: &mut Frame,
    shell: &mut Shell,
    screen: &mut RegistriesScreen,
    store: &Store,
    area: Rect,
) {
    let geometry = table_geometry(area);
    let available = TableLayout::available_width(geometry.inner.width);
    screen.note_width(available);
    let now = Timestamp::now();
    let columns = screen.table().layout.visible_columns(available);
    // Parsed with the level's own schema, so a `registry:` filter is a field
    // here as it was in the filter, not a word lit up nowhere.
    let schema = match screen.level {
        Level::Repositories => REPOSITORY_SCHEMA,
        Level::Tags { .. } => TAG_SCHEMA,
    };
    let mut highlighter =
        Query::new(&crate::filter::Query::parse(screen.input().text(), schema).words);

    let title = screen.title();
    let status = screen.status(store);
    let total = screen.count();
    let sorted = Some((
        screen.table().sort,
        if screen.table().descending {
            "↓"
        } else {
            "↑"
        },
    ));

    // The rows on screen, built from whichever level is showing.
    let rows: Vec<Vec<Cell>> = match screen.level.clone() {
        Level::Repositories => {
            let window = geometry.window(&mut screen.repositories.cursor, total);
            screen.visible()[window]
                .iter()
                .map(|at| {
                    repository_cells(
                        &store.repositories[*at],
                        &columns,
                        store,
                        &mut highlighter,
                        now,
                    )
                })
                .collect()
        }
        Level::Tags { .. } => {
            let window = geometry.window(&mut screen.tags.cursor, total);
            match screen.tags_of(store) {
                Some(Ok(tags)) => screen.tag_visible()[window]
                    .iter()
                    .map(|at| tag_cells(&tags[*at], &columns, &mut highlighter, now))
                    .collect(),
                // Nothing read yet, or it refused; the details pane says
                // which, and the table is simply empty.
                _ => Vec::new(),
            }
        }
    };

    let first = screen.table().cursor.scroll.offset;
    let focused = shell.focus == Focus::Table;
    let cursor = &mut screen.table_mut().cursor;
    let mut spec = TableSpec {
        title,
        status,
        focused,
        columns: &columns,
        sorted,
        rows: &rows,
        total,
        cursor,
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

fn repository_cells(
    row: &crate::azure::Repository,
    columns: &[ColumnConfig],
    store: &Store,
    highlighter: &mut Query,
    now: Timestamp,
) -> Vec<Cell> {
    let palette = theme();
    let stale = store.stale.contains(&row.registry);
    let base = if stale {
        Style::default().fg(palette.muted)
    } else {
        Style::default()
    };
    // A count the attributes walk has not reached yet reads as a pause, not
    // as a zero.
    let filling = Style::default().fg(palette.muted);
    columns
        .iter()
        .map(|column| match column.id {
            ColumnId::Env => env_cell(&row.registry, base, highlighter),
            ColumnId::Registry => {
                Cell::styled(row.registry.clone(), base).matched(highlighter.indices(&row.registry))
            }
            ColumnId::Repository => {
                Cell::styled(row.name.clone(), base).matched(highlighter.indices(&row.name))
            }
            ColumnId::Tags => count_cell(row.tag_count, base, filling),
            ColumnId::Manifests => count_cell(row.manifest_count, base, filling),
            ColumnId::Updated => stamp_cell(row.updated, base, filling, now),
            ColumnId::Created => stamp_cell(row.created, base, filling, now),
            _ => Cell::styled(String::new(), base),
        })
        .collect()
}

fn tag_cells(
    tag: &crate::azure::Tag,
    columns: &[ColumnConfig],
    highlighter: &mut Query,
    now: Timestamp,
) -> Vec<Cell> {
    let base = Style::default();
    columns
        .iter()
        .map(|column| match column.id {
            ColumnId::Tag => {
                Cell::styled(tag.name.clone(), base).matched(highlighter.indices(&tag.name))
            }
            ColumnId::Digest => Cell::styled(short_digest(&tag.digest), base),
            ColumnId::Updated => stamp_cell(tag.updated, base, base, now),
            ColumnId::Created => stamp_cell(tag.created, base, base, now),
            _ => Cell::styled(String::new(), base),
        })
        .collect()
}

fn count_cell(count: Option<u64>, base: Style, filling: Style) -> Cell {
    count.map_or_else(
        || Cell::styled("…".to_owned(), filling),
        |count| Cell::styled(count.to_string(), base),
    )
}

fn stamp_cell(stamp: Option<Timestamp>, base: Style, filling: Style, now: Timestamp) -> Cell {
    stamp.map_or_else(
        || Cell::styled("…".to_owned(), filling),
        |stamp| Cell::styled(stamp.relative_age(now), base),
    )
}

fn render_details(
    frame: &mut Frame,
    shell: &mut Shell,
    screen: &mut RegistriesScreen,
    store: &Store,
    area: Rect,
) {
    let focused = shell.focus == Focus::Details;
    let width = pane_width(area);
    let lines = match screen.level.clone() {
        Level::Repositories => repository_lines(screen, store, width),
        Level::Tags { registry, repo } => tag_lines(screen, store, &registry, &repo, width),
    };
    let lines = lines.unwrap_or_else(|| vec![quiet(nothing_to_show(screen, store))]);
    render_pane(
        frame,
        shell,
        area,
        focused,
        &mut screen.details_scroll,
        lines,
    );
}

/// Why the pane has nothing to say. At the tag level that is nearly always
/// "the tags have not landed", which is worth saying rather than leaving a
/// pane that reads as broken.
fn nothing_to_show(screen: &RegistriesScreen, store: &Store) -> String {
    match screen.level {
        Level::Repositories => "Nothing selected".to_owned(),
        Level::Tags { .. } => match screen.tags_of(store) {
            None => "reading the tags…".to_owned(),
            Some(Err(message)) => message.clone(),
            Some(Ok(tags)) if tags.is_empty() => "this repository has no tags".to_owned(),
            Some(Ok(_)) => "no tag matches the query".to_owned(),
        },
    }
}

fn repository_lines(
    screen: &RegistriesScreen,
    store: &Store,
    width: u16,
) -> Option<Vec<Line<'static>>> {
    let palette = theme();
    let row = screen.selected_repository(store)?;
    let registry = store.registry(&row.registry);
    let login = registry.map_or(row.registry.as_str(), |held| held.login_server.as_str());
    let now = Timestamp::now();

    let mut said = vec![login.to_owned()];
    if let Some(tags) = row.tag_count {
        said.push(format!("{tags} tags"));
    }
    if let Some(manifests) = row.manifest_count {
        said.push(format!("{manifests} manifests"));
    }
    if let Some(updated) = row.updated {
        said.push(format!("updated {}", updated.relative_age(now)));
    }
    if let Some(created) = row.created {
        said.push(format!("created {}", created.relative_age(now)));
    }
    let parts: Vec<&str> = said.iter().map(String::as_str).collect();

    let mut lines = vec![title(row.name.clone()), subtitle(&parts), Line::from("")];
    lines.push(with_hint(
        field("Pull", format!("{login}/{}", row.name)),
        "y",
        width,
    ));
    if let Some(registry) = registry {
        lines.push(link_field(
            "Portal",
            crate::azure::portal_url(&registry.id),
            width,
        ));
    }
    lines.push(Line::from(""));
    lines.push(section("Tags", width));

    match store.tags.get(&(row.registry.clone(), row.name.clone())) {
        None => lines.push(quiet("reading…")),
        Some(Err(message)) => lines.push(refused(message.clone())),
        Some(Ok(tags)) if tags.is_empty() => lines.push(quiet("none")),
        Some(Ok(tags)) => {
            for tag in tags.iter().take(TAGS_IN_PANE) {
                lines.push(Line::from(Span::styled(
                    format!(
                        "{:<16} {:<20} {}",
                        tag.name,
                        short_digest(&tag.digest),
                        age(tag.updated, now)
                    ),
                    Style::default().fg(palette.body),
                )));
            }
            if tags.len() > TAGS_IN_PANE {
                lines.push(quiet(format!(
                    "and {} more — Enter",
                    tags.len() - TAGS_IN_PANE
                )));
            }
        }
    }
    Some(lines)
}

fn tag_lines(
    screen: &RegistriesScreen,
    store: &Store,
    registry: &str,
    repo: &str,
    width: u16,
) -> Option<Vec<Line<'static>>> {
    let palette = theme();
    let tag = screen.selected_tag(store)?;
    let login = store
        .registry(registry)
        .map_or(registry, |held| held.login_server.as_str());
    let now = Timestamp::now();

    let mut lines = vec![
        title(format!("{repo}:{}", tag.name)),
        subtitle(&[login]),
        Line::from(""),
        with_hint(
            field(
                "Pull",
                crate::azure::acr::pull_reference(login, repo, &tag.name),
            ),
            "y",
            width,
        ),
        with_hint(
            field(
                "Digest",
                crate::azure::acr::digest_reference(login, repo, &tag.digest),
            ),
            "Y",
            width,
        ),
    ];

    match store
        .manifests
        .get(&(registry.to_owned(), repo.to_owned(), tag.digest.clone()))
    {
        None => lines.push(field("Size", "reading…")),
        Some(Err(message)) => lines.push(super::details::coloured_field(
            "Size",
            message.clone(),
            palette.error,
        )),
        Some(Ok(manifest)) => {
            // A multi-arch index names no architecture; what it is is still
            // worth saying.
            let platform = match (&manifest.os, &manifest.architecture) {
                (Some(os), Some(architecture)) => format!("{os}/{architecture}"),
                _ => "index".to_owned(),
            };
            let size = manifest.size.map_or_else(|| "—".to_owned(), human_size);
            lines.push(field("Size", format!("{size} · {platform}")));
            if manifest.tags.len() > 1 {
                let others: Vec<&str> = manifest
                    .tags
                    .iter()
                    .map(String::as_str)
                    .filter(|held| *held != tag.name)
                    .collect();
                if !others.is_empty() {
                    lines.push(field("Also tags", others.join(", ")));
                }
            }
        }
    }
    lines.push(field(
        "Created",
        tag.created.map_or_else(
            || "—".to_owned(),
            |at| format!("{} · {}", at.calendar_date(), at.relative_age(now)),
        ),
    ));
    lines.push(field(
        "Updated",
        tag.updated.map_or_else(
            || "—".to_owned(),
            |at| format!("{} · {}", at.calendar_date(), at.relative_age(now)),
        ),
    ));
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::registries::RegistriesScreen;
    use crate::azure::{Inventory, Manifest, Registry, Repository, Tag};
    use crate::timestamp::ts;
    use crate::worker::Event;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn stocked() -> Store {
        let registry = |name: &str| Registry {
            id: format!("/registries/{name}"),
            name: name.to_owned(),
            resource_group: "rg".into(),
            location: "eastus".into(),
            login_server: format!("{name}.azurecr.io"),
        };
        let mut store = Store::default();
        store.apply(Event::Inventory(Ok(Inventory {
            vaults: Vec::new(),
            registries: vec![registry("acrprod")],
        })));
        store.apply(Event::Repositories {
            registry: "acrprod".into(),
            result: Ok(vec![
                Repository {
                    registry: "acrprod".into(),
                    name: "payments-api".into(),
                    tag_count: Some(48),
                    manifest_count: Some(51),
                    created: Some(ts("2025-09-11T18:00:00Z")),
                    updated: Some(ts("2026-09-11T18:00:00Z")),
                },
                Repository {
                    registry: "acrprod".into(),
                    name: "notifications".into(),
                    tag_count: None,
                    manifest_count: None,
                    created: None,
                    updated: None,
                },
            ]),
        });
        store.apply(Event::Tags {
            registry: "acrprod".into(),
            repo: "payments-api".into(),
            result: Ok(vec![Tag {
                name: "1.42.0".into(),
                digest: "sha256:ab12ef0199".into(),
                created: Some(ts("2026-09-11T18:00:00Z")),
                updated: Some(ts("2026-09-11T18:00:00Z")),
            }]),
        });
        store
    }

    fn draw(width: u16, height: u16, screen: &mut RegistriesScreen, store: &Store) -> String {
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
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn open(screen: &mut RegistriesScreen, store: &Store) {
        let mut shell = Shell::default();
        screen.refilter(store);
        screen.handle_key(
            &mut shell,
            store,
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
        );
        screen.refilter(store);
    }

    #[test]
    fn the_repository_table_says_which_counts_are_still_filling_in() {
        let store = stocked();
        let mut screen = RegistriesScreen::default();
        let drawn = draw(120, 20, &mut screen, &store);
        assert!(drawn.contains("Repositories"), "{drawn}");
        assert!(drawn.contains("payments-api"), "{drawn}");
        assert!(drawn.contains('…'), "a count not yet read: {drawn}");
        assert!(drawn.contains("filling 1/2"), "{drawn}");
        assert!(
            drawn.contains("acrprod.azurecr.io/payments-api"),
            "the pull line: {drawn}"
        );
        assert!(drawn.contains("── Tags"), "{drawn}");
        assert!(drawn.contains("1.42.0"), "{drawn}");
    }

    #[test]
    fn the_tag_level_names_the_repository_and_shows_its_digest() {
        let store = stocked();
        let mut screen = RegistriesScreen::default();
        open(&mut screen, &store);
        let drawn = draw(120, 20, &mut screen, &store);
        assert!(drawn.contains("payments-api · acrprod"), "{drawn}");
        assert!(
            drawn.contains("sha256:ab12ef01"),
            "the short digest: {drawn}"
        );
        assert!(
            drawn.contains("acrprod.azurecr.io/payments-api@sha256:ab12ef0199"),
            "the full one in the pane: {drawn}"
        );
        assert!(
            drawn.contains("reading…"),
            "the manifest is not in yet: {drawn}"
        );
    }

    #[test]
    fn a_multi_arch_index_says_index_where_a_platform_would_go() {
        let mut store = stocked();
        store.apply(Event::Manifest {
            registry: "acrprod".into(),
            repo: "payments-api".into(),
            digest: "sha256:ab12ef0199".into(),
            result: Ok(Manifest {
                digest: "sha256:ab12ef0199".into(),
                size: Some(84_200_000),
                architecture: None,
                os: None,
                created: Some(ts("2026-09-11T18:00:00Z")),
                tags: vec!["1.42.0".into(), "1.42".into(), "stable".into()],
            }),
        });
        let mut screen = RegistriesScreen::default();
        open(&mut screen, &store);
        let drawn = draw(120, 20, &mut screen, &store);
        assert!(drawn.contains("84.2 MB · index"), "{drawn}");
        assert!(
            drawn.contains("1.42, stable"),
            "the manifest's other tags: {drawn}"
        );

        store.apply(Event::Manifest {
            registry: "acrprod".into(),
            repo: "payments-api".into(),
            digest: "sha256:ab12ef0199".into(),
            result: Ok(Manifest {
                digest: "sha256:ab12ef0199".into(),
                size: Some(84_200_000),
                architecture: Some("amd64".into()),
                os: Some("linux".into()),
                created: None,
                tags: vec!["1.42.0".into()],
            }),
        });
        let drawn = draw(120, 20, &mut screen, &store);
        assert!(drawn.contains("84.2 MB · linux/amd64"), "{drawn}");
    }

    #[test]
    fn a_stale_registrys_rows_are_painted_muted() {
        let mut store = stocked();
        store.apply(Event::Repositories {
            registry: "acrprod".into(),
            result: Err("acrprod: no permission".into()),
        });
        let mut screen = RegistriesScreen::default();
        let mut shell = Shell::default();
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal
            .draw(|frame| {
                shell.begin_frame();
                screen.refilter(&store);
                render(frame, &mut shell, &mut screen, &store, frame.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let muted = theme().muted;
        // Only the table half: the details pane's title is on the same
        // terminal row as the table's header, and matching that would have
        // been asserting about the header's colour.
        let row = (0..20)
            .find(|y| {
                (0..60)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("payments-api")
            })
            .expect("a row");
        let painted: Vec<_> = (3..12).map(|x| buffer[(x, row)].fg).collect();
        assert!(painted.contains(&muted), "{painted:?}");
    }

    #[test]
    fn a_tag_level_with_nothing_in_it_says_why_rather_than_looking_broken() {
        let mut store = stocked();
        store.apply(Event::Tags {
            registry: "acrprod".into(),
            repo: "payments-api".into(),
            result: Err("acrprod.azurecr.io: no permission (needs AcrPull)".into()),
        });
        let mut screen = RegistriesScreen::default();
        open(&mut screen, &store);
        let drawn = draw(120, 14, &mut screen, &store);
        assert!(drawn.contains("no permission (needs AcrPull)"), "{drawn}");
        assert!(!drawn.contains("Nothing selected"), "{drawn}");
    }
}
