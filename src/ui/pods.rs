//! How a pod is painted: its row's cells and its details.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::details::{field, quiet, refused, section, subtitle};
use super::table::Cell;
use super::theme::theme;
use crate::columns::{ColumnConfig, ColumnId};
use crate::kube::{Pod, Replicas};
use crate::search::Query;
use crate::timestamp::{Timestamp, age};

/// The colour of a pod's glyph and status word: what its state reads as, at
/// a glance.
#[must_use]
pub fn pod_style(pod: &Pod) -> Style {
    let palette = theme();
    let colour = match pod.glyph() {
        "\u{25cf}" => palette.success,
        "\u{25d0}" => palette.warning,
        "\u{2717}" => palette.error,
        _ => palette.muted,
    };
    Style::default().fg(colour)
}

/// The colour the row's other cells take: the error colour on a pod in
/// trouble, muted on one that has finished, and nothing otherwise.
fn row_style(pod: &Pod) -> Style {
    let palette = theme();
    if pod.is_unhealthy() {
        Style::default().fg(palette.error)
    } else if matches!(pod.glyph(), "\u{2713}" | "\u{25cb}") {
        Style::default().fg(palette.muted)
    } else {
        Style::default()
    }
}

/// One row's cells, in the order the visible columns are in.
#[must_use]
pub fn cells(
    pod: &Pod,
    columns: &[ColumnConfig],
    highlighter: &Query,
    now: Timestamp,
) -> Vec<Cell> {
    let base = row_style(pod);
    columns
        .iter()
        .map(|column| match column.id {
            ColumnId::Name => {
                Cell::styled(pod.key.name.clone(), base).matched(highlighter.indices(&pod.key.name))
            }
            ColumnId::Namespace => Cell::styled(pod.key.namespace.clone(), base)
                .matched(highlighter.indices(&pod.key.namespace)),
            ColumnId::Ready => Cell::styled(pod.ready_label(), base),
            ColumnId::Status => {
                Cell::styled(format!("{} {}", pod.glyph(), pod.status), pod_style(pod)).matched(
                    highlighter
                        .indices(&pod.status)
                        .into_iter()
                        .map(|index| index + 2)
                        .collect(),
                )
            }
            ColumnId::Restarts => Cell::styled(pod.restarts.to_string(), base),
            ColumnId::Age => Cell::styled(age(pod.created, now), base),
            ColumnId::Node => {
                Cell::styled(pod.node.clone(), base).matched(highlighter.indices(&pod.node))
            }
            ColumnId::Ip => Cell::styled(pod.ip.clone(), base),
            ColumnId::Owner => Cell::styled(pod.owner_name().to_owned(), base)
                .matched(highlighter.indices(pod.owner_name())),
            ColumnId::Image => {
                let image = pod.containers.first().map_or("", |c| c.image.as_str());
                Cell::styled(image.to_owned(), base).matched(highlighter.indices(image))
            }
            _ => Cell::styled(String::new(), base),
        })
        .collect()
}

/// Everything the pane says about one pod, under the toolbar.
#[must_use]
pub fn detail_lines(
    pod: &Pod,
    owner: Option<&Replicas>,
    last_error: Option<&String>,
    width: u16,
    now: Timestamp,
) -> Vec<Line<'static>> {
    let palette = theme();
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{} ", pod.glyph()), pod_style(pod)),
        Span::styled(
            pod.key.name.clone(),
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(pod.status.clone(), pod_style(pod)),
    ])];
    let created = age(pod.created, now);
    let owner_said = owner.map_or_else(
        || pod.owner_label(),
        |replicas| format!("{} · {}", pod.owner_label(), replicas.label()),
    );
    lines.push(subtitle(&[
        &format!("{}/{}", pod.key.cluster, pod.key.namespace),
        &owner_said,
        &created,
    ]));
    lines.push(Line::from(""));
    lines.push(field("Ready", pod.ready_label()));
    lines.push(field("Restarts", pod.restarts.to_string()));
    lines.push(field("Node", dash_if_empty(&pod.node)));
    lines.push(field("IP", dash_if_empty(&pod.ip)));
    lines.push(field(
        "Created",
        pod.created.map_or_else(
            || "\u{2014}".to_owned(),
            |stamp| format!("{} · {created}", stamp.calendar_date()),
        ),
    ));
    if !pod.labels.is_empty() {
        let mut spans = vec![Span::styled(
            format!("{:<width$}", "Labels", width = super::details::LABEL),
            Style::default().fg(palette.muted),
        )];
        for (at, (key, value)) in pod.labels.iter().enumerate() {
            if at > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(super::details::chip(key, value));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(""));
    lines.push(section("Containers", width));
    for container in &pod.containers {
        let (mark, colour) = if container.ready {
            ("\u{2713}", palette.success)
        } else {
            ("\u{2717}", palette.error)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{mark} "), Style::default().fg(colour)),
            Span::styled(container.name.clone(), Style::default().fg(palette.text)),
            Span::styled(
                format!("  {}  \u{21bb}{}", container.state, container.restarts),
                Style::default().fg(if container.ready {
                    palette.muted
                } else {
                    palette.error
                }),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            format!("  {}", container.image),
            Style::default().fg(palette.muted),
        )));
        if let Some((reason, code)) = &container.last_termination {
            lines.push(Line::from(Span::styled(
                format!("  last exit: {reason} ({code})"),
                Style::default().fg(palette.warning),
            )));
        }
    }
    if let Some(message) = last_error {
        lines.push(Line::from(""));
        lines.push(section("Problem", width));
        lines.push(refused(format!("last read failed: {message}")));
        lines.push(quiet("these rows are from the read before it"));
    }
    lines
}

fn dash_if_empty(value: &str) -> String {
    if value.is_empty() {
        "\u{2014}".to_owned()
    } else {
        value.to_owned()
    }
}
