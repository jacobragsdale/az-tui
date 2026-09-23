use super::*;
use crate::azure::{Inventory, Registry};
use crate::timestamp::ts;
use crate::worker::Event;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn registry(name: &str) -> Registry {
    Registry {
        id: format!("/registries/{name}"),
        name: name.to_owned(),
        resource_group: "rg".into(),
        location: "eastus".into(),
        login_server: format!("{name}.azurecr.io"),
    }
}

fn repository(registry: &str, name: &str, tags: Option<u64>, updated: Option<&str>) -> Repository {
    Repository {
        registry: registry.to_owned(),
        name: name.to_owned(),
        tag_count: tags,
        manifest_count: tags,
        created: None,
        updated: updated.map(ts),
    }
}

fn tag(name: &str, digest: &str, updated: &str) -> Tag {
    Tag {
        name: name.to_owned(),
        digest: digest.to_owned(),
        created: Some(ts(updated)),
        updated: Some(ts(updated)),
    }
}

pub(crate) fn stocked() -> AzureStore {
    let mut store = AzureStore::default();
    store.apply(Event::Inventory(Ok(Inventory {
        vaults: Vec::new(),
        registries: vec![registry("acrdev"), registry("acrprod")],
    })));
    store.apply(Event::Repositories {
        registry: "acrprod".into(),
        result: Ok(vec![
            repository(
                "acrprod",
                "payments-api",
                Some(48),
                Some("2026-09-11T18:00:00Z"),
            ),
            repository(
                "acrprod",
                "web-frontend",
                Some(12),
                Some("2026-09-01T18:00:00Z"),
            ),
            // Still filling: no counts and no stamps yet.
            repository("acrprod", "notifications", None, None),
        ]),
    });
    store.apply(Event::Tags {
        registry: "acrprod".into(),
        repo: "payments-api".into(),
        result: Ok(vec![
            tag("1.42.0", "sha256:ab12ef0199", "2026-09-11T18:00:00Z"),
            tag("1.41.3", "sha256:9f01aa4288", "2026-09-08T18:00:00Z"),
        ]),
    });
    store
}

fn press(screen: &mut RegistriesScreen, store: &AzureStore, code: KeyCode) -> AppAction {
    let mut shell = Shell::default();
    screen.refilter(store);
    let action = screen.handle_key(&mut shell, store, KeyEvent::new(code, KeyModifiers::NONE));
    screen.refilter(store);
    action
}

fn shown(screen: &RegistriesScreen, store: &AzureStore) -> Vec<String> {
    screen
        .visible()
        .iter()
        .map(|at| store.repositories[*at].name.clone())
        .collect()
}

#[test]
fn enter_opens_a_repository_and_backspace_comes_back_with_both_cursors_intact() {
    let store = stocked();
    let mut screen = RegistriesScreen::default();
    screen.refilter(&store);
    // Updated descending, so the newest repository is first and the one
    // with no stamp yet is last.
    assert_eq!(
        shown(&screen, &store),
        ["payments-api", "web-frontend", "notifications"]
    );

    let action = press(&mut screen, &store, KeyCode::Enter);
    assert!(
        matches!(&action, AppAction::Azure(Request::Tags { repo, .. }) if repo == "payments-api"),
        "{action:?}"
    );
    assert_eq!(screen.open_repository(), Some(("acrprod", "payments-api")));
    assert_eq!(screen.count(), 2, "the two tags");

    press(&mut screen, &store, KeyCode::Char('j'));
    assert_eq!(screen.selected_tag(&store).unwrap().name, "1.41.3");

    press(&mut screen, &store, KeyCode::Char('h'));
    assert_eq!(screen.level, Level::Repositories);
    assert_eq!(
        screen.selected_repository(&store).unwrap().name,
        "payments-api",
        "the level-1 cursor is where it was"
    );

    press(&mut screen, &store, KeyCode::Enter);
    assert_eq!(
        screen.selected_tag(&store).unwrap().name,
        "1.41.3",
        "and so is the level-2 one"
    );
}

#[test]
fn each_level_keeps_its_own_query_across_the_round_trip() {
    let store = stocked();
    let mut screen = RegistriesScreen::default();
    screen.repositories.input.set_text("pay");
    screen.refilter(&store);
    assert_eq!(shown(&screen, &store), ["payments-api"]);

    press(&mut screen, &store, KeyCode::Enter);
    assert!(
        screen.input().is_empty(),
        "a level just opened has no query"
    );
    screen.tags.input.set_text("1.41");
    screen.refilter(&store);
    assert_eq!(screen.count(), 1);

    press(&mut screen, &store, KeyCode::Char('h'));
    assert_eq!(screen.input().text(), "pay", "level 1's query came back");
    press(&mut screen, &store, KeyCode::Enter);
    assert_eq!(screen.input().text(), "1.41", "and so did level 2's");
}

#[test]
fn a_count_that_has_not_arrived_sorts_last_whichever_way_the_sort_points() {
    let store = stocked();
    let mut screen = RegistriesScreen::default();
    screen.note_width(200);
    screen.refilter(&store);
    assert_eq!(shown(&screen, &store).last().unwrap(), "notifications");

    screen.repositories.descending = false;
    screen.refilter(&store);
    assert_eq!(
        shown(&screen, &store).last().unwrap(),
        "notifications",
        "flipped, and still last"
    );

    screen.repositories.sort = ColumnId::Tags;
    screen.refilter(&store);
    assert_eq!(shown(&screen, &store).last().unwrap(), "notifications");
}

#[test]
fn the_border_says_how_many_are_still_filling_in() {
    let store = stocked();
    let mut screen = RegistriesScreen::default();
    screen.refilter(&store);
    let status = screen.status(&store);
    assert!(status.contains("filling 2/3"), "{status}");

    // Once the last attributes call lands, the counter goes.
    let mut store = store;
    store.apply(Event::Repository {
        registry: "acrprod".into(),
        repository: repository(
            "acrprod",
            "notifications",
            Some(3),
            Some("2026-09-10T18:00:00Z"),
        ),
    });
    screen.invalidate();
    screen.refilter(&store);
    let status = screen.status(&store);
    assert!(!status.contains("filling"), "{status}");
    assert!(status.starts_with("3 · Updated"), "{status}");
}

#[test]
fn y_and_capital_y_produce_references_that_pull_at_each_level() {
    let store = stocked();
    let mut screen = RegistriesScreen::default();
    screen.refilter(&store);

    match press(&mut screen, &store, KeyCode::Char('y')) {
        AppAction::Copy { text, .. } => {
            assert_eq!(text, "acrprod.azurecr.io/payments-api");
        }
        other => panic!("{other:?}"),
    }

    press(&mut screen, &store, KeyCode::Enter);
    match press(&mut screen, &store, KeyCode::Char('y')) {
        AppAction::Copy { text, .. } => {
            assert_eq!(text, "acrprod.azurecr.io/payments-api:1.42.0");
        }
        other => panic!("{other:?}"),
    }
    match press(&mut screen, &store, KeyCode::Char('Y')) {
        AppAction::Copy { text, label } => {
            assert_eq!(text, "acrprod.azurecr.io/payments-api@sha256:ab12ef0199");
            assert!(label.contains("sha256:ab12ef0199"), "{label}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_repository_that_leaves_the_registry_closes_the_level_it_had_open() {
    let mut store = stocked();
    let mut screen = RegistriesScreen::default();
    screen.refilter(&store);
    press(&mut screen, &store, KeyCode::Enter);
    assert!(matches!(screen.level, Level::Tags { .. }));

    let was = screen.cursor_identity(&store);
    store.apply(Event::Repositories {
        registry: "acrprod".into(),
        result: Ok(vec![repository(
            "acrprod",
            "web-frontend",
            Some(12),
            Some("2026-09-01T18:00:00Z"),
        )]),
    });
    screen.invalidate();
    screen.keep_cursor(&store, was);
    assert_eq!(
        screen.level,
        Level::Repositories,
        "the repository it was inside is gone"
    );
}

#[test]
fn tags_are_asked_for_once_per_row_after_the_rest_interval() {
    let store = stocked();
    let mut screen = RegistriesScreen::default();
    screen.refilter(&store);
    // The cursor starts on payments-api, whose tags are already in.
    let mut clock = Instant::now();
    assert!(screen.tick(&store, clock).is_none());
    clock += REST;
    assert!(
        screen.tick(&store, clock).is_none(),
        "nothing to ask: the tags are already held"
    );

    press(&mut screen, &store, KeyCode::Char('j'));
    // The first tick after a move only notes where the cursor landed.
    assert!(screen.tick(&store, clock).is_none());
    clock += REST;
    let request = screen.tick(&store, clock);
    assert!(
        matches!(&request, Some(Request::Tags { repo, .. }) if repo == "web-frontend"),
        "{request:?}"
    );
    clock += REST;
    assert!(screen.tick(&store, clock).is_none(), "asked once per run");
}

#[test]
fn a_manifest_is_asked_for_once_per_digest_at_the_tag_level() {
    let store = stocked();
    let mut screen = RegistriesScreen::default();
    screen.refilter(&store);
    press(&mut screen, &store, KeyCode::Enter);

    let mut clock = Instant::now();
    screen.tick(&store, clock);
    clock += REST;
    let request = screen.tick(&store, clock);
    assert!(
        matches!(&request, Some(Request::Manifest { digest, .. }) if digest == "sha256:ab12ef0199"),
        "{request:?}"
    );
    clock += REST;
    assert!(screen.tick(&store, clock).is_none());
}

#[test]
fn each_levels_filters_narrow_their_own_table() {
    let store = stocked();
    let mut screen = RegistriesScreen::default();
    screen.repositories.input.set_text("registry:acrprod web");
    screen.refilter(&store);
    assert_eq!(shown(&screen, &store), ["web-frontend"]);

    screen.repositories.input.set_text("registry:acrdev");
    screen.refilter(&store);
    assert!(screen.visible().is_empty(), "nothing is in acrdev");

    screen.repositories.input.clear();
    screen.refilter(&store);
    press(&mut screen, &store, KeyCode::Enter);
    screen.tags.input.set_text("digest:9f01");
    screen.refilter(&store);
    assert_eq!(screen.count(), 1);
    assert_eq!(screen.selected_tag(&store).unwrap().name, "1.41.3");
}

#[test]
fn updated_filters_by_how_long_ago_rather_than_how_far_ahead() {
    let now = ts("2026-09-11T20:00:00Z");
    let recent = repository("acrprod", "recent", None, Some("2026-09-08T20:00:00Z"));
    let stale = repository("acrprod", "stale", None, Some("2026-06-13T20:00:00Z"));
    let kept = |raw: &str| -> Vec<&str> {
        let query = Query::parse(raw, REPOSITORY_SCHEMA);
        [&recent, &stale]
            .into_iter()
            .filter(|row| repository_passes(row, &query, now))
            .map(|row| row.name.as_str())
            .collect()
    };
    assert_eq!(kept("updated:<30d"), ["recent"]);
    assert_eq!(kept("updated:>30d"), ["stale"]);
}

#[test]
fn a_header_click_on_the_default_column_turns_the_sort_over() {
    let mut screen = RegistriesScreen::default();
    assert_eq!(screen.repositories.sort, ColumnId::Updated);
    assert!(screen.repositories.descending);
    screen.sort_by(ColumnId::Updated);
    assert!(
        !screen.repositories.descending,
        "ascending now, rather than a click that did nothing"
    );
    screen.sort_by(ColumnId::Updated);
    assert!(screen.repositories.descending);

    // Another column still cycles ascending, descending, then the default.
    screen.sort_by(ColumnId::Tags);
    assert_eq!(
        (screen.repositories.sort, screen.repositories.descending),
        (ColumnId::Tags, false)
    );
    screen.sort_by(ColumnId::Tags);
    assert!(screen.repositories.descending);
    screen.sort_by(ColumnId::Tags);
    assert_eq!(
        (screen.repositories.sort, screen.repositories.descending),
        (ColumnId::Updated, true)
    );
}

#[test]
fn the_details_pane_pages_and_jumps_when_it_has_focus() {
    let store = stocked();
    let mut screen = RegistriesScreen::default();
    let mut shell = Shell::default();
    shell.focus = Focus::Details;
    screen.details_scroll.set_viewport(5, 40);
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    screen.handle_key(&mut shell, &store, key(KeyCode::PageDown));
    assert_eq!(screen.details_scroll.offset, 4, "a screenful less one row");
    screen.handle_key(&mut shell, &store, key(KeyCode::End));
    assert_eq!(screen.details_scroll.offset, 35);
    screen.handle_key(&mut shell, &store, key(KeyCode::Home));
    assert_eq!(screen.details_scroll.offset, 0);
}

#[test]
fn capital_r_turns_the_sort_over_and_capital_s_walks_on() {
    let store = stocked();
    let mut screen = RegistriesScreen::default();
    screen.note_width(200);
    assert_eq!(
        (screen.repositories.sort, screen.repositories.descending),
        (ColumnId::Updated, true),
        "repositories open newest first"
    );
    let before = shown(&screen, &store);

    press(&mut screen, &store, KeyCode::Char('R'));
    assert_eq!(
        (screen.repositories.sort, screen.repositories.descending),
        (ColumnId::Updated, false)
    );
    assert_ne!(shown(&screen, &store), before);

    press(&mut screen, &store, KeyCode::Char('S'));
    assert_ne!(screen.repositories.sort, ColumnId::Updated);
    assert!(
        !screen.repositories.descending,
        "a new column starts ascending"
    );
}
