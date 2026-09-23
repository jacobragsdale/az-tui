//! One list's own state — cursor, search box, sort, layout, and the rows the
//! query leaves — over any kind of row.
//!
//! Every tab keeps four of these, one per kind, so switching kinds and back
//! finds the cursor where it was left.

use std::cmp::Ordering;

use super::cursor::ListCursor;
use super::{flip, none_last};
use crate::columns::{ColumnId, TableLayout, columns_for};
use crate::filter::{self, Query};
use crate::kube::{ConfigMap, K8sEvent, Kind, Pod, SecretMeta};
use crate::text_input::TextInput;

/// What one kind of row has to say about itself for the list to search,
/// sort and keep a cursor on it.
pub trait Row {
    /// The `key:` filters this kind knows. Everything else typed is a word.
    const SCHEMA: &'static [&'static str];
    /// The column the list opens sorted by.
    const DEFAULT_SORT: ColumnId;
    /// Every cell a person might type part of, joined once per read rather
    /// than per keystroke.
    fn haystack(&self) -> String;
    /// Whether the row answers every `key:value` in the query. The words are
    /// the haystack's job.
    fn passes(&self, query: &Query) -> bool;
    /// This row against another by one column, before the name breaks ties.
    /// The column's order, turned when `descending`. A row with nothing in
    /// the column sorts last whichever way it is turned.
    fn compare(&self, other: &Self, by: ColumnId, descending: bool) -> Ordering;
    /// What tells this row from every other, for putting the cursor back
    /// after a read.
    fn identity(&self) -> String;
}

/// Two names, compared without regard to ASCII case and without allocating.
pub fn cmp_ignore_ascii_case(left: &str, right: &str) -> Ordering {
    left.bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .cmp(right.bytes().map(|byte| byte.to_ascii_lowercase()))
}

impl Row for Pod {
    const SCHEMA: &'static [&'static str] = &["name", "ns", "status", "owner", "app", "node"];
    const DEFAULT_SORT: ColumnId = ColumnId::Name;

    fn haystack(&self) -> String {
        let mut text = String::with_capacity(96);
        text.push_str(&self.key.name);
        text.push(' ');
        text.push_str(&self.key.namespace);
        text.push(' ');
        text.push_str(&self.status);
        text.push(' ');
        text.push_str(self.owner_name());
        text.push(' ');
        text.push_str(&self.node);
        for container in &self.containers {
            text.push(' ');
            text.push_str(&container.image);
        }
        text
    }

    fn passes(&self, query: &Query) -> bool {
        query.fields.iter().all(|(key, value)| match key.as_str() {
            "name" => filter::contains(&self.key.name, value),
            "ns" => filter::contains(&self.key.namespace, value),
            "status" => filter::contains(&self.status, value),
            "owner" => filter::contains(self.owner_name(), value),
            "app" => self.app().is_some_and(|app| filter::contains(app, value)),
            "node" => filter::contains(&self.node, value),
            _ => true,
        })
    }

    fn compare(&self, other: &Self, by: ColumnId, descending: bool) -> Ordering {
        if by == ColumnId::Age {
            return none_last(self.created, other.created, !descending);
        }
        let text = |left: &str, right: &str| cmp_ignore_ascii_case(left, right);
        let ordering = match by {
            ColumnId::Name => text(&self.key.name, &other.key.name),
            ColumnId::Namespace => text(&self.key.namespace, &other.key.namespace),
            // How much of a pod is up first, then how big it is: `0/1` before
            // `1/2` before `2/2`.
            ColumnId::Ready => self.ready.cmp(&other.ready),
            ColumnId::Status => text(&self.status, &other.status),
            ColumnId::Restarts => self.restarts.cmp(&other.restarts),
            ColumnId::Node => text(&self.node, &other.node),
            ColumnId::Ip => text(&self.ip, &other.ip),
            ColumnId::Owner => text(self.owner_name(), other.owner_name()),
            ColumnId::Image => text(
                self.containers.first().map_or("", |c| c.image.as_str()),
                other.containers.first().map_or("", |c| c.image.as_str()),
            ),
            _ => Ordering::Equal,
        };
        flip(ordering, descending)
    }

    fn identity(&self) -> String {
        format!("{}/{}", self.key.namespace, self.key.name)
    }
}

impl Row for K8sEvent {
    const SCHEMA: &'static [&'static str] = &["type", "reason", "object", "kind", "message", "ns"];
    const DEFAULT_SORT: ColumnId = ColumnId::Age;

    fn haystack(&self) -> String {
        format!(
            "{} {} {} {} {}",
            self.kind,
            self.reason,
            self.object.slash(),
            self.message,
            self.namespace
        )
    }

    fn passes(&self, query: &Query) -> bool {
        query.fields.iter().all(|(key, value)| match key.as_str() {
            "type" => filter::contains(&self.kind, value),
            "reason" => filter::contains(&self.reason, value),
            "object" => filter::contains(&self.object.name, value),
            "kind" => filter::contains(&self.object.kind, value),
            "message" => filter::contains(&self.message, value),
            "ns" => filter::contains(&self.namespace, value),
            _ => true,
        })
    }

    fn compare(&self, other: &Self, by: ColumnId, descending: bool) -> Ordering {
        if by == ColumnId::Age {
            return none_last(self.last, other.last, !descending);
        }
        let text = |left: &str, right: &str| cmp_ignore_ascii_case(left, right);
        let ordering = match by {
            ColumnId::K8sType => text(&self.kind, &other.kind),
            ColumnId::Reason => text(&self.reason, &other.reason),
            ColumnId::Object => text(&self.object.name, &other.object.name),
            ColumnId::Count => other.count.cmp(&self.count),
            ColumnId::Message => text(&self.message, &other.message),
            _ => Ordering::Equal,
        };
        flip(ordering, descending)
    }

    fn identity(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }
}

impl Row for ConfigMap {
    const SCHEMA: &'static [&'static str] = &["name", "ns", "key"];
    const DEFAULT_SORT: ColumnId = ColumnId::Name;

    fn haystack(&self) -> String {
        let mut text = format!("{} {}", self.name, self.namespace);
        for (key, _) in &self.data {
            text.push(' ');
            text.push_str(key);
        }
        text
    }

    fn passes(&self, query: &Query) -> bool {
        query.fields.iter().all(|(key, value)| match key.as_str() {
            "name" => filter::contains(&self.name, value),
            "ns" => filter::contains(&self.namespace, value),
            "key" => self
                .data
                .iter()
                .any(|(held, _)| filter::contains(held, value)),
            _ => true,
        })
    }

    fn compare(&self, other: &Self, by: ColumnId, descending: bool) -> Ordering {
        if by == ColumnId::Age {
            return none_last(self.created, other.created, !descending);
        }
        let ordering = match by {
            ColumnId::Name => cmp_ignore_ascii_case(&self.name, &other.name),
            ColumnId::Namespace => cmp_ignore_ascii_case(&self.namespace, &other.namespace),
            ColumnId::Keys => other.data.len().cmp(&self.data.len()),
            _ => Ordering::Equal,
        };
        flip(ordering, descending)
    }

    fn identity(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }
}

impl Row for SecretMeta {
    const SCHEMA: &'static [&'static str] = &["name", "ns", "type", "key"];
    const DEFAULT_SORT: ColumnId = ColumnId::Name;

    fn haystack(&self) -> String {
        let mut text = format!("{} {} {}", self.name, self.namespace, self.kind);
        for (key, _) in &self.keys {
            text.push(' ');
            text.push_str(key);
        }
        text
    }

    fn passes(&self, query: &Query) -> bool {
        query.fields.iter().all(|(key, value)| match key.as_str() {
            "name" => filter::contains(&self.name, value),
            "ns" => filter::contains(&self.namespace, value),
            "type" => filter::contains(&self.kind, value),
            "key" => self
                .keys
                .iter()
                .any(|(held, _)| filter::contains(held, value)),
            _ => true,
        })
    }

    fn compare(&self, other: &Self, by: ColumnId, descending: bool) -> Ordering {
        if by == ColumnId::Age {
            return none_last(self.created, other.created, !descending);
        }
        let ordering = match by {
            ColumnId::Name => cmp_ignore_ascii_case(&self.name, &other.name),
            ColumnId::Namespace => cmp_ignore_ascii_case(&self.namespace, &other.namespace),
            ColumnId::K8sType => cmp_ignore_ascii_case(&self.kind, &other.kind),
            ColumnId::Keys => other.keys.len().cmp(&self.keys.len()),
            _ => Ordering::Equal,
        };
        flip(ordering, descending)
    }

    fn identity(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }
}

/// One list: its cursor, its search box, its sort, its columns, and the rows
/// the query leaves, in order.
#[derive(Debug)]
pub struct ListState {
    pub kind: Kind,
    pub cursor: ListCursor,
    pub layout: TableLayout,
    pub input: TextInput,
    pub sort: ColumnId,
    pub descending: bool,
    /// Which rows are shown, in the order they are shown.
    visible: Vec<usize>,
    /// Every row, in sort order. A query filters this rather than the rows,
    /// so the shown rows come out sorted without being sorted again.
    sorted: Vec<usize>,
    /// One searchable string per row, built when the rows change rather
    /// than when the query does.
    haystacks: Vec<String>,
    /// What `sorted` was last built from.
    ordered_for: Option<(ColumnId, bool, usize)>,
    /// What `visible` was last built from, so a redraw that changed nothing
    /// does not rebuild it.
    built_for: Option<(String, ColumnId, bool, usize)>,
    /// The width the columns were last solved at, which is what `S` walks.
    available: u16,
}

impl ListState {
    #[must_use]
    pub fn new(kind: Kind, default_sort: ColumnId) -> Self {
        Self {
            kind,
            cursor: ListCursor::default(),
            layout: TableLayout::new(columns_for(kind)),
            input: TextInput::default(),
            sort: default_sort,
            descending: false,
            visible: Vec::new(),
            sorted: Vec::new(),
            haystacks: Vec::new(),
            ordered_for: None,
            built_for: None,
            available: 0,
        }
    }

    /// The rows on screen, as indices into the kind's rows.
    #[must_use]
    pub fn visible(&self) -> &[usize] {
        &self.visible
    }

    /// The index into the rows of the one under the cursor.
    #[must_use]
    pub fn selected_index(&self) -> Option<usize> {
        self.visible.get(self.cursor.index).copied()
    }

    /// Rebuilds the shown rows when the query, the sort or the rows have
    /// moved. Cheap to call every frame: it compares first, and a keystroke
    /// only ever re-runs the filter.
    pub fn refilter<T: Row>(&mut self, rows: &[T]) {
        let order_key = (self.sort, self.descending, rows.len());
        if self.ordered_for != Some(order_key) {
            self.ordered_for = Some(order_key);
            self.built_for = None;
            self.reorder(rows);
        }
        let key = (
            self.input.text().to_owned(),
            self.sort,
            self.descending,
            rows.len(),
        );
        if self.built_for.as_ref() == Some(&key) {
            return;
        }
        self.built_for = Some(key);

        let parsed = Query::parse(self.input.text(), T::SCHEMA);
        let words = crate::search::Query::new(&parsed.words);
        self.visible = self
            .sorted
            .iter()
            .copied()
            .filter(|at| rows[*at].passes(&parsed) && words.matches(&self.haystacks[*at]))
            .collect();
        self.cursor.clamp(self.visible.len());
    }

    /// Every row, in sort order, and the searchable text of each. Run when
    /// the rows or the sort change, not once a keystroke of the query.
    fn reorder<T: Row>(&mut self, rows: &[T]) {
        if self.haystacks.len() != rows.len() {
            self.haystacks = rows.iter().map(Row::haystack).collect();
        }
        self.sorted = (0..rows.len()).collect();
        let (by, descending) = (self.sort, self.descending);
        let ids: Vec<String> = rows.iter().map(Row::identity).collect();
        self.sorted.sort_by(|a, b| {
            let ordering = rows[*a].compare(&rows[*b], by, descending);
            ordering.then_with(|| cmp_ignore_ascii_case(&ids[*a], &ids[*b]))
        });
    }

    /// Forces the next `refilter` to do the work, after the rows underneath
    /// have moved.
    pub fn invalidate(&mut self) {
        self.built_for = None;
        self.ordered_for = None;
        self.haystacks.clear();
    }

    /// After a read: back onto the same row if it is still shown, wherever
    /// it now sorts.
    pub fn keep_cursor<T: Row>(&mut self, rows: &[T], was: Option<String>) {
        self.refilter(rows);
        let Some(identity) = was else {
            self.cursor.clamp(self.visible.len());
            return;
        };
        match self
            .visible
            .iter()
            .position(|at| rows[*at].identity() == identity)
        {
            Some(at) => self.cursor.focus(at),
            None => self.cursor.clamp(self.visible.len()),
        }
    }

    /// What the cursor is on, by identity rather than by position.
    #[must_use]
    pub fn cursor_identity<T: Row>(&self, rows: &[T]) -> Option<String> {
        self.selected_index().map(|at| rows[at].identity())
    }

    /// Puts the cursor on the row with this identity, clearing the query if
    /// that is what hides it. Says whether it was found.
    pub fn select<T: Row>(&mut self, rows: &[T], identity: &str) -> bool {
        self.refilter(rows);
        let find = |list: &Self| {
            list.visible
                .iter()
                .position(|at| rows[*at].identity() == identity)
        };
        let at = match find(self) {
            Some(at) => at,
            None => {
                self.input.clear();
                self.refilter(rows);
                match find(self) {
                    Some(at) => at,
                    None => return false,
                }
            }
        };
        self.cursor.focus(at);
        true
    }

    /// `S`: the next column on screen.
    pub fn next_sort(&mut self) {
        if let Some(next) = self.layout.next_sort(self.sort, self.available) {
            self.sort = next;
            self.descending = false;
        }
    }

    /// A header click: the same column cycles ascending, descending, then
    /// back to the default; a different column starts ascending.
    pub fn sort_by(&mut self, column: ColumnId, default: ColumnId) {
        if self.sort == column {
            if self.descending {
                self.sort = default;
                self.descending = false;
            } else {
                self.descending = true;
            }
        } else {
            self.sort = column;
            self.descending = false;
        }
    }

    /// What the bottom border says: how many rows of how many, and the sort.
    #[must_use]
    pub fn status(&self, total: usize) -> String {
        let arrow = if self.descending { "↓" } else { "↑" };
        let shown = if self.visible.len() == total {
            format!("{total}")
        } else {
            format!("{}/{total}", self.visible.len())
        };
        format!("{shown} · {} {arrow}", self.sort.label())
    }

    /// Remembers the width the columns were solved at, so `S` walks the
    /// columns that are actually on screen.
    pub fn note_width(&mut self, available: u16) {
        self.available = available;
    }

    /// Moves the cursor by `delta`, or a page, clamped to the rows shown.
    /// Says whether it moved.
    pub fn move_cursor(&mut self, delta: isize, pages: bool) -> bool {
        let before = self.cursor.index;
        let count = self.visible.len();
        if pages {
            self.cursor.page(delta, count);
        } else {
            self.cursor.move_by(delta, count);
        }
        self.cursor.index != before
    }

    /// A click on a row. Says whether the cursor moved.
    pub fn click_row(&mut self, index: usize) -> bool {
        let before = self.cursor.index;
        self.cursor
            .focus(index.min(self.visible.len().saturating_sub(1)));
        self.cursor.index != before
    }

    /// The wheel over the table: the viewport moves and the cursor follows
    /// it rather than being left behind, so what a key acts on is always
    /// something on screen. Says whether the cursor moved.
    pub fn wheel(&mut self, delta: i32) -> bool {
        self.cursor.wheel(delta, self.visible.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube::tests::{crashing, pod};
    use crate::timestamp::Timestamp;

    fn pods() -> Vec<Pod> {
        let mut old = pod("qa", "dev", "billing-worker-1a2b3c-old01", "Completed");
        old.created = Timestamp::parse("2026-01-01T00:00:00Z");
        old.owner = Some(("Job".to_owned(), "billing-worker".to_owned()));
        old.ready = (0, 1);
        vec![
            pod("qa", "dev", "orders-api-7d9f5b-k9x2p", "Running"),
            crashing("qa", "dev", "orders-api-7d9f5b-abc12"),
            old,
        ]
    }

    fn names(list: &ListState, rows: &[Pod]) -> Vec<String> {
        list.visible()
            .iter()
            .map(|at| rows[*at].key.name.clone())
            .collect()
    }

    #[test]
    fn the_table_opens_by_name_and_a_query_narrows_it_by_word_and_by_field() {
        let rows = pods();
        let mut list = ListState::new(Kind::Pods, ColumnId::Name);
        list.refilter(&rows);
        assert_eq!(
            names(&list, &rows),
            [
                "billing-worker-1a2b3c-old01",
                "orders-api-7d9f5b-abc12",
                "orders-api-7d9f5b-k9x2p"
            ]
        );
        list.input.set_text("status:crash");
        list.refilter(&rows);
        assert_eq!(names(&list, &rows), ["orders-api-7d9f5b-abc12"]);
        list.input.set_text("orders-api:1.2.3");
        list.refilter(&rows);
        assert_eq!(names(&list, &rows).len(), 3, "an image reference is a word");
        list.input.set_text("owner:billing");
        list.refilter(&rows);
        assert_eq!(names(&list, &rows), ["billing-worker-1a2b3c-old01"]);
        list.input.set_text("app:orders-api k9x");
        list.refilter(&rows);
        assert_eq!(names(&list, &rows), ["orders-api-7d9f5b-k9x2p"]);
        list.input.set_text("nothing-like-this");
        list.refilter(&rows);
        assert!(names(&list, &rows).is_empty());
        assert_eq!(list.status(rows.len()), "0/3 · Name ↑");
    }

    #[test]
    fn a_header_click_sorts_by_the_column_and_age_puts_the_newest_first() {
        let rows = pods();
        let mut list = ListState::new(Kind::Pods, ColumnId::Name);
        list.sort_by(ColumnId::Restarts, ColumnId::Name);
        list.refilter(&rows);
        assert_eq!(names(&list, &rows)[0], "billing-worker-1a2b3c-old01");
        list.sort_by(ColumnId::Restarts, ColumnId::Name);
        list.refilter(&rows);
        assert_eq!(
            names(&list, &rows)[0],
            "orders-api-7d9f5b-abc12",
            "the same header again turns it round"
        );
        list.sort_by(ColumnId::Restarts, ColumnId::Name);
        assert_eq!(list.sort, ColumnId::Name, "and a third time is the default");
        list.sort_by(ColumnId::Age, ColumnId::Name);
        list.refilter(&rows);
        assert_eq!(
            names(&list, &rows).last().map(String::as_str),
            Some("billing-worker-1a2b3c-old01"),
            "the oldest last"
        );
        list.sort_by(ColumnId::Ready, ColumnId::Name);
        list.refilter(&rows);
        assert_eq!(
            names(&list, &rows)[2],
            "orders-api-7d9f5b-k9x2p",
            "1/1 after the 0/1s"
        );
    }

    #[test]
    fn name_turned_round_puts_the_last_name_first_in_every_namespace() {
        let mut rows = pods();
        rows.push(pod("qa", "zzz", "aardvark-0", "Running"));
        let mut list = ListState::new(Kind::Pods, ColumnId::Name);
        list.sort_by(ColumnId::Name, ColumnId::Name);
        assert!(list.descending);
        list.refilter(&rows);
        let shown = names(&list, &rows);
        assert_eq!(shown[0], "orders-api-7d9f5b-k9x2p");
        assert_eq!(shown[3], "aardvark-0", "by name, not by namespace");
    }

    #[test]
    fn a_re_read_leaves_the_cursor_on_the_row_it_was_on_and_select_finds_a_hidden_one() {
        let mut rows = pods();
        let mut list = ListState::new(Kind::Pods, ColumnId::Name);
        list.refilter(&rows);
        list.cursor.focus(2);
        let was = list.cursor_identity(&rows);
        assert_eq!(was.as_deref(), Some("dev/orders-api-7d9f5b-k9x2p"));

        rows.insert(0, pod("qa", "dev", "orders-api-7d9f5b-aaa01", "Running"));
        rows.remove(3);
        list.invalidate();
        list.keep_cursor(&rows, was);
        assert_eq!(list.cursor.index, 2);
        assert_eq!(
            list.cursor_identity(&rows).as_deref(),
            Some("dev/orders-api-7d9f5b-k9x2p")
        );

        list.input.set_text("aaa01");
        assert!(list.select(&rows, "dev/orders-api-7d9f5b-abc12"));
        assert!(list.input.is_empty(), "the query hid it, so the query went");
        assert_eq!(
            list.cursor_identity(&rows).as_deref(),
            Some("dev/orders-api-7d9f5b-abc12")
        );
        assert!(!list.select(&rows, "dev/nothing"));

        let was = list.cursor_identity(&rows);
        rows.clear();
        list.invalidate();
        list.keep_cursor(&rows, was);
        assert_eq!(list.cursor.index, 0);
        assert!(list.selected_index().is_none());
    }

    #[test]
    fn events_open_newest_first_and_the_other_kinds_by_name() {
        let mut older = K8sEvent::from_json(&serde_json::json!({
            "metadata": {"name": "a", "namespace": "dev"},
            "lastTimestamp": "2026-09-12T10:00:00Z", "type": "Normal", "reason": "Pulled",
            "involvedObject": {"kind": "Pod", "name": "p1"}, "message": "pulled"
        }))
        .unwrap();
        let newer = K8sEvent::from_json(&serde_json::json!({
            "metadata": {"name": "b", "namespace": "dev"},
            "lastTimestamp": "2026-09-12T12:00:00Z", "type": "Warning", "reason": "BackOff",
            "involvedObject": {"kind": "Pod", "name": "p2"}, "message": "back-off", "count": 9
        }))
        .unwrap();
        older.count = 2;
        let rows = vec![older, newer];
        let mut list = ListState::new(Kind::Events, K8sEvent::DEFAULT_SORT);
        list.refilter(&rows);
        assert_eq!(list.visible(), [1, 0], "newest first");
        list.input.set_text("type:warn");
        list.refilter(&rows);
        assert_eq!(list.visible(), [1]);
        list.input.set_text("p1");
        list.refilter(&rows);
        assert_eq!(list.visible(), [0], "the object's name is a word");
        list.input.clear();
        list.sort_by(ColumnId::Count, ColumnId::Age);
        list.refilter(&rows);
        assert_eq!(list.visible(), [1, 0], "the most times first");

        let maps = vec![
            ConfigMap::from_json(&serde_json::json!({"metadata": {"name": "zeta", "namespace": "dev"}, "data": {"K": "v"}})).unwrap(),
            ConfigMap::from_json(&serde_json::json!({"metadata": {"name": "alpha", "namespace": "dev"}, "data": {"LOG_LEVEL": "info", "B": "2"}})).unwrap(),
        ];
        let mut list = ListState::new(Kind::ConfigMaps, ConfigMap::DEFAULT_SORT);
        list.refilter(&maps);
        assert_eq!(list.visible(), [1, 0]);
        list.input.set_text("key:log");
        list.refilter(&maps);
        assert_eq!(list.visible(), [1]);
        list.input.clear();
        list.sort_by(ColumnId::Keys, ColumnId::Name);
        list.refilter(&maps);
        assert_eq!(list.visible(), [1, 0], "the most keys first");
    }

    #[test]
    fn an_event_with_no_stamp_sorts_last_whichever_way_age_is_turned() {
        let stamped = K8sEvent::from_json(&serde_json::json!({
            "metadata": {"name": "a", "namespace": "dev"},
            "lastTimestamp": "2026-09-12T10:00:00Z", "type": "Normal", "reason": "Pulled",
            "involvedObject": {"kind": "Pod", "name": "p1"}, "message": "pulled"
        }))
        .unwrap();
        let unstamped = K8sEvent::from_json(&serde_json::json!({
            "metadata": {"name": "z", "namespace": "dev"},
            "type": "Normal", "reason": "Pulled",
            "involvedObject": {"kind": "Pod", "name": "p2"}, "message": "no stamp at all"
        }))
        .unwrap();
        assert!(unstamped.last.is_none());
        let rows = vec![unstamped, stamped];
        let mut list = ListState::new(Kind::Events, K8sEvent::DEFAULT_SORT);
        list.refilter(&rows);
        assert_eq!(list.visible(), [1, 0], "newest first, the unstamped last");
        list.sort_by(ColumnId::Age, ColumnId::Age);
        assert!(list.descending);
        list.refilter(&rows);
        assert_eq!(
            list.visible(),
            [1, 0],
            "turned round, the unstamped is still last"
        );
    }
}
