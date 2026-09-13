use super::*;
use crate::timestamp::ts;

fn now() -> Timestamp {
    ts("2026-09-11T20:00:00Z")
}

fn row(vault: &str, name: &str) -> SecretRow {
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

#[test]
fn the_haystack_holds_every_cell_a_person_might_type_part_of() {
    let mut held = row("kv-prod", "db-password");
    held.content_type = Some("text/plain".into());
    held.tags = vec![("env".into(), "prod".into())];
    let text = haystack(&held);
    assert!(text.contains("kv-prod"));
    assert!(text.contains("db-password"));
    assert!(text.contains("text/plain"));
    assert!(text.contains("env=prod"));
}

#[test]
fn a_field_filter_narrows_and_an_unknown_one_is_ignored() {
    let mut held = row("kv-prod", "db-password");
    held.content_type = Some("text/plain".into());
    held.tags = vec![("env".into(), "prod".into())];

    let query = |raw: &str| Query::parse(raw, SCHEMA);
    assert!(passes(&held, &query("vault:kv-prod"), now()));
    assert!(passes(&held, &query("vault:PROD"), now()));
    assert!(!passes(&held, &query("vault:kv-qa"), now()));
    assert!(passes(&held, &query("type:plain"), now()));
    assert!(passes(&held, &query("tag:env=prod"), now()));
    assert!(!passes(&held, &query("tag:env=qa"), now()));
    assert!(
        passes(&held, &query("vault:kv-prod tag:env"), now()),
        "every filter is ANDed"
    );
}

#[test]
fn a_half_typed_boolean_does_not_empty_the_table() {
    let held = row("kv-prod", "db-password");
    let query = Query::parse("enabled:", SCHEMA);
    assert!(query.fields.is_empty(), "a bare key is still a word");

    let query = Query::parse("enabled:y", SCHEMA);
    assert!(passes(&held, &query, now()));
    let query = Query::parse("enabled:whatever", SCHEMA);
    assert!(
        passes(&held, &query, now()),
        "a value that is not a yes or a no is no opinion"
    );
    let query = Query::parse("enabled:no", SCHEMA);
    assert!(!passes(&held, &query, now()));
}

#[test]
fn an_expiry_is_told_four_ways_and_the_boundary_lands_where_it_says() {
    assert_eq!(Expiry::of(None, now()), Expiry::None);
    assert_eq!(
        Expiry::of(Some(ts("2026-09-08T20:00:00Z")), now()),
        Expiry::Expired
    );
    assert_eq!(
        Expiry::of(Some(ts("2026-10-11T20:00:00Z")), now()),
        Expiry::Soon,
        "exactly 30 days is still soon"
    );
    assert_eq!(
        Expiry::of(Some(ts("2026-10-12T20:01:00Z")), now()),
        Expiry::Later
    );
    assert_eq!(
        Expiry::of(Some(ts("2026-09-11T20:00:00Z")), now()),
        Expiry::Soon,
        "today is soon, not expired"
    );
}

#[test]
fn an_expiry_cell_says_the_age_or_a_dash_or_that_it_has_gone() {
    let soon = Some(ts("2026-09-23T20:00:00Z"));
    assert_eq!(Expiry::None.cell(None, now()), "—");
    assert_eq!(
        Expiry::Soon.cell(soon, now()),
        "12d ⚠",
        "marked as well as coloured"
    );
    assert_eq!(
        Expiry::Expired.cell(Some(ts("2026-09-08T20:00:00Z")), now()),
        "expired"
    );
}

fn order() -> HashMap<&'static str, usize> {
    [("kv-dev", 0), ("kv-qa", 1), ("kv-prod", 2)]
        .into_iter()
        .collect()
}

fn sorted(rows: &[SecretRow], by: ColumnId, descending: bool) -> Vec<String> {
    let mut indices: Vec<usize> = (0..rows.len()).collect();
    sort(&mut indices, rows, by, descending, &order());
    indices
        .into_iter()
        .map(|at| format!("{}/{}", rows[at].vault, rows[at].name))
        .collect()
}

#[test]
fn the_default_sort_puts_one_secrets_three_environments_next_to_each_other() {
    let rows = vec![
        row("kv-prod", "db-password"),
        row("kv-dev", "api-key"),
        row("kv-dev", "db-password"),
        row("kv-qa", "db-password"),
    ];
    assert_eq!(
        sorted(&rows, ColumnId::Name, false),
        [
            "kv-dev/api-key",
            "kv-dev/db-password",
            "kv-qa/db-password",
            "kv-prod/db-password",
        ],
        "by name, then the configuration's vault order — not the alphabet"
    );
}

#[test]
fn sorting_by_the_vault_column_follows_the_configuration_not_the_alphabet() {
    let rows = vec![row("kv-prod", "a"), row("kv-dev", "a"), row("kv-qa", "a")];
    assert_eq!(
        sorted(&rows, ColumnId::Vault, false),
        ["kv-dev/a", "kv-qa/a", "kv-prod/a"]
    );
    assert_eq!(
        sorted(&rows, ColumnId::Vault, true),
        ["kv-prod/a", "kv-qa/a", "kv-dev/a"]
    );
}

#[test]
fn a_stamp_that_is_not_set_sorts_last_whichever_way_the_sort_points() {
    let mut rows = vec![row("kv-dev", "a"), row("kv-dev", "b"), row("kv-dev", "c")];
    rows[0].expires = Some(ts("2026-12-01T00:00:00Z"));
    rows[1].expires = None;
    rows[2].expires = Some(ts("2026-10-01T00:00:00Z"));

    assert_eq!(
        sorted(&rows, ColumnId::Expires, false),
        ["kv-dev/c", "kv-dev/a", "kv-dev/b"],
        "soonest first, and the one with no expiry last"
    );
    assert_eq!(
        sorted(&rows, ColumnId::Expires, true),
        ["kv-dev/a", "kv-dev/c", "kv-dev/b"],
        "flipped, and still last"
    );
}

#[test]
fn only_the_columns_on_screen_can_be_sorted_by() {
    let layout = TableLayout::new(SECRET_COLUMNS);
    let wide = sortable(&layout, 200);
    assert!(wide.contains(&ColumnId::Env));
    assert!(
        !wide.contains(&ColumnId::Vault),
        "the vault is hidden behind its environment"
    );
    assert!(wide.contains(&ColumnId::Name));
    assert!(
        !wide.contains(&ColumnId::Type),
        "Type is hidden by default, so `s` does not stop on it"
    );

    let narrow = sortable(&layout, 30);
    assert!(narrow.len() < wide.len(), "{narrow:?}");
    assert!(
        narrow.contains(&ColumnId::Name) && narrow.contains(&ColumnId::Env),
        "the pinned columns survive any width: {narrow:?}"
    );
}

pub(crate) fn stocked() -> AzureStore {
    use crate::azure::{Inventory, Vault};
    let vault = |name: &str| Vault {
        id: format!("/vaults/{name}"),
        name: name.to_owned(),
        resource_group: "rg".into(),
        location: "eastus".into(),
        uri: format!("https://{name}.vault.azure.net/"),
    };
    let mut store = AzureStore::default();
    store.apply(crate::worker::Event::Inventory(Ok(Inventory {
        vaults: vec![vault("kv-dev"), vault("kv-prod")],
        registries: Vec::new(),
    })));
    store.apply(crate::worker::Event::Secrets {
        vault: "kv-dev".into(),
        result: Ok(vec![row("kv-dev", "api-key"), row("kv-dev", "db-password")]),
    });
    store.apply(crate::worker::Event::Secrets {
        vault: "kv-prod".into(),
        result: Ok(vec![row("kv-prod", "db-password")]),
    });
    store
}

fn shown(screen: &SecretsScreen, store: &AzureStore) -> Vec<String> {
    screen
        .visible()
        .iter()
        .map(|at| format!("{}/{}", store.secrets[*at].vault, store.secrets[*at].name))
        .collect()
}

#[test]
fn a_query_narrows_the_rows_and_the_border_says_how_many_are_left() {
    let store = stocked();
    let mut screen = SecretsScreen::default();
    screen.refilter(&store);
    assert_eq!(screen.visible().len(), 3);
    assert_eq!(screen.status(&store), "3 · Name ↑");

    screen.input.set_text("db");
    screen.refilter(&store);
    assert_eq!(
        shown(&screen, &store),
        ["kv-dev/db-password", "kv-prod/db-password"]
    );
    assert_eq!(screen.status(&store), "2/3 · Name ↑");

    screen.input.set_text("vault:kv-prod");
    screen.refilter(&store);
    assert_eq!(shown(&screen, &store), ["kv-prod/db-password"]);

    screen.input.set_text("zzz");
    screen.refilter(&store);
    assert!(screen.visible().is_empty());
    assert_eq!(screen.cursor.index, 0, "the cursor comes back onto nothing");
}

#[test]
fn the_cursor_stays_on_the_same_secret_when_a_refresh_reorders_the_rows() {
    let mut store = stocked();
    let mut screen = SecretsScreen::default();
    screen.refilter(&store);
    screen.cursor.focus(2);
    assert_eq!(
        screen.cursor_identity(&store),
        Some(("kv-prod".to_owned(), "db-password".to_owned()))
    );

    let was = screen.cursor_identity(&store);
    // kv-dev grows a secret that sorts before the one under the cursor.
    store.apply(crate::worker::Event::Secrets {
        vault: "kv-dev".into(),
        result: Ok(vec![
            row("kv-dev", "aaa-new"),
            row("kv-dev", "api-key"),
            row("kv-dev", "db-password"),
        ]),
    });
    screen.invalidate();
    screen.keep_cursor(&store, was);
    assert_eq!(
        screen.cursor_identity(&store),
        Some(("kv-prod".to_owned(), "db-password".to_owned())),
        "the row moved down one and the cursor went with it"
    );

    // And when the secret under it is gone, the cursor lands on the list.
    let was = screen.cursor_identity(&store);
    store.apply(crate::worker::Event::Secrets {
        vault: "kv-prod".into(),
        result: Ok(Vec::new()),
    });
    screen.invalidate();
    screen.keep_cursor(&store, was);
    assert!(screen.cursor.index < screen.visible().len());
}

#[test]
fn s_walks_the_columns_and_a_header_click_cycles_one() {
    let store = stocked();
    let mut screen = SecretsScreen::default();
    screen.note_width(120);
    assert_eq!(screen.sort, ColumnId::Name);

    screen.next_sort();
    assert_eq!(screen.sort, ColumnId::Enabled);
    assert!(!screen.descending, "a new column starts ascending");

    screen.sort_by(ColumnId::Expires);
    assert_eq!((screen.sort, screen.descending), (ColumnId::Expires, false));
    screen.sort_by(ColumnId::Expires);
    assert_eq!((screen.sort, screen.descending), (ColumnId::Expires, true));
    screen.sort_by(ColumnId::Expires);
    assert_eq!(
        (screen.sort, screen.descending),
        (ColumnId::Name, false),
        "a third click on the same header goes back to the default"
    );
    let _ = &store;
}

/// Prints the cost of one keystroke over forty thousand rows. Ignored by
/// default because it is a measurement, not an assertion about
/// correctness, and because a debug build is ten times slower than the
/// one anybody runs:
///
/// ```console
/// cargo test --release -- --ignored --nocapture filters_forty_thousand
/// ```
#[test]
#[ignore = "a measurement; run it in release"]
fn filters_forty_thousand_rows_between_keystrokes() {
    use crate::azure::{Inventory, Vault};
    let vaults: Vec<String> = (0..8).map(|n| format!("kv-{n}")).collect();
    let mut store = AzureStore::default();
    store.apply(crate::worker::Event::Inventory(Ok(Inventory {
        vaults: vaults
            .iter()
            .map(|name| Vault {
                id: format!("/vaults/{name}"),
                name: name.clone(),
                resource_group: "rg".into(),
                location: "eastus".into(),
                uri: format!("https://{name}.vault.azure.net/"),
            })
            .collect(),
        registries: Vec::new(),
    })));
    for vault in &vaults {
        store.apply(crate::worker::Event::Secrets {
            vault: vault.clone(),
            result: Ok((0..5000)
                .map(|n| row(vault, &format!("service-{n:05}-connection-string")))
                .collect()),
        });
    }
    assert_eq!(store.secrets.len(), 40_000);

    let mut screen = SecretsScreen::default();
    let built = std::time::Instant::now();
    screen.refilter(&store);
    println!("first filter (builds the haystacks): {:?}", built.elapsed());

    // What a person typing `service-04` actually costs, one character at
    // a time, with the haystacks already built.
    let mut worst = std::time::Duration::ZERO;
    for typed in 1..="service-04".len() {
        screen.input.set_text(&"service-04"[..typed]);
        let started = std::time::Instant::now();
        screen.refilter(&store);
        let took = started.elapsed();
        println!(
            "  {:>12} -> {:>6} rows in {took:?}",
            screen.input.text(),
            screen.visible().len()
        );
        worst = worst.max(took);
    }
    println!("worst keystroke: {worst:?}");
    assert!(
        worst < std::time::Duration::from_millis(200),
        "even a debug build should not be this slow: {worst:?}"
    );
}

// ── Reveal, copy, and the rest interval ─────────────────────────────

fn key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
}

fn press(
    screen: &mut SecretsScreen,
    shell: &mut Shell,
    store: &AzureStore,
    code: char,
) -> AppAction {
    screen.handle_key(shell, store, key(crossterm::event::KeyCode::Char(code)))
}

fn value(name: &str) -> Result<(Secret, String), String> {
    Ok((Secret::new(format!("{name}-value")), "v1".to_owned()))
}

#[test]
fn v_asks_once_shows_the_value_and_lets_it_go_at_sixty_seconds() {
    let store = stocked();
    let mut screen = SecretsScreen::default();
    let mut shell = Shell::default();
    screen.refilter(&store);
    let row = screen.selected(&store).unwrap().clone();
    assert_eq!(row.name, "api-key");

    let action = press(&mut screen, &mut shell, &store, 'v');
    assert!(
        matches!(&action, AppAction::Send(crate::worker::Request::Value { name, .. }) if name == "api-key"),
        "{action:?}"
    );
    assert!(screen.is_reading(&row));

    let clock = Instant::now();
    screen.on_value(
        &mut shell,
        &store,
        "kv-dev",
        "api-key",
        value("api-key"),
        clock,
    );
    let held = screen.revealed_here(&row).expect("a value");
    assert_eq!(held.expose(), "api-key-value");
    assert_eq!(held.version, "v1");
    assert_eq!(held.clears_in(clock), 60);
    assert_eq!(held.clears_in(clock + Duration::from_secs(13)), 47);

    // One second short of a minute it is still there; at a minute it is
    // not.
    screen.tick(&store, clock + Duration::from_secs(59));
    assert!(screen.revealed_here(&row).is_some());
    screen.tick(&store, clock + REVEAL_FOR);
    assert!(screen.revealed_here(&row).is_none());
}

#[test]
fn v_again_hides_it_and_so_does_moving_r_and_a_tab_switch() {
    let store = stocked();
    let mut screen = SecretsScreen::default();
    let mut shell = Shell::default();
    screen.refilter(&store);
    let row = screen.selected(&store).unwrap().clone();
    let show = |screen: &mut SecretsScreen, shell: &mut Shell| {
        press(screen, shell, &store, 'v');
        screen.on_value(
            shell,
            &store,
            "kv-dev",
            "api-key",
            value("api-key"),
            Instant::now(),
        );
    };

    show(&mut screen, &mut shell);
    press(&mut screen, &mut shell, &store, 'v');
    assert!(screen.revealed_here(&row).is_none(), "v again hides it");

    show(&mut screen, &mut shell);
    press(&mut screen, &mut shell, &store, 'j');
    assert!(screen.revealed.is_none(), "moving the cursor drops it");

    screen.cursor.focus(0);
    show(&mut screen, &mut shell);
    screen.on_refresh();
    assert!(screen.revealed.is_none(), "r and a tab switch drop it");
}

#[test]
fn a_value_for_a_row_the_cursor_has_left_is_dropped_on_the_floor() {
    let store = stocked();
    let mut screen = SecretsScreen::default();
    let mut shell = Shell::default();
    screen.refilter(&store);
    press(&mut screen, &mut shell, &store, 'v');
    press(&mut screen, &mut shell, &store, 'j');

    screen.on_value(
        &mut shell,
        &store,
        "kv-dev",
        "api-key",
        value("api-key"),
        Instant::now(),
    );
    assert!(screen.revealed.is_none(), "it belonged to the row before");
}

#[test]
fn y_without_a_reveal_asks_and_copies_on_arrival_and_with_one_copies_at_once() {
    let store = stocked();
    let mut screen = SecretsScreen::default();
    let mut shell = Shell::default();
    screen.refilter(&store);

    let action = press(&mut screen, &mut shell, &store, 'y');
    assert!(matches!(action, AppAction::Send(_)), "{action:?}");
    let action = screen.on_value(
        &mut shell,
        &store,
        "kv-dev",
        "api-key",
        value("api-key"),
        Instant::now(),
    );
    match action {
        AppAction::Copy { text, label } => {
            assert_eq!(text, "api-key-value");
            assert_eq!(label, "Copied value of api-key (kv-dev)");
            assert!(!label.contains("api-key-value"), "the label never says it");
        }
        other => panic!("{other:?}"),
    }
    assert!(
        screen.revealed.is_none(),
        "copying blind does not put it on the screen"
    );

    // With one already showing, `y` copies it and sends nothing.
    press(&mut screen, &mut shell, &store, 'v');
    screen.on_value(
        &mut shell,
        &store,
        "kv-dev",
        "api-key",
        value("api-key"),
        Instant::now(),
    );
    let action = press(&mut screen, &mut shell, &store, 'y');
    assert!(matches!(action, AppAction::Copy { .. }), "{action:?}");
}

#[test]
fn a_refusal_shows_under_value_and_goes_when_the_cursor_does() {
    let store = stocked();
    let mut screen = SecretsScreen::default();
    let mut shell = Shell::default();
    screen.refilter(&store);
    press(&mut screen, &mut shell, &store, 'v');
    screen.on_value(
        &mut shell,
        &store,
        "kv-dev",
        "api-key",
        Err("kv-dev: no permission to read secrets".to_owned()),
        Instant::now(),
    );
    assert_eq!(
        screen.refusal().map(String::as_str),
        Some("kv-dev: no permission to read secrets")
    );
    press(&mut screen, &mut shell, &store, 'j');
    assert!(screen.refusal().is_none());
}

#[test]
fn holding_j_down_across_ten_rows_asks_for_one_rows_versions_not_ten() {
    // Deeper than the ten presses, so the cursor never hits the end and
    // starts resting there.
    let mut store = stocked();
    store.apply(crate::worker::Event::Secrets {
        vault: "kv-dev".into(),
        result: Ok((0..20)
            .map(|n| row("kv-dev", &format!("secret-{n:02}")))
            .collect()),
    });
    let mut screen = SecretsScreen::default();
    let mut shell = Shell::default();
    screen.refilter(&store);

    let mut clock = Instant::now();
    let mut asked = 0;
    for _ in 0..10 {
        press(&mut screen, &mut shell, &store, 'j');
        // A key every 40 ms, which is a held-down key.
        clock += Duration::from_millis(40);
        if screen.tick(&store, clock).is_some() {
            asked += 1;
        }
    }
    assert_eq!(asked, 0, "nothing rested long enough to be worth asking");

    // Let go, and the row under the cursor is asked about — once.
    clock += REST;
    let request = screen.tick(&store, clock);
    let here = screen.selected(&store).unwrap().name.clone();
    assert!(
        matches!(&request, Some(crate::worker::Request::Versions { name, .. }) if *name == here),
        "{request:?} for {here}"
    );
    clock += REST;
    assert!(
        screen.tick(&store, clock).is_none(),
        "asked once per row per run"
    );
}

#[test]
fn the_debug_of_the_whole_screen_never_contains_a_value() {
    let store = stocked();
    let mut screen = SecretsScreen::default();
    let mut shell = Shell::default();
    screen.refilter(&store);
    press(&mut screen, &mut shell, &store, 'v');
    screen.on_value(
        &mut shell,
        &store,
        "kv-dev",
        "api-key",
        Ok((Secret::new("hunter2"), "v1".to_owned())),
        Instant::now(),
    );
    let printed = format!("{:?}", screen.revealed.as_ref().unwrap());
    assert!(printed.contains("api-key"), "{printed}");
    assert!(!printed.contains("hunter2"), "{printed}");
}

#[test]
fn capital_y_copies_the_name_and_o_opens_the_vaults_blade() {
    let store = stocked();
    let mut screen = SecretsScreen::default();
    let mut shell = Shell::default();
    screen.refilter(&store);

    match press(&mut screen, &mut shell, &store, 'Y') {
        AppAction::Copy { text, label } => {
            assert_eq!(text, "api-key");
            assert!(label.contains("kv-dev"), "{label}");
        }
        other => panic!("{other:?}"),
    }
    match press(&mut screen, &mut shell, &store, 'o') {
        AppAction::OpenUrl(url) => {
            assert!(
                url.starts_with("https://portal.azure.com/#@/resource/vaults/kv-dev"),
                "{url}"
            );
            assert!(url.ends_with("/secrets"), "{url}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_badge_counts_only_enabled_secrets_that_are_running_out() {
    let mut rows = vec![
        row("kv-a", "fine"),
        row("kv-a", "soon"),
        row("kv-a", "gone"),
    ];
    rows[1].expires = Some(ts("2026-09-23T20:00:00Z"));
    rows[2].expires = Some(ts("2026-09-08T20:00:00Z"));
    assert_eq!(expiring(&rows, now()), 2);

    rows[2].enabled = false;
    assert_eq!(
        expiring(&rows, now()),
        1,
        "a disabled secret's expiry is nobody's problem"
    );
}

#[test]
fn an_answer_for_a_row_the_cursor_left_does_not_cancel_the_ask_on_the_new_one() {
    let store = stocked();
    let mut screen = SecretsScreen::default();
    let mut shell = Shell::default();
    screen.refilter(&store);
    let first = screen.selected(&store).unwrap().clone();
    press(&mut screen, &mut shell, &store, 'v');
    press(&mut screen, &mut shell, &store, 'j');
    let second = screen.selected(&store).unwrap().clone();
    assert_ne!(first.name, second.name);
    press(&mut screen, &mut shell, &store, 'v');
    // The first answer lands late, for a row nobody is on any more.
    screen.on_value(
        &mut shell,
        &store,
        &first.vault,
        &first.name,
        Ok((Secret::new("a"), "v1".into())),
        Instant::now(),
    );
    screen.on_value(
        &mut shell,
        &store,
        &second.vault,
        &second.name,
        Ok((Secret::new("b"), "v2".into())),
        Instant::now(),
    );
    assert!(
        screen.revealed_here(&second).is_some(),
        "the ask still out was answered, not cancelled by the stale one"
    );
}

#[test]
fn a_click_on_another_row_takes_a_revealed_value_off_the_screen() {
    let store = stocked();
    let mut screen = SecretsScreen::default();
    let mut shell = Shell::default();
    screen.refilter(&store);
    let row = screen.selected(&store).unwrap().clone();
    press(&mut screen, &mut shell, &store, 'v');
    screen.on_value(
        &mut shell,
        &store,
        &row.vault,
        &row.name,
        Ok((Secret::new("x"), "v".into())),
        Instant::now(),
    );
    assert!(screen.revealed_here(&row).is_some());
    screen.handle_click(&mut shell, &store, Target::Row(1));
    assert!(
        screen.revealed_here(&row).is_none(),
        "looking away drops it, by mouse as by key"
    );
    assert!(!screen.is_ticking(), "and nothing is left counting down");
}

#[test]
fn the_wheel_after_a_shrinking_refresh_does_not_turn_the_window_inside_out() {
    use crate::azure::Inventory;
    use crate::worker::Event;

    let mut store = AzureStore::default();
    store.apply(Event::Inventory(Ok(Inventory {
        vaults: vec![crate::azure::Vault {
            id: "/vaults/kv-dev".into(),
            name: "kv-dev".into(),
            resource_group: "rg".into(),
            location: "eastus".into(),
            uri: "https://kv-dev.vault.azure.net/".into(),
        }],
        registries: Vec::new(),
    })));
    store.apply(Event::Secrets {
        vault: "kv-dev".into(),
        result: Ok((0..20)
            .map(|n| row("kv-dev", &format!("s-{n:02}")))
            .collect()),
    });
    let mut screen = SecretsScreen::default();
    screen.refilter(&store);
    // As the last draw left it: a five-row window, the cursor well down.
    screen.cursor.scroll.set_viewport(5, screen.visible().len());
    screen.cursor.focus(15);
    // The vault answers with two rows; the table is short and the scroll
    // state is stale.
    store.apply(Event::Secrets {
        vault: "kv-dev".into(),
        result: Ok(vec![row("kv-dev", "a"), row("kv-dev", "b")]),
    });
    screen.invalidate();
    screen.keep_cursor(&store, None);
    screen.handle_wheel(&mut Shell::default(), None, 3);
    assert!(screen.cursor.index < screen.visible().len());
}
