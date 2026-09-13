//! Literal, in-memory matching over whatever is on screen.
//!
//! Every whitespace-separated word in the query must appear in the row as a
//! substring, ignoring case. Scattered letters match nothing: `tks` does not
//! find `ticket search`, which is what makes typing a secret's name a
//! reliable way to find it rather than a guess.
//!
//! There is no relevance score and no ordering of its own — rows keep the
//! table's sort — and no worker thread, because ten thousand rows re-filter
//! in well under a millisecond on the main thread.
//!
//! Done by hand rather than with a matcher crate: `nucleo-matcher` 0.3
//! cannot find a substring after a non-ASCII character in the haystack, so
//! an event message with an ellipsis in it was unfindable.
//!
// ponytail: matching on the main thread on every keystroke. Past roughly
// 50,000 rows this wants the worker and a generation counter, which is what
// ticket-tui's search.rs does; below that a debounce would only add latency.

/// One query, compiled once and asked about many rows: whether a row
/// matches, and which characters of a cell it lit up.
pub struct Query {
    /// Each word, lowered one character for one.
    words: Vec<Vec<char>>,
}

impl Query {
    #[must_use]
    pub fn new(words: &[String]) -> Self {
        Self {
            words: words
                .iter()
                .flat_map(|word| word.split_whitespace())
                .map(lowered)
                .filter(|word| !word.is_empty())
                .collect(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Whether every word of the query is somewhere in this row's text.
    #[must_use]
    pub fn matches(&self, haystack: &str) -> bool {
        if self.is_empty() {
            return true;
        }
        let haystack = lowered(haystack);
        self.words
            .iter()
            .all(|word| find(&haystack, word).is_some())
    }

    /// The character indices of this cell that the query matched, sorted and
    /// without repeats, so the table can paint them in `search_match`. Each
    /// word lights where it first appears.
    #[must_use]
    pub fn indices(&self, haystack: &str) -> Vec<u32> {
        if self.is_empty() || haystack.is_empty() {
            return Vec::new();
        }
        let haystack = lowered(haystack);
        let mut indices: Vec<u32> = self
            .words
            .iter()
            .filter_map(|word| find(&haystack, word).map(|at| (at, word.len())))
            .flat_map(|(at, len)| {
                (at..at + len).map(|index| u32::try_from(index).unwrap_or(u32::MAX))
            })
            .collect();
        indices.sort_unstable();
        indices.dedup();
        indices
    }
}

/// The text as characters, each lowered to one character, so an index into
/// it is an index into the text's characters.
fn lowered(text: &str) -> Vec<char> {
    text.chars()
        .map(|character| character.to_lowercase().next().unwrap_or(character))
        .collect()
}

/// Where `word` first appears in `haystack`, as a character index.
fn find(haystack: &[char], word: &[char]) -> Option<usize> {
    haystack
        .windows(word.len())
        .position(|window| window == word)
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
        let query = Query::new(&words("api"));
        assert!(!query.is_empty());
        assert!(query.matches("payments-api"));
        assert!(!query.matches("web"));
        assert!(query.matches("api-key"));
    }

    #[test]
    fn the_indices_mark_the_literal_run_and_nothing_else() {
        let query = Query::new(&words("pass"));
        let text = "db-password";
        let lit: String = query
            .indices(text)
            .into_iter()
            .map(|index| text.chars().nth(index as usize).unwrap())
            .collect();
        assert_eq!(lit, "pass");

        let query = Query::new(&words("qa"));
        assert!(query.indices("db-password").is_empty());

        let query = Query::new(&[]);
        assert!(query.is_empty());
        assert!(query.indices("db-password").is_empty());
    }

    #[test]
    fn both_words_of_a_query_are_lit_in_the_cell_that_holds_them() {
        let query = Query::new(&words("db pass"));
        let indices = query.indices("db-password");
        assert!(indices.contains(&0) && indices.contains(&1), "db");
        assert!(indices.contains(&3), "the start of pass");
    }

    #[test]
    fn a_word_after_an_accent_or_an_ellipsis_is_found_and_lit() {
        let query = Query::new(&words("bad"));
        assert!(query.matches("caf\u{e9} bad"));
        assert_eq!(query.indices("caf\u{e9} bad"), [5, 6, 7]);
        assert!(query.matches("\u{2026} kubectl BAD"));
        assert!(
            Query::new(&words("caf\u{c9}")).matches("caf\u{e9}"),
            "case, not just ASCII case"
        );
    }
}
