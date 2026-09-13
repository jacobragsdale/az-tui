//! How a configmap or a secret is painted: its row's cells, and details that
//! list its keys with the details cursor on one of them.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::details::{field, quiet, section, subtitle};
use super::table::Cell;
use super::theme::theme;
use crate::columns::{ColumnConfig, ColumnId};
use crate::kube::{ConfigMap, SecretMeta};
use crate::search::Query;
use crate::timestamp::{Timestamp, age};

/// One configmap row's cells.
#[must_use]
pub fn configmap_cells(
    held: &ConfigMap,
    columns: &[ColumnConfig],
    highlighter: &Query,
    now: Timestamp,
) -> Vec<Cell> {
    columns
        .iter()
        .map(|column| match column.id {
            ColumnId::Name => Cell::new(held.name.clone()).matched(highlighter.indices(&held.name)),
            ColumnId::Namespace => Cell::new(held.namespace.clone()),
            ColumnId::Keys => Cell::new(held.data.len().to_string()),
            ColumnId::Age => Cell::new(age(held.created, now)),
            _ => Cell::new(String::new()),
        })
        .collect()
}

/// One secret row's cells.
#[must_use]
pub fn secret_cells(
    held: &SecretMeta,
    columns: &[ColumnConfig],
    highlighter: &Query,
    now: Timestamp,
) -> Vec<Cell> {
    let palette = theme();
    columns
        .iter()
        .map(|column| match column.id {
            ColumnId::Name => Cell::new(held.name.clone()).matched(highlighter.indices(&held.name)),
            ColumnId::Namespace => Cell::new(held.namespace.clone()),
            ColumnId::K8sType => {
                Cell::styled(held.kind.clone(), Style::default().fg(palette.muted))
                    .matched(highlighter.indices(&held.kind))
            }
            ColumnId::Keys => Cell::new(held.keys.len().to_string()),
            ColumnId::Age => Cell::new(age(held.created, now)),
            _ => Cell::new(String::new()),
        })
        .collect()
}

/// What the pane's first lines say about a configmap or a secret.
pub struct Head<'a> {
    pub name: &'a str,
    /// `configmap` or `secret`.
    pub what: &'a str,
    pub namespace: &'a str,
    /// A secret's type; a configmap has none.
    pub kind_word: Option<&'a str>,
    pub created: Option<Timestamp>,
}

/// Everything the pane says about one configmap or secret, under the
/// toolbar: what it is, then its keys with the details cursor on one.
/// Answers the lines and the index of the first key's line, so the caller
/// can put a region on each.
#[must_use]
pub fn detail_lines(
    head: &Head<'_>,
    keys: &[(String, String)],
    key_cursor: usize,
    width: u16,
    now: Timestamp,
) -> (Vec<Line<'static>>, usize) {
    let palette = theme();
    let mut lines = vec![Line::from(Span::styled(
        head.name.to_owned(),
        Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::BOLD),
    ))];
    let count = format!("{} keys", keys.len());
    let mut parts = vec![head.namespace, head.what, &count];
    if let Some(kind_word) = head.kind_word {
        parts.push(kind_word);
    }
    lines.push(subtitle(&parts));
    lines.push(Line::from(""));
    lines.push(field(
        "Created",
        head.created.map_or_else(
            || "\u{2014}".to_owned(),
            |stamp| format!("{} · {}", stamp.calendar_date(), age(Some(stamp), now)),
        ),
    ));
    lines.push(Line::from(""));
    lines.push(section("Keys", width));
    let first_key = lines.len();
    if keys.is_empty() {
        lines.push(quiet("none"));
    }
    for (at, (key, said)) in keys.iter().enumerate() {
        let on = at == key_cursor;
        lines.push(Line::from(vec![
            Span::styled(
                if on { "\u{203a} " } else { "  " },
                Style::default().fg(palette.accent),
            ),
            Span::styled(
                key.clone(),
                if on {
                    Style::default()
                        .fg(palette.text)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(palette.body)
                },
            ),
            Span::styled(format!("  {said}"), Style::default().fg(palette.muted)),
        ]));
    }
    (lines, first_key)
}
