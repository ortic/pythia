//! Find / replace.

use std::ops::Range;

use regex::{Regex, RegexBuilder};

use super::text::is_word_char;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchQuery {
    pub text: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

/// A compiled query.
pub struct Matcher {
    regex: Regex,
    whole_word: bool,
    is_regex: bool,
}

impl Matcher {
    /// Returns `None` for an empty query, `Err` for an invalid regex.
    pub fn new(query: &SearchQuery) -> Option<Result<Self, regex::Error>> {
        if query.text.is_empty() {
            return None;
        }
        let pattern = if query.regex {
            query.text.clone()
        } else {
            regex::escape(&query.text)
        };
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(!query.case_sensitive)
            .multi_line(true)
            .build();
        Some(regex.map(|regex| Self {
            regex,
            whole_word: query.whole_word,
            is_regex: query.regex,
        }))
    }

    /// Byte ranges of all non-empty matches within `scope` of `haystack`.
    /// The whole text is searched from the scope's start, so that `^`, `$`
    /// and `\b` read the text around it as [`Self::replacement`] does: in a
    /// slice, a selection's edges were a line's and a word's, and `$1` then
    /// had no captures to expand (#418). A match must end in the scope. The
    /// scope is clamped to the text and its characters.
    pub fn find_all(&self, haystack: &str, scope: Option<Range<usize>>) -> Vec<Range<usize>> {
        let boundary = |mut i: usize| {
            i = i.min(haystack.len());
            while !haystack.is_char_boundary(i) {
                i -= 1;
            }
            i
        };
        let scope = scope.map_or(0..haystack.len(), |s| boundary(s.start)..boundary(s.end));
        let mut out = Vec::new();
        let mut at = scope.start;
        while at <= scope.end {
            let Some(m) = self.regex.find_at(haystack, at) else {
                break;
            };
            if m.end() > scope.end {
                break;
            }
            let r = m.start()..m.end();
            if !r.is_empty() && (!self.whole_word || is_whole_word(haystack, &r)) {
                out.push(r.clone());
            }
            // Past an empty match by one character.
            at = if r.is_empty() {
                r.end + haystack[r.end..].chars().next().map_or(1, char::len_utf8)
            } else {
                r.end
            };
        }
        out
    }

    /// The replacement text for the match at `range` (expands `$1` in regex mode).
    pub fn replacement(&self, haystack: &str, range: &Range<usize>, replace: &str) -> String {
        if !self.is_regex {
            return replace.to_owned();
        }
        // Re-run the regex anchored at the match to get its captures. Use the
        // whole haystack so look-around-like anchors (`^`, `$`) stay right.
        match self.regex.captures_at(haystack, range.start) {
            Some(caps) if caps.get(0).is_some_and(|m| m.start() == range.start) => {
                let mut out = String::new();
                caps.expand(replace, &mut out);
                out
            }
            _ => replace.to_owned(),
        }
    }
}

fn is_whole_word(haystack: &str, r: &Range<usize>) -> bool {
    let before = haystack[..r.start].chars().next_back();
    let after = haystack[r.end..].chars().next();
    !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
}

/// Index of the first match at or after `pos` (wrapping), or the last match
/// before `pos` when searching backwards.
pub fn next_match(matches: &[Range<usize>], pos: usize, forward: bool) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    if forward {
        Some(matches.iter().position(|m| m.start >= pos).unwrap_or(0))
    } else {
        Some(
            matches
                .iter()
                .rposition(|m| m.end <= pos)
                .unwrap_or(matches.len() - 1),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str) -> SearchQuery {
        SearchQuery {
            text: text.into(),
            ..Default::default()
        }
    }

    fn rq(text: &str) -> SearchQuery {
        SearchQuery {
            regex: true,
            case_sensitive: true,
            ..q(text)
        }
    }

    /// One match, from `start` to `end`.
    fn one(start: usize, end: usize) -> Vec<Range<usize>> {
        vec![Range { start, end }]
    }

    /// In a selection, the text around it still counts for `^`, `$` and
    /// word boundaries, and a match must lie in it: `$1` always expands.
    #[test]
    fn matches_in_a_scope() {
        let m = Matcher::new(&rq(r"(\w+)")).unwrap().unwrap();
        // The selection ends inside `name`: only `a`.
        let hay = "a name x";
        let found = m.find_all(hay, Some(0..4));
        assert_eq!(found, one(0, 1));
        assert_eq!(m.replacement(hay, &found[0], "\"$1\""), "\"a\"");
        // Starting mid-line, `^` is no line start there.
        let m = Matcher::new(&rq(r"^(\s+)(\S)")).unwrap().unwrap();
        let hay = "  x\n  y";
        let found = m.find_all(hay, Some(2..7));
        assert_eq!(found, one(4, 7));
        assert_eq!(m.replacement(hay, &found[0], "$2"), "y");
        // Ending mid-line, `$` is no line end there.
        let m = Matcher::new(&rq(r"(\w)$")).unwrap().unwrap();
        assert!(m.find_all("ab cd", Some(0..2)).is_empty());
        // A scope past the text or inside a character is clamped.
        let m = Matcher::new(&q("a")).unwrap().unwrap();
        assert_eq!(m.find_all("t.\nü", Some(0..4)), Vec::<Range<usize>>::new());
        assert_eq!(m.find_all("aü", Some(0..99)), one(0, 1));
        // Empty matches are stepped over by a character.
        let m = Matcher::new(&rq("x*")).unwrap().unwrap();
        assert_eq!(m.find_all("üxü", None), one(2, 3));
    }

    #[test]
    fn literal_case_and_word() {
        let hay = "Emp emp employees EMP";
        let m = Matcher::new(&q("emp")).unwrap().unwrap();
        assert_eq!(m.find_all(hay, None).len(), 4);
        let m = Matcher::new(&SearchQuery {
            case_sensitive: true,
            ..q("emp")
        })
        .unwrap()
        .unwrap();
        assert_eq!(m.find_all(hay, None), vec![4..7, 8..11]);
        let m = Matcher::new(&SearchQuery {
            whole_word: true,
            ..q("emp")
        })
        .unwrap()
        .unwrap();
        assert_eq!(m.find_all(hay, None), vec![0..3, 4..7, 18..21]);
        assert_eq!(m.find_all(hay, Some(2..12)), vec![4..7]);
        let m = Matcher::new(&q("a.b")).unwrap().unwrap();
        assert!(m.find_all("axb", None).is_empty());
    }

    #[test]
    fn regex_and_replacement() {
        let hay = "x := 1;\ny := 22;";
        let query = SearchQuery {
            regex: true,
            ..q(r"(\w) := (\d+)")
        };
        let m = Matcher::new(&query).unwrap().unwrap();
        let found = m.find_all(hay, None);
        assert_eq!(found, vec![0..6, 8..15]);
        assert_eq!(m.replacement(hay, &found[1], "$2 => $1"), "22 => y");
        assert!(
            Matcher::new(&SearchQuery {
                regex: true,
                ..q("(")
            })
            .unwrap()
            .is_err()
        );
        assert!(Matcher::new(&q("")).is_none());
        let line_start = Matcher::new(&SearchQuery {
            regex: true,
            ..q("^y")
        })
        .unwrap()
        .unwrap();
        assert_eq!(line_start.find_all(hay, None), vec![8..9]);
    }

    #[test]
    fn navigation() {
        let m = vec![2..4, 10..12, 20..22];
        assert_eq!(next_match(&m, 5, true), Some(1));
        assert_eq!(next_match(&m, 25, true), Some(0));
        assert_eq!(next_match(&m, 10, false), Some(0));
        assert_eq!(next_match(&m, 1, false), Some(2));
    }
}
