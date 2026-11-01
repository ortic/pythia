//! Statement history, persisted per connection.

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
}

impl History {
    /// Records an execution; re-running a statement moves it to the top.
    pub fn add(&mut self, connection: &str, sql: &str) {
        let sql = sql.trim();
        if sql.is_empty() {
            return;
        }
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let count = match self
            .entries
            .iter()
            .position(|e| e.connection == connection && e.sql == sql)
        {
            Some(i) => self.entries.remove(i).count + 1,
            None => 1,
        };
        self.entries.push(Entry {
            connection: connection.to_owned(),
            sql: sql.to_owned(),
            at,
            count,
        });
        if self.entries.len() > MAX_ENTRIES {
            let excess = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..excess);
        }
        self.dirty = true;
    }

    /// Takes in the statements of `saved`, the file another Pythia may have
    /// written since this one read it (#196): one entry per connection and
    /// statement, at its last execution, in the order they ran.
    pub fn merge(&mut self, saved: History) {
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

    /// Newest first, optionally limited to one connection and a filter text.
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
        let a: Vec<_> = h
            .search(Some("a"), "")
            .map(|e| (e.sql.as_str(), e.count))
            .collect();
        assert_eq!(a, [("select 1 from dual", 2), ("select 2 from dual", 1)]);
        assert_eq!(h.search(None, "3").count(), 1);
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
        };
        let saved = History {
            entries: vec![entry("both", 30, 5), entry("theirs", 15, 1)],
            dirty: false,
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
