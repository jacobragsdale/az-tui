//! How an event is painted: its row's cells and its details.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Wrap;

use super::details::{field, section, subtitle};
use super::table::Cell;
use super::theme::theme;
use crate::columns::{ColumnConfig, ColumnId};
use crate::kube::K8sEvent;
use crate::search::Query;
use crate::timestamp::{Timestamp, age};

/// A warning reads in the warning colour across its row; the rest is plain.
fn row_style(event: &K8sEvent) -> Style {
    if event.is_warning() {
        Style::default().fg(theme().warning)
    } else {
        Style::default()
    }
}

/// One row's cells, in the order the visible columns are in.
#[must_use]
pub fn cells(
    event: &K8sEvent,
    columns: &[ColumnConfig],
    highlighter: &Query,
    now: Timestamp,
) -> Vec<Cell> {
    let base = row_style(event);
    columns
        .iter()
        .map(|column| match column.id {
            ColumnId::Age => Cell::styled(age(event.last, now), base),
            ColumnId::K8sType => {
                Cell::styled(event.kind.clone(), base).matched(highlighter.indices(&event.kind))
            }
            ColumnId::Reason => {
                Cell::styled(event.reason.clone(), base).matched(highlighter.indices(&event.reason))
            }
            ColumnId::Object => {
                let slash = event.object.slash();
                Cell::styled(slash.clone(), base).matched(highlighter.indices(&slash))
            }
            ColumnId::Count => Cell::styled(event.count.to_string(), base),
            ColumnId::Message => Cell::styled(event.message.clone(), base)
                .matched(highlighter.indices(&event.message)),
            ColumnId::Namespace => Cell::styled(event.namespace.clone(), base),
            _ => Cell::styled(String::new(), base),
        })
        .collect()
}

/// Everything the pane says about one event, under the toolbar.
#[must_use]
pub fn detail_lines(event: &K8sEvent, width: u16, now: Timestamp) -> Vec<Line<'static>> {
    let palette = theme();
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!(
                "{} ",
                if event.is_warning() {
                    "\u{26a0}"
                } else {
                    "\u{2022}"
                }
            ),
            row_style(event),
        ),
        Span::styled(
            event.reason.clone(),
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(event.kind.clone(), row_style(event)),
    ])];
    let times = if event.count > 1 {
        format!("{}\u{00d7}", event.count)
    } else {
        "once".to_owned()
    };
    lines.push(subtitle(&[
        &event.object.slash(),
        &times,
        &format!("last {}", age(event.last, now)),
    ]));
    lines.push(Line::from(""));
    lines.push(field("Object", event.object.slash()));
    lines.push(field("Namespace", event.namespace.clone()));
    lines.push(field(
        "First",
        event.first.map_or_else(
            || "\u{2014}".to_owned(),
            |stamp| format!("{} · {}", stamp.calendar_date(), age(Some(stamp), now)),
        ),
    ));
    lines.push(field(
        "Last",
        event.last.map_or_else(
            || "\u{2014}".to_owned(),
            |stamp| format!("{} · {}", stamp.calendar_date(), age(Some(stamp), now)),
        ),
    ));
    lines.push(field("Source", event.source.clone()));
    lines.push(Line::from(""));
    lines.push(section("Message", width));
    for line in event.message.lines() {
        lines.push(Line::from(Span::styled(
            line.to_owned(),
            Style::default().fg(palette.body),
        )));
    }
    // A hint the pane wraps rather than the table: the table cuts the
    // message at its column.
    let _ = Wrap { trim: false };
    lines
}
