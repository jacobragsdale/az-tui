//! One list, drawn as a table: the frame round it, the header, the rows, and
//! where every one of them landed so a click can be turned back into a row.
//!
//! Lifted from ticket-tui, less the bookmark gutter it has no use for here and
//! less every cell painter: there a screen hands the table a closure and the
//! table asks it for a cell at a time, which only pays off when the cells
//! carry work-item colours the table itself has to know about. Here a screen
//! paints its own rows and hands them over, so this file knows about a border,
//! a header, a cursor and a search match, and about nothing in Azure at all.

use ratatui::Frame;
use ratatui::layout::{Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::merge::MergeStrategy;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, HighlightSpacing, Padding, Row, Table, TableState,
};

use crate::app::cursor::ListCursor;
use crate::columns::{
    COLUMN_SPACING, ColumnConfig, ColumnId, SCROLLBAR_WIDTH, SELECTION_WIDTH, TableLayout,
};
use crate::ui::theme::theme;

/// Where a list table's parts land inside its area. A screen works this out
/// before it draws, because how many rows fit is what its viewport is, and
/// because it has to know how many rows to paint before it can hand them over.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TableGeometry {
    /// Inside the border.
    pub inner: Rect,
    /// The rows, below the header and the rule under it.
    pub body: Rect,
    pub visible_rows: usize,
}

impl TableGeometry {
    /// Which rows of a list of `total` this table has room for, having told
    /// the cursor how much of the list is on screen so that it can scroll
    /// itself and know what a page is.
    ///
    /// A screen paints exactly these rows and hands them to [`TableSpec`],
    /// which is the only way the two agree on what `rows[0]` is.
    pub fn window(self, cursor: &mut ListCursor, total: usize) -> std::ops::Range<usize> {
        cursor.scroll.set_viewport(self.visible_rows, total);
        let start = cursor.scroll.offset.min(total);
        start..total.min(start.saturating_add(self.visible_rows))
    }
}

/// One row per line, always.
///
// ponytail: ticket-tui's tables can double their row height to fit a second
// line of tag badges under the title. Nothing in az-tui has a second line to
// put there, so the height is one and the arithmetic that divided by it is
// gone; a density setting would bring back a `row_height` field here and a
// division in `visible_rows`.
#[must_use]
pub fn table_geometry(area: Rect) -> TableGeometry {
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    // The header and the rule under it.
    let body_height = inner.height.saturating_sub(2);
    TableGeometry {
        inner,
        body: Rect::new(inner.x, inner.y.saturating_add(2), inner.width, body_height),
        visible_rows: usize::from(body_height).max(1),
    }
}

/// One cell of one row, as the screen that owns the row has painted it.
///
/// The style carries whatever the screen decided — the muted foreground of a
/// stale vault's row, the warning colour on an expiry inside thirty days —
/// and `Style::default()` leaves the cell whatever the row it is in was
/// painted. Nothing here knows why.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Cell {
    pub text: String,
    pub style: Style,
    /// The characters the query matched, counted in `char`s from the start of
    /// `text`, as `nucleo` hands them back. They are painted in
    /// `theme().search_match` so a hit reads from across the table.
    pub matches: Vec<u32>,
}

impl Cell {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            style,
            ..Self::new(text)
        }
    }

    /// A cell in one colour, or in whatever its row is painted when the
    /// screen has nothing to say about this one.
    #[must_use]
    pub fn colored(text: impl Into<String>, color: Option<Color>) -> Self {
        Self::styled(
            text,
            color.map_or_else(Style::default, |color| Style::default().fg(color)),
        )
    }

    /// The same cell with the query's hits in it lit up.
    #[must_use]
    pub fn matched(mut self, matches: Vec<u32>) -> Self {
        self.matches = matches;
        self
    }
}

/// One list, drawn as a table. Both tabs go through here, so a row is
/// selected, hovered and sorted the same way whatever it holds.
pub struct TableSpec<'a> {
    /// What the pane is, on the top border: `Secrets`, `Registries`, a
    /// repository's name. It stays the same while the list underneath it
    /// changes.
    pub title: String,
    /// What the list is doing, on the bottom border: how many rows, how they
    /// are ordered. Empty leaves the bottom border bare.
    pub status: String,
    pub focused: bool,
    /// The columns as the width solver left them — the caller needs them
    /// anyway, to know how many cells to paint per row.
    pub columns: &'a [ColumnConfig],
    /// The column the list is ordered by and the arrow that says which way, if
    /// it is ordered by a column at all.
    pub sorted: Option<(ColumnId, &'static str)>,
    /// The rows on screen and no others, in order, the first of them being
    /// row `cursor.scroll.offset` of the list. A screen sizes this window
    /// with [`table_geometry`] before it paints anything.
    pub rows: &'a [Vec<Cell>],
    /// How many rows the list has, which is not how many are on screen.
    pub total: usize,
    /// Which row is selected and how far the list is scrolled. The table
    /// records what it turned out to have room for on the way past, so the
    /// keys that move the cursor know what a page is.
    pub cursor: &'a mut ListCursor,
    /// The row the pointer is over, if it is over one.
    pub hovered: Option<usize>,
}

/// Where the table put the things a click can land on. The screen that drew
/// it turns these into its own targets and rebuilds the list every frame, so
/// nothing here has to know what a click means.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TableHits {
    /// The rows on screen: which row of the list, and where it landed.
    pub rows: Vec<(usize, Rect)>,
    /// The header cells, in the order they were drawn. The column comes back
    /// whole rather than as its key: a screen that has just been clicked on
    /// wants to sort by it, and looking a key back up could fail.
    pub headers: Vec<(ColumnId, Rect)>,
}

pub fn render_list_table(frame: &mut Frame, area: Rect, spec: &mut TableSpec<'_>) -> TableHits {
    let geometry = table_geometry(area);
    let inner = geometry.inner;
    // Recorded on the way past rather than asked for: a screen that has just
    // been resized cannot know what a page is until something has been drawn.
    spec.cursor
        .scroll
        .set_viewport(geometry.visible_rows, spec.total);
    let offset = spec.cursor.scroll.offset;

    // The scrollbar's column is padding, not a place a cell may be painted:
    // the table lays its columns out inside what is left, so the last one
    // keeps every character it was given whether or not the list overflows.
    let mut block =
        focused_block(spec.title.clone(), spec.focused).padding(Padding::right(SCROLLBAR_WIDTH));
    if !spec.status.is_empty() {
        block = block.title_bottom(Line::from(format!(" {} ", spec.status)));
    }

    let constraints: Vec<_> = spec
        .columns
        .iter()
        .copied()
        .map(TableLayout::constraint)
        .collect();
    let header = Row::new(spec.columns.iter().map(|column| {
        let column_spec = column.id.spec();
        let arrow = spec
            .sorted
            .filter(|(sorted, _)| *sorted == column.id)
            .map_or_else(String::new, |(_, symbol)| {
                // The arrow gets a space in front of it only where the
                // column has one to give: `Updated` is eight cells wide and
                // `Updated ↓` is nine, which would truncate the arrow away
                // and leave the sorted column looking unsorted.
                let label = column_spec.label.chars().count();
                if column_spec.flexible || label + 2 <= usize::from(column.width) {
                    format!(" {symbol}")
                } else {
                    symbol.to_string()
                }
            });
        Line::from(format!("{}{arrow}", column_spec.label)).alignment(column_spec.align)
    }))
    .style(
        Style::default()
            .fg(theme().header)
            .add_modifier(Modifier::BOLD),
    )
    .height(1)
    .bottom_margin(1);

    // A palette that names the colour a selected row reads in lends it to the
    // cells that have none of their own; a cell the screen coloured keeps
    // what it was given, so the row under the cursor still reads as what it
    // is.
    let selection_fg = theme().selection_fg;
    let matched = Style::default()
        .fg(theme().search_match)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let rows = spec
        .rows
        .iter()
        .take(geometry.visible_rows)
        .enumerate()
        .map(|(visible_index, cells)| {
            let row = Row::new(spec.columns.iter().enumerate().map(|(index, column)| {
                let align = column.id.spec().align;
                cells.get(index).map_or_else(Line::default, |cell| {
                    highlight_line(&cell.text, &cell.matches, cell.style, matched).alignment(align)
                })
            }));
            if selection_fg == Color::Reset || spec.cursor.index != offset + visible_index {
                row
            } else {
                row.style(Style::default().fg(selection_fg))
            }
        });

    let table = Table::new(rows, constraints.clone())
        .header(header)
        .block(block)
        .column_spacing(COLUMN_SPACING)
        .row_highlight_style(
            Style::default()
                .bg(theme().selected_background)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(Line::styled(
            "\u{203a} ",
            Style::default()
                .fg(theme().accent)
                .add_modifier(Modifier::BOLD),
        ))
        .highlight_spacing(HighlightSpacing::Always);
    let mut state = TableState::default();
    if let Some(selected) = spec
        .cursor
        .index
        .checked_sub(offset)
        .filter(|selected| *selected < geometry.visible_rows)
    {
        state.select(Some(selected));
    }
    frame.render_stateful_widget(table, area, &mut state);

    let mut hits = TableHits::default();
    if inner.height < 2 || inner.width == 0 {
        return hits;
    }
    // The blank row the header's bottom margin leaves, drawn as a rule: the
    // column names read as a heading over the rows rather than as a first row
    // among them.
    frame.render_widget(
        Line::styled(
            BorderType::border_symbols(theme().border_type)
                .horizontal_top
                .repeat(usize::from(inner.width)),
            Style::default().fg(theme().border),
        ),
        Rect::new(inner.x, inner.y.saturating_add(1), inner.width, 1),
    );

    let header_area = Rect::new(
        inner.x.saturating_add(SELECTION_WIDTH),
        inner.y,
        inner
            .width
            .saturating_sub(SELECTION_WIDTH)
            .saturating_sub(SCROLLBAR_WIDTH),
        1,
    );
    hits.headers = Layout::horizontal(constraints)
        .spacing(COLUMN_SPACING)
        .split(header_area)
        .iter()
        .zip(spec.columns)
        .map(|(rect, column)| (column.id, *rect))
        .collect();

    let body = geometry.body;
    let drawn = spec.rows.len().min(geometry.visible_rows);
    hits.rows = (0..drawn)
        .map_while(|visible_index| {
            let y = body
                .y
                .saturating_add(u16::try_from(visible_index).unwrap_or(u16::MAX));
            (y < body.y.saturating_add(body.height)).then(|| {
                (
                    offset + visible_index,
                    // Less the scrollbar's column, which belongs to the
                    // scrollbar whether or not one is drawn there.
                    Rect::new(body.x, y, body.width.saturating_sub(SCROLLBAR_WIDTH), 1),
                )
            })
        })
        .collect();

    if let Some(hovered) = spec.hovered
        && let Some((_, rect)) = hits.rows.iter().find(|(index, _)| *index == hovered)
    {
        tint(frame, *rect);
    }
    hits
}

/// The wash under the row the pointer is over.
///
/// A row's cells carry colours of their own — a warning on an expiry, a muted
/// foreground on a stale vault — and reversing the row would flatten them into
/// one block, so it is tinted instead. Painted after the table, so a row that
/// is hovered *and* selected shows the hover over the selection. A palette
/// with no tint to give has to reverse the row after all.
fn tint(frame: &mut Frame, rect: Rect) {
    let wash = theme().hover_background;
    let rect = rect.intersection(frame.area());
    let buffer = frame.buffer_mut();
    for y in rect.y..rect.y.saturating_add(rect.height) {
        for x in rect.x..rect.x.saturating_add(rect.width) {
            let cell = &mut buffer[(x, y)];
            let style = if wash == Color::Reset {
                cell.style().add_modifier(Modifier::REVERSED)
            } else {
                cell.style().bg(wash)
            };
            cell.set_style(style);
        }
    }
}

/// The frame every pane wears: the theme's corners, the accent while it has
/// focus and a weight on its name to say so, and borders that merge with a
/// neighbour's rather than sitting beside them.
///
/// The merge is `Fuzzy` rather than `Exact` because the corners are rounded:
/// there is no rounded `┬`, so an exact merge would leave `╮` where two panes
/// meet. Fuzzy falls back to the plain junction, which is the glyph a seam
/// wants.
fn focused_block<'a>(title: impl Into<Line<'a>>, focused: bool) -> Block<'a> {
    let title = title.into();
    Block::default()
        .title(if focused {
            title.style(Style::default().add_modifier(Modifier::BOLD))
        } else {
            title
        })
        .borders(Borders::ALL)
        .border_type(theme().border_type)
        .merge_borders(MergeStrategy::Fuzzy)
        .border_style(Style::default().fg(if focused {
            theme().border_focused
        } else {
            theme().border
        }))
}

/// `text`, with the characters at `indices` painted in `matched` and the rest
/// in `base`.
///
/// The indices are `char` positions and they arrive sorted, which is what
/// `nucleo` produces; one pass over the string is enough, and runs of
/// neighbouring characters come out as one span rather than one each.
#[must_use]
pub fn highlight_line(text: &str, indices: &[u32], base: Style, matched: Style) -> Line<'static> {
    if indices.is_empty() {
        // The style rides on the span rather than on the line, so a caller
        // that harvests `spans` keeps the colour.
        return Line::from(Span::styled(text.to_owned(), base));
    }

    let mut spans = Vec::new();
    let mut current = String::new();
    let mut current_matched = false;
    let mut next = 0;
    for (index, character) in text.chars().enumerate() {
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        while next < indices.len() && indices[next] < index {
            next += 1;
        }
        let is_match = indices.get(next) == Some(&index);
        if !current.is_empty() && is_match != current_matched {
            let style = if current_matched { matched } else { base };
            spans.push(Span::styled(std::mem::take(&mut current), style));
        }
        current.push(character);
        current_matched = is_match;
    }
    if !current.is_empty() {
        let style = if current_matched { matched } else { base };
        spans.push(Span::styled(current, style));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use super::*;
    use crate::columns::{SECRET_COLUMNS, TAG_COLUMNS};

    fn row(cells: &[&str]) -> Vec<Cell> {
        cells.iter().map(|text| Cell::new(*text)).collect()
    }

    fn secrets() -> Vec<Vec<Cell>> {
        vec![
            row(&["kv-prod", "db-password", "\u{2713}", "12d", "2h"]),
            row(&["kv-prod", "signing-key", "\u{2717}", "\u{2014}", "3d"]),
            row(&["kv-dev", "db-password", "\u{2713}", "\u{2014}", "1y"]),
        ]
    }

    /// Draws one table and hands back the buffer and where things landed.
    fn draw(
        width: u16,
        height: u16,
        rows: &[Vec<Cell>],
        cursor: &mut ListCursor,
        build: impl FnOnce(&mut TableSpec<'_>),
    ) -> (Buffer, TableHits) {
        let layout = TableLayout::new(SECRET_COLUMNS);
        let columns = layout.visible_columns(TableLayout::available_width(width - 2));
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut hits = TableHits::default();
        terminal
            .draw(|frame| {
                let mut spec = TableSpec {
                    title: " Secrets ".to_owned(),
                    status: format!("{}/412 \u{b7} Name \u{2191}", rows.len()),
                    focused: true,
                    columns: &columns,
                    sorted: Some((ColumnId::Name, "\u{2191}")),
                    rows,
                    total: 412,
                    cursor,
                    hovered: None,
                };
                build(&mut spec);
                hits = render_list_table(frame, frame.area(), &mut spec);
            })
            .unwrap();
        (terminal.backend().buffer().clone(), hits)
    }

    fn text(buffer: &Buffer) -> String {
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
    fn the_table_says_what_it_is_on_one_border_and_what_it_is_doing_on_the_other() {
        let mut cursor = ListCursor::default();
        let (buffer, hits) = draw(90, 10, &secrets(), &mut cursor, |_| {});
        let drawn = text(&buffer);

        assert!(drawn.contains(" Secrets "), "{drawn}");
        assert!(drawn.contains("3/412 \u{b7} Name \u{2191}"), "{drawn}");
        assert!(drawn.contains("Name \u{2191}"), "the sorted header {drawn}");
        assert!(drawn.contains("Vault"), "{drawn}");
        assert!(drawn.contains("db-password"), "{drawn}");
        assert_eq!(
            hits.headers
                .iter()
                .map(|(column, _)| column.key())
                .collect::<Vec<_>>(),
            vec!["vault", "name", "enabled", "expires", "updated"],
        );
        assert_eq!(
            hits.rows
                .iter()
                .map(|(index, _)| *index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2],
        );
        // The header cell a click lands on is over the column it names.
        let (_, vault) = hits.headers[0];
        assert_eq!(
            buffer[(vault.x, hits.rows[0].1.y)].symbol(),
            "k",
            "the Vault header sits over the vault names"
        );
        assert_eq!(
            cursor.scroll.viewport, 6,
            "ten rows less two borders, the header and its rule"
        );
        assert_eq!(cursor.scroll.content, 412);
    }

    #[test]
    fn the_row_under_the_cursor_is_marked_and_the_one_under_the_pointer_is_washed() {
        let mut cursor = ListCursor::default();
        // What the frame before this one left the cursor knowing, which is
        // what puts row 1 on screen rather than scrolling until it is.
        table_geometry(Rect::new(0, 0, 90, 10)).window(&mut cursor, 412);
        cursor.focus(1);
        let (buffer, hits) = draw(90, 10, &secrets(), &mut cursor, |spec| {
            spec.hovered = Some(2)
        });

        let (_, selected) = hits.rows[1];
        assert_eq!(
            buffer[(selected.x, selected.y)].symbol(),
            "\u{203a}",
            "the selected row wears the marker"
        );
        assert_eq!(
            buffer[(selected.x, selected.y)].fg,
            theme().accent,
            "in the accent"
        );
        assert_eq!(
            buffer[(selected.x + 4, selected.y)].bg,
            theme().selected_background,
        );

        let (_, hovered) = hits.rows[2];
        let washed = &buffer[(hovered.x + 4, hovered.y)];
        let neighbour = &buffer[(hits.rows[0].1.x + 4, hits.rows[0].1.y)];
        if theme().hover_background == Color::Reset {
            // A palette with no tint to give reverses the row instead, so
            // that is the signal to look for and to look for the absence of.
            assert!(washed.modifier.contains(Modifier::REVERSED));
            assert!(
                !neighbour.modifier.contains(Modifier::REVERSED),
                "and its neighbours are left alone"
            );
        } else {
            assert_eq!(washed.bg, theme().hover_background);
            assert_ne!(
                neighbour.bg,
                theme().hover_background,
                "and its neighbours are left alone"
            );
        }
    }

    #[test]
    fn a_cell_keeps_the_colour_its_screen_gave_it_and_lights_up_what_matched() {
        let mut rows = secrets();
        rows[0][1] =
            Cell::styled("db-password", Style::default().fg(theme().warning)).matched(vec![0, 1]);
        let mut cursor = ListCursor::default();
        let (buffer, hits) = draw(90, 10, &rows, &mut cursor, |_| {});

        let name = hits.headers[1].1;
        let y = hits.rows[0].1.y;
        assert_eq!(
            buffer[(name.x, y)].fg,
            theme().search_match,
            "the two characters the query matched"
        );
        assert_eq!(buffer[(name.x + 1, y)].fg, theme().search_match);
        assert_eq!(
            buffer[(name.x + 2, y)].fg,
            theme().warning,
            "and the rest of the cell as the screen painted it"
        );
    }

    #[test]
    fn a_long_list_shows_the_window_it_was_handed_and_the_rest_stays_off_screen() {
        let all: Vec<Vec<Cell>> = (0..40)
            .map(|index| row(&["kv-prod", &format!("secret-{index:02}")]))
            .collect();
        let mut cursor = ListCursor::default();
        cursor.scroll.set_viewport(4, 40);
        cursor.focus(30);

        let window = &all[cursor.scroll.offset..cursor.scroll.offset + 4];
        let (buffer, hits) = draw(90, 8, window, &mut cursor, |spec| spec.total = 40);
        let drawn = text(&buffer);

        assert_eq!(
            hits.rows
                .iter()
                .map(|(index, _)| *index)
                .collect::<Vec<_>>(),
            vec![27, 28, 29, 30],
            "the window is numbered from the offset, not from zero"
        );
        assert!(drawn.contains("secret-30"), "{drawn}");
        assert!(!drawn.contains("secret-00"), "{drawn}");
        assert_eq!(
            buffer[(hits.rows[3].1.x, hits.rows[3].1.y)].symbol(),
            "\u{203a}",
            "the cursor is on the last row of the window"
        );
    }

    #[test]
    fn a_table_with_no_room_for_a_row_still_draws_its_frame() {
        let mut cursor = ListCursor::default();
        let (buffer, hits) = draw(90, 3, &secrets(), &mut cursor, |_| {});
        assert!(text(&buffer).contains(" Secrets "));
        assert!(hits.rows.is_empty(), "there is nowhere to put one");
        assert_eq!(cursor.scroll.viewport, 1, "and a page is never zero rows");
    }

    #[test]
    fn a_cell_a_column_was_dropped_out_from_under_is_not_drawn() {
        // A screen paints one cell per column it was told about; the tags
        // table in 52 cells has room for three of its four.
        let layout = TableLayout::new(TAG_COLUMNS);
        let columns = layout.visible_columns(TableLayout::available_width(50));
        assert_eq!(columns.len(), 3);

        let mut cursor = ListCursor::default();
        let rows = vec![row(&["1.42.0", "sha256:ab12ef01"])];
        let mut terminal = Terminal::new(TestBackend::new(52, 8)).unwrap();
        let hits = terminal
            .draw(|frame| {
                let mut spec = TableSpec {
                    title: " payments-api ".to_owned(),
                    status: String::new(),
                    focused: false,
                    columns: &columns,
                    sorted: None,
                    rows: &rows,
                    total: 1,
                    cursor: &mut cursor,
                    hovered: None,
                };
                render_list_table(frame, frame.area(), &mut spec);
            })
            .unwrap();
        let _ = hits;
        let drawn = text(terminal.backend().buffer());
        assert!(drawn.contains("1.42.0"), "{drawn}");
        assert!(drawn.contains("Digest"), "{drawn}");
        assert!(
            !drawn.contains("Updated"),
            "the column that did not fit went {drawn}"
        );
    }

    #[test]
    fn a_run_of_matched_characters_is_one_span_and_an_empty_query_is_none() {
        let base = Style::default().fg(theme().text);
        let matched = Style::default().fg(theme().search_match);

        let plain = highlight_line("db-password", &[], base, matched);
        assert_eq!(plain.spans.len(), 1);
        assert_eq!(plain.spans[0].style, base);

        let line = highlight_line("db-password", &[0, 1, 3, 4], base, matched);
        assert_eq!(
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<Vec<_>>(),
            vec!["db", "-", "pa", "ssword"],
        );
        assert_eq!(line.spans[0].style, matched);
        assert_eq!(line.spans[1].style, base);
        assert_eq!(line.spans[2].style, matched);

        // An index past the end, and one on the last character.
        let line = highlight_line("ab", &[1, 9], base, matched);
        assert_eq!(
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<Vec<_>>(),
            vec!["a", "b"],
        );
        assert_eq!(line.spans[1].style, matched);
        assert!(highlight_line("", &[0], base, matched).spans.is_empty());
        // Counted in characters, not bytes.
        let line = highlight_line("é-x", &[2], base, matched);
        assert_eq!(line.spans.last().unwrap().content.as_ref(), "x");
        assert_eq!(line.spans.last().unwrap().style, matched);
    }
}
