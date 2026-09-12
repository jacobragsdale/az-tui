//! Literal, in-memory matching over whatever is on screen.
//!
//! Every whitespace-separated word in the query must appear in the row as a
//! substring. Scattered letters match nothing: `tks` does not find
//! `ticket search`, which is what makes typing a secret's name a reliable way
//! to find it rather than a guess.
//!
//! There is no relevance score and no ordering of its own — rows keep the
//! table's sort — and no worker thread, because ten thousand rows re-filter
//! in well under a millisecond on the main thread.
//!
// ponytail: matching on the main thread on every keystroke. Past roughly
// 50,000 rows this wants the worker and a generation counter, which is what
// ticket-tui's search.rs does; below that a debounce would only add latency.

use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// One query, compiled once and asked about many rows.
pub struct Query {
    pattern: Pattern,
    matcher: Matcher,
    buffer: Vec<char>,
    empty: bool,
}

impl Query {
    #[must_use]
    pub fn new(words: &[String]) -> Self {
        let joined = words.join(" ");
        Self {
            pattern: Pattern::new(
                &joined,
                CaseMatching::Ignore,
                Normalization::Smart,
                AtomKind::Substring,
            ),
            matcher: Matcher::new(Config::DEFAULT),
            buffer: Vec::new(),
            empty: joined.trim().is_empty(),
        }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.empty
    }

    /// Whether every word of the query is somewhere in this row's text.
    pub fn matches(&mut self, haystack: &str) -> bool {
        if self.empty {
            return true;
        }
        let haystack = Utf32Str::new(haystack, &mut self.buffer);
        self.pattern.score(haystack, &mut self.matcher).is_some()
    }
}

/// Which characters of one cell a query lit up, so the table can paint them
/// in `search_match`.
pub struct Highlighter {
    pattern: Pattern,
    matcher: Matcher,
    buffer: Vec<char>,
}

impl Highlighter {
    #[must_use]
    pub fn new(words: &[String]) -> Self {
        Self {
            pattern: Pattern::new(
                &words.join(" "),
                CaseMatching::Ignore,
                Normalization::Smart,
                AtomKind::Substring,
            ),
            matcher: Matcher::new(Config::DEFAULT),
            buffer: Vec::new(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pattern.atoms.is_empty()
    }

    /// The character indices of this cell that the query matched, sorted and
    /// without repeats.
    pub fn indices(&mut self, haystack: &str) -> Vec<u32> {
        if self.is_empty() || haystack.is_empty() {
            return Vec::new();
        }
        let mut indices = Vec::new();
        let haystack = Utf32Str::new(haystack, &mut self.buffer);
        for atom in &self.pattern.atoms {
            let _ = atom.indices(haystack, &mut self.matcher, &mut indices);
        }
        indices.sort_unstable();
        indices.dedup();
        indices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(raw: &str) -> Vec<String> {
        raw.split_whitespace().map(str::to_owned).collect()
    }

    fn matches(query: &str, haystack: &str) -> bool {
        Query::new(&words(query)).matches(haystack)
    }

    #[test]
    fn matching_is_literal_case_insensitive_and_needs_every_word() {
        assert!(matches("db", "kv-prod db-password"));
        assert!(matches("DB-PASS", "kv-prod db-password"));
        assert!(matches("prod db", "kv-prod db-password"), "in any order");
        assert!(!matches("db qa", "kv-prod db-password"), "every word");
        assert!(
            !matches("dbpwd", "kv-prod db-password"),
            "scattered letters are not a match"
        );
        assert!(matches("", "anything"), "an empty query keeps every row");
        assert!(matches("   ", "anything"));
    }

    #[test]
    fn a_query_is_compiled_once_and_asked_about_many_rows() {
        let mut query = Query::new(&words("api"));
        assert!(!query.is_empty());
        assert!(query.matches("payments-api"));
        assert!(!query.matches("web"));
        assert!(query.matches("api-key"));
    }

    #[test]
    fn the_highlighter_marks_the_literal_run_and_nothing_else() {
        let mut highlighter = Highlighter::new(&words("pass"));
        let text = "db-password";
        let lit: String = highlighter
            .indices(text)
            .into_iter()
            .map(|index| text.chars().nth(index as usize).unwrap())
            .collect();
        assert_eq!(lit, "pass");

        let mut highlighter = Highlighter::new(&words("qa"));
        assert!(highlighter.indices("db-password").is_empty());

        let mut highlighter = Highlighter::new(&[]);
        assert!(highlighter.is_empty());
        assert!(highlighter.indices("db-password").is_empty());
    }

    #[test]
    fn both_words_of_a_query_are_lit_in_the_cell_that_holds_them() {
        let mut highlighter = Highlighter::new(&words("db pass"));
        let indices = highlighter.indices("db-password");
        assert!(indices.contains(&0) && indices.contains(&1), "db");
        assert!(indices.contains(&3), "the start of pass");
    }
}
