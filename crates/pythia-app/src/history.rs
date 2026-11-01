//! Statement history, persisted per connection.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const MAX_ENTRIES: usize = 2000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub connection: String,
    pub sql: String,
    /// Unix seconds of the last execution.
    pub at: u64,
    pub count: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct History {
    #[serde(rename = "entry")]
    pub entries: Vec<Entry>,
    #[serde(skip)]
    pub dirty: bool,
    /// Entries added at the end since the history was last tidied: a
    /// statement may be in it twice, and there may be too many.
    #[serde(skip)]
    added: usize,
}

impl History {
    /// Records an execution; re-running a statement moves it to the top
    /// once the history is [tidied](Self::tidy).
    pub fn add(&mut self, connection: &str, sql: &str) {
        let sql = sql.trim();
        if sql.is_empty() {
            return;
        }
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        self.entries.push(Entry {
            connection: connection.to_owned(),
            sql: sql.to_owned(),
            at,
            count: 1,
        });
        self.added += 1;
        self.dirty = true;
        if self.added >= MAX_ENTRIES {
            self.tidy();
        }
    }

    /// Puts the statements added since the last time in their place: each
    /// statement of a connection once, where it ran last, with the count of
    /// all its runs, and the oldest beyond [`MAX_ENTRIES`] dropped. Done once
    /// for all statements of a frame rather than for each (#318): a script
    /// hands over up to 1,000 at once (#291), and looking through the
    /// history for each took most of the frame.
    pub fn tidy(&mut self) {
        if self.added == 0 {
            return;
        }
        self.added = 0;
        let last: HashMap<(&str, &str), usize> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| ((e.connection.as_str(), e.sql.as_str()), i))
            .collect();
        let kept: Vec<usize> = self
            .entries
            .iter()
            .map(|e| last[&(e.connection.as_str(), e.sql.as_str())])
            .collect();
        let mut counts = vec![0; kept.len()];
        for (e, &k) in self.entries.iter().zip(&kept) {
            counts[k] += e.count;
        }
        let mut i = 0;
        self.entries.retain_mut(|e| {
            let keep = kept[i] == i;
            e.count = counts[i];
            i += 1;
            keep
        });
        if self.entries.len() > MAX_ENTRIES {
            let excess = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..excess);
        }
    }

    /// Takes in the statements of `saved`, the file another Pythia may have
    /// written since this one read it (#196): one entry per connection and
    /// statement, at its last execution, in the order they ran.
    pub fn merge(&mut self, saved: History) {
        self.tidy();
        let key = |e: &Entry| (e.connection.clone(), e.sql.clone());
        let mut at: std::collections::HashMap<(String, String), usize> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (key(e), i))
            .collect();
        for entry in saved.entries {
            match at.get(&key(&entry)) {
                Some(&i) => {
                    let mine = &mut self.entries[i];
                    mine.at = mine.at.max(entry.at);
                    mine.count = mine.count.max(entry.count);
                }
                None => {
                    at.insert(key(&entry), self.entries.len());
                    self.entries.push(entry);
                }
            }
        }
        // Stable: entries of the same second keep their order.
        self.entries.sort_by_key(|e| e.at);
        if self.entries.len() > MAX_ENTRIES {
            let excess = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..excess);
        }
    }

    /// Newest first, optionally limited to one connection and a filter text;
    /// statements added since the history was last [tidied](Self::tidy)
    /// may be there twice.
    pub fn search<'a>(
        &'a self,
        connection: Option<&'a str>,
        filter: &'a str,
    ) -> impl Iterator<Item = &'a Entry> {
        let filter = filter.to_lowercase();
        self.entries.iter().rev().filter(move |e| {
            connection.is_none_or(|c| e.connection == c)
                && (filter.is_empty() || e.sql.to_lowercase().contains(&filter))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupes_and_orders() {
        let mut h = History::default();
        h.add("a", "select 1 from dual");
        h.add("a", "select 2 from dual");
        h.add("a", " select 1 from dual ");
        h.add("b", "select 3 from dual");
        h.tidy();
        let a: Vec<_> = h
            .search(Some("a"), "")
            .map(|e| (e.sql.as_str(), e.count))
            .collect();
        assert_eq!(a, [("select 1 from dual", 2), ("select 2 from dual", 1)]);
        assert_eq!(h.search(None, "3").count(), 1);
    }

    /// A frame hands over many statements at once (#291): they go into
    /// the history in one pass, not each through all of it (#318). 20,000
    /// added to a full history in frames of 1,000 took 560 ms in a debug
    /// build, now 40 ms; the order, the counts of statements run again and
    /// the size stay as for each added alone.
    #[test]
    fn statements_go_in_at_once() {
        let mut h = History::default();
        for i in 0..MAX_ENTRIES {
            h.add("a", &format!("select {i} from dual"));
        }
        h.tidy();
        let started = std::time::Instant::now();
        for i in 0..20_000 {
            h.add("a", &format!("insert into r51_t values ({i})"));
            if i % 1000 == 999 {
                h.tidy();
            }
        }
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_millis(200), "{took:?}");
        assert_eq!(h.entries.len(), MAX_ENTRIES);
        assert_eq!(h.entries[0].sql, "insert into r51_t values (18000)");

        // Run again, in one frame and in the next.
        for sql in [
            "insert into r51_t values (18000)",
            "select 1 from dual",
            "x",
        ] {
            h.add("a", sql);
        }
        h.add("a", "insert into r51_t values (18000)");
        h.add("b", "x");
        h.tidy();
        h.add("a", "x");
        h.tidy();
        let newest: Vec<_> = h
            .search(None, "")
            .take(5)
            .map(|e| (e.connection.as_str(), e.sql.as_str(), e.count))
            .collect();
        assert_eq!(
            newest,
            [
                ("a", "x", 2),
                ("b", "x", 1),
                ("a", "insert into r51_t values (18000)", 3),
                ("a", "select 1 from dual", 1),
                ("a", "insert into r51_t values (19999)", 1),
            ]
        );
        assert_eq!(h.entries.len(), MAX_ENTRIES);
    }

    /// Saving takes in the statements another Pythia saved (#196).
    #[test]
    fn merges_another_pythias() {
        let entry = |sql: &str, at, count| Entry {
            connection: "a".into(),
            sql: sql.into(),
            at,
            count,
        };
        let mut mine = History {
            entries: vec![entry("one", 10, 1), entry("both", 20, 2)],
            dirty: true,
            added: 0,
        };
        let saved = History {
            entries: vec![entry("both", 30, 5), entry("theirs", 15, 1)],
            dirty: false,
            added: 0,
        };
        mine.merge(saved);
        let entries: Vec<_> = mine
            .entries
            .iter()
            .map(|e| (e.sql.as_str(), e.at, e.count))
            .collect();
        assert_eq!(
            entries,
            [("one", 10, 1), ("theirs", 15, 1), ("both", 30, 5)]
        );
    }
}
