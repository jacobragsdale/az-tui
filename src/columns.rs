//! The columns a list table shows: which ones, in what order, how wide, and
//! what it drops first when the terminal is too narrow for all of them.
//!
//! Lifted from ticket-tui, with its per-screen `ColumnId` trait collapsed into
//! one enum. There, every screen brings its own column type and `TableLayout`
//! is generic over it; here the three tables — secrets, repositories, and one
//! repository's tags — share `Updated` and `Created`, and the table is handed
//! rows that are already painted, so nothing downstream ever matches on a
//! column type. One enum and three ordered slices of it is the whole of it:
//! no trait, no generic parameter to thread through `TableSpec`, and one
//! `spec()` match rather than six trait methods per screen. What a column
//! *starts out* visible as belongs to the table and not to the column —
//! `Created` opens hidden on secrets and shown on tags — so the slices carry
//! that, and everything else rides on the variant.

use ratatui::layout::{Alignment, Constraint};

/// The two columns the selection marker (`› `) is always given, whether or
/// not the row under the cursor is on screen.
pub const SELECTION_WIDTH: u16 = 2;

/// The scrollbar's own column, at the right edge of every list table. It is
/// reserved whether or not the list overflows, so a table does not shuffle
/// sideways as rows arrive.
pub const SCROLLBAR_WIDTH: u16 = 1;

/// The blank column between two neighbouring cells.
pub const COLUMN_SPACING: u16 = 1;

/// The fewest cells any column is drawn in. Under three there is no room for
/// a value and the sign that it was cut.
pub const MIN_COLUMN_WIDTH: u16 = 3;

/// The fewest cells a flexible column is squeezed to before the table starts
/// dropping optional columns from the right, unless that column asks for
/// something else. Below this a name stops being a name — `payments-ap` — and
/// whatever took the room is worth less than what lost it.
pub const MIN_FLEXIBLE_WIDTH: u16 = 24;

/// Every column any of the three tables offers.
///
/// The variants are shared where the columns are: a repository's tags say
/// when they were updated in the same words, and with the same header, as a
/// secret does.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ColumnId {
    // The Secrets table.
    Vault,
    Name,
    Enabled,
    Expires,
    Type,
    // The Registries table, showing repositories.
    Registry,
    Repository,
    Tags,
    Manifests,
    // The Registries table, showing one repository's tags.
    Tag,
    Digest,
    // Every table says when a row last changed, and can be asked when it
    // first appeared.
    Updated,
    Created,
}

/// What one column is: what the session file calls it, what its header says,
/// and how much room it wants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColumnSpec {
    /// The identity. It is what the session file records and what a clicked
    /// header carries back to the screen that drew it, so it is lowercase and
    /// it does not change between releases: renaming one silently drops
    /// somebody's stored width.
    pub key: &'static str,
    /// What the header cell says, which is not always the name a sort menu
    /// uses: a header has less room.
    pub label: &'static str,
    /// What the column opens at. The flexible column has none and takes
    /// whatever is left over instead.
    pub width: u16,
    /// The fewest cells the column is drawn in: the floor the flexible column
    /// is squeezed to while there is still an optional column to drop, and a
    /// hard floor for everything else.
    pub min: u16,
    /// The one column per table that takes the width left over. Its stored
    /// width is ignored.
    pub flexible: bool,
    /// Numbers read better against the right edge of their cell.
    pub align: Alignment,
    /// Whether the column stays whatever happens: the auto-drop never takes a
    /// pinned column away, so a table keeps the column that says which vault
    /// or registry a row is in, and the name of the thing itself, however
    /// narrow the terminal gets.
    pub pinned: bool,
}

impl ColumnSpec {
    const fn fixed(key: &'static str, label: &'static str, width: u16) -> Self {
        Self {
            key,
            label,
            width,
            min: MIN_COLUMN_WIDTH,
            flexible: false,
            align: Alignment::Left,
            pinned: false,
        }
    }

    /// A count, which is a fixed column read against its right edge.
    const fn count(key: &'static str, label: &'static str, width: u16) -> Self {
        Self {
            align: Alignment::Right,
            ..Self::fixed(key, label, width)
        }
    }

    /// The column a table is really about, which is always the one that takes
    /// the leftover width and always one it will not drop.
    const fn flexible(key: &'static str, label: &'static str, min: u16) -> Self {
        Self {
            width: 0,
            min,
            flexible: true,
            pinned: true,
            ..Self::fixed(key, label, 0)
        }
    }

    /// A fixed column the table keeps however narrow it gets.
    const fn pinned(key: &'static str, label: &'static str, width: u16) -> Self {
        Self {
            pinned: true,
            ..Self::fixed(key, label, width)
        }
    }
}

impl ColumnId {
    /// Every column there is, which is what a key out of the session file is
    /// resolved against.
    pub const ALL: [Self; 13] = [
        Self::Vault,
        Self::Name,
        Self::Enabled,
        Self::Expires,
        Self::Type,
        Self::Registry,
        Self::Repository,
        Self::Tags,
        Self::Manifests,
        Self::Tag,
        Self::Digest,
        Self::Updated,
        Self::Created,
    ];

    #[must_use]
    pub const fn spec(self) -> ColumnSpec {
        match self {
            Self::Vault => ColumnSpec::pinned("vault", "Vault", 12),
            Self::Name => ColumnSpec::flexible("name", "Name", MIN_FLEXIBLE_WIDTH),
            Self::Enabled => ColumnSpec::fixed("enabled", "Enabled", 7),
            Self::Expires => ColumnSpec::fixed("expires", "Expires", 9),
            Self::Type => ColumnSpec::fixed("type", "Type", 14),
            Self::Registry => ColumnSpec::pinned("registry", "Registry", 10),
            // A repository name is shorter than a work item's title and
            // rarely worth 24 cells, so it says so.
            Self::Repository => ColumnSpec::flexible("repository", "Repository", 20),
            Self::Tags => ColumnSpec::count("tag_count", "Tags", 5),
            Self::Manifests => ColumnSpec::count("manifest_count", "Manifests", 9),
            Self::Tag => ColumnSpec::flexible("tag", "Tag", 16),
            Self::Digest => ColumnSpec::fixed("digest", "Digest", 20),
            Self::Updated => ColumnSpec::fixed("updated", "Updated", 8),
            Self::Created => ColumnSpec::fixed("created", "Created", 8),
        }
    }

    #[must_use]
    pub const fn key(self) -> &'static str {
        self.spec().key
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        self.spec().label
    }

    /// The column that key names. An unknown key comes out of a session file
    /// written by an older build, or off a header of some other table, and is
    /// dropped.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|column| column.key() == key)
    }
}

/// One column as a table currently has it: its identity, plus the two things
/// a user is allowed to change about it and the session file remembers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColumnConfig {
    pub id: ColumnId,
    pub visible: bool,
    pub width: u16,
}

impl ColumnConfig {
    #[must_use]
    pub const fn shown(id: ColumnId) -> Self {
        Self {
            id,
            visible: true,
            width: id.spec().width,
        }
    }

    /// A column the table offers but does not open with: room it would rather
    /// spend on the name until somebody asks for it back.
    #[must_use]
    pub const fn hidden(id: ColumnId) -> Self {
        Self {
            visible: false,
            ..Self::shown(id)
        }
    }
}

/// The Secrets table, in the order it opens with.
pub const SECRET_COLUMNS: &[ColumnConfig] = &[
    ColumnConfig::shown(ColumnId::Vault),
    ColumnConfig::shown(ColumnId::Name),
    ColumnConfig::shown(ColumnId::Enabled),
    ColumnConfig::shown(ColumnId::Expires),
    ColumnConfig::shown(ColumnId::Updated),
    ColumnConfig::hidden(ColumnId::Type),
    ColumnConfig::hidden(ColumnId::Created),
];

/// The Registries table at its first level, one row per repository.
pub const REPOSITORY_COLUMNS: &[ColumnConfig] = &[
    ColumnConfig::shown(ColumnId::Registry),
    ColumnConfig::shown(ColumnId::Repository),
    ColumnConfig::shown(ColumnId::Tags),
    // Manifests sits where it belongs rather than at the end, so turning it
    // on does not move it: a hidden column still holds its place.
    ColumnConfig::hidden(ColumnId::Manifests),
    ColumnConfig::shown(ColumnId::Updated),
    ColumnConfig::hidden(ColumnId::Created),
];

/// The Registries table at its second level, one row per tag of the
/// repository the cursor opened.
pub const TAG_COLUMNS: &[ColumnConfig] = &[
    ColumnConfig::shown(ColumnId::Tag),
    ColumnConfig::shown(ColumnId::Digest),
    ColumnConfig::shown(ColumnId::Created),
    ColumnConfig::shown(ColumnId::Updated),
];

/// One table's columns as they stand: what it opened with, plus whatever the
/// session file or the user has done to them since.
///
// ponytail: nothing edits a layout in v1 — there is no Columns overlay, so
// `visible` and `width` only ever move when step 09 restores them, which it
// does by writing `columns` directly. An overlay would want ticket-tui's
// `toggle_visible`/`move_column`/`resize` back, and they are three matches on
// `pinned` away.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableLayout {
    pub columns: Vec<ColumnConfig>,
}

impl TableLayout {
    /// A table opened at its defaults, which are one of the three slices
    /// above.
    #[must_use]
    pub fn new(defaults: &[ColumnConfig]) -> Self {
        Self {
            columns: defaults.to_vec(),
        }
    }

    /// The width the columns and the gaps between them share, inside a pane
    /// `inner_width` wide: what the selection marker and the scrollbar take is
    /// spent before a column sees any of it.
    #[must_use]
    pub const fn available_width(inner_width: u16) -> u16 {
        inner_width
            .saturating_sub(SELECTION_WIDTH)
            .saturating_sub(SCROLLBAR_WIDTH)
    }

    /// The columns this table draws in `available` cells, dropping the
    /// right-most unpinned one for as long as the flexible column would
    /// otherwise fall under its minimum. A pinned column never goes, so a
    /// table always keeps its identity and its name — a narrow terminal is
    /// still worth reading, and a table that had dropped the name column
    /// would not be.
    #[must_use]
    pub fn visible_columns(&self, available: u16) -> Vec<ColumnConfig> {
        let mut columns: Vec<_> = self
            .columns
            .iter()
            .copied()
            .filter(|column| column.visible)
            .collect();
        while required_width(&columns) > available {
            let Some(index) = columns.iter().rposition(|column| !column.id.spec().pinned) else {
                break;
            };
            columns.remove(index);
        }
        columns
    }

    #[must_use]
    pub const fn constraint(column: ColumnConfig) -> Constraint {
        if column.id.spec().flexible || column.width == 0 {
            Constraint::Fill(1)
        } else {
            Constraint::Length(column.width)
        }
    }
}

/// What these columns need to draw with the flexible one still readable:
/// every fixed width, the flexible column's own minimum, and a gap between
/// each pair.
fn required_width(columns: &[ColumnConfig]) -> u16 {
    let spacing = COLUMN_SPACING.saturating_mul(
        u16::try_from(columns.len())
            .unwrap_or(u16::MAX)
            .saturating_sub(1),
    );
    columns
        .iter()
        .map(|column| {
            let spec = column.id.spec();
            if spec.flexible {
                spec.min
            } else {
                column.width.max(spec.min)
            }
        })
        .fold(spacing, u16::saturating_add)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the flexible column is left with once the fixed ones and the gaps
    /// between them are paid for.
    fn flexible_width(columns: &[ColumnConfig], available: u16) -> u16 {
        let spacing = COLUMN_SPACING * (columns.len() as u16 - 1);
        let fixed: u16 = columns
            .iter()
            .filter(|column| !column.id.spec().flexible)
            .map(|column| column.width)
            .sum();
        available.saturating_sub(spacing).saturating_sub(fixed)
    }

    fn ids(columns: &[ColumnConfig]) -> Vec<ColumnId> {
        columns.iter().map(|column| column.id).collect()
    }

    #[test]
    fn columns_drop_from_the_right_before_the_name_is_squeezed() {
        let layout = TableLayout::new(SECRET_COLUMNS);
        for pane in [140_u16, 110, 90, 70, 55] {
            // What the pane leaves the columns: its own border, the selection
            // marker and the scrollbar.
            let available = TableLayout::available_width(pane - 2);
            let columns = layout.visible_columns(available);
            let visible = ids(&columns);

            assert_eq!(visible[0], ColumnId::Vault, "the pinned columns stay");
            assert_eq!(visible[1], ColumnId::Name);
            assert!(
                flexible_width(&columns, available) >= MIN_FLEXIBLE_WIDTH,
                "{pane} left the name {} wide with {visible:?}",
                flexible_width(&columns, available)
            );
            // Whatever went, went off the right-hand end.
            let mut ordered = visible.clone();
            ordered.sort_by_key(|id| {
                SECRET_COLUMNS
                    .iter()
                    .position(|column| column.id == *id)
                    .unwrap()
            });
            assert_eq!(ordered, visible, "the columns keep their order");
        }

        assert_eq!(
            ids(&layout.visible_columns(TableLayout::available_width(158))),
            vec![
                ColumnId::Vault,
                ColumnId::Name,
                ColumnId::Enabled,
                ColumnId::Expires,
                ColumnId::Updated,
            ],
            "a wide enough table keeps every column it opened with"
        );
    }

    #[test]
    fn a_table_too_narrow_for_anything_keeps_what_says_which_row_it_is() {
        // Narrower than the two pinned columns want, and nothing left to
        // drop: the name takes what is there rather than the table dropping
        // the column that says which vault a secret is in.
        let secrets = TableLayout::new(SECRET_COLUMNS);
        let cramped = TableLayout::available_width(MIN_WIDTH_INNER);
        let columns = secrets.visible_columns(cramped);
        assert_eq!(ids(&columns), vec![ColumnId::Vault, ColumnId::Name]);
        assert!(flexible_width(&columns, cramped) > 0);

        assert_eq!(
            ids(&secrets.visible_columns(0)),
            vec![ColumnId::Vault, ColumnId::Name],
            "a table with no room at all still says what its rows are"
        );
        assert_eq!(
            ids(&TableLayout::new(REPOSITORY_COLUMNS).visible_columns(0)),
            vec![ColumnId::Registry, ColumnId::Repository],
        );
        // The tags table pins only its name, so it is the one that can be cut
        // back to a single column.
        assert_eq!(
            ids(&TableLayout::new(TAG_COLUMNS).visible_columns(0)),
            vec![ColumnId::Tag],
        );
    }

    /// The smallest table the app draws at all, from `ui::MIN_WIDTH`, less
    /// its border.
    const MIN_WIDTH_INNER: u16 = crate::ui::MIN_WIDTH - 2;

    #[test]
    fn each_table_drops_its_optional_columns_in_turn() {
        let repositories = TableLayout::new(REPOSITORY_COLUMNS);
        assert_eq!(
            ids(&repositories.visible_columns(200)),
            vec![
                ColumnId::Registry,
                ColumnId::Repository,
                ColumnId::Tags,
                ColumnId::Updated,
            ],
            "Manifests and Created are offered, not opened with"
        );
        assert_eq!(
            ids(&repositories.visible_columns(45)),
            vec![ColumnId::Registry, ColumnId::Repository, ColumnId::Tags],
            "Updated goes before the count that is the point of the row"
        );

        let tags = TableLayout::new(TAG_COLUMNS);
        assert_eq!(
            ids(&tags.visible_columns(70)),
            vec![
                ColumnId::Tag,
                ColumnId::Digest,
                ColumnId::Created,
                ColumnId::Updated,
            ]
        );
        assert_eq!(
            ids(&tags.visible_columns(50)),
            vec![ColumnId::Tag, ColumnId::Digest, ColumnId::Created],
        );
    }

    #[test]
    fn a_hidden_column_holds_its_place_until_it_is_turned_on() {
        let mut layout = TableLayout::new(REPOSITORY_COLUMNS);
        let manifests = layout
            .columns
            .iter()
            .position(|column| column.id == ColumnId::Manifests)
            .expect("the table offers a Manifests column");
        assert!(!layout.columns[manifests].visible, "nobody asked for it");

        layout.columns[manifests].visible = true;
        assert_eq!(
            ids(&layout.visible_columns(200)),
            vec![
                ColumnId::Registry,
                ColumnId::Repository,
                ColumnId::Tags,
                ColumnId::Manifests,
                ColumnId::Updated,
            ],
            "and it comes back where it always was"
        );
    }

    #[test]
    fn every_key_is_its_own_and_survives_the_round_trip() {
        let mut keys: Vec<&str> = ColumnId::ALL.iter().map(|id| id.key()).collect();
        keys.sort_unstable();
        let count = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), count, "two columns cannot share a session key");

        for id in ColumnId::ALL {
            assert_eq!(ColumnId::from_key(id.key()), Some(id));
            let spec = id.spec();
            assert!(
                spec.key.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{spec:?} is not a stable lowercase key"
            );
            assert!(
                u16::try_from(spec.label.chars().count()).unwrap() <= spec.width.max(spec.min),
                "{spec:?} cannot show its own header"
            );
        }
        assert_eq!(ColumnId::from_key("state"), None, "an older build's column");
        // The two that could have collided: the tag itself and how many of
        // them a repository has.
        assert_ne!(ColumnId::Tag.key(), ColumnId::Tags.key());
    }

    #[test]
    fn the_flexible_column_fills_and_the_rest_are_what_they_say() {
        let columns = TableLayout::new(SECRET_COLUMNS).visible_columns(120);
        let constraints: Vec<_> = columns.into_iter().map(TableLayout::constraint).collect();
        assert_eq!(constraints[0], Constraint::Length(12));
        assert_eq!(
            constraints[1],
            Constraint::Fill(1),
            "the name takes the rest"
        );
        assert_eq!(constraints[2], Constraint::Length(7));

        assert_eq!(
            TableLayout::available_width(100),
            97,
            "the marker and the scrollbar are spent first"
        );
        assert_eq!(TableLayout::available_width(2), 0, "and cannot overdraw");
    }
}
