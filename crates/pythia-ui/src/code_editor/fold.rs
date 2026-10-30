//! Code folding: foldable regions from the PL/SQL token stream, and the
//! mapping between document lines and display rows when regions are folded.
//!
//! A folded region keeps its first line visible and hides the rest (up to
//! and including the line with the closing `END`); the view shows a "⋯" pill
//! at the end of the first line.

use std::collections::BTreeMap;

use pythia_plsql::{StatementKind, Token, TokenKind, split_statements, tokenize};

/// What a region is, for "fold all" decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionKind {
    /// Package, procedure or function body (`IS`/`AS` … `END name`), or an
    /// anonymous `DECLARE` block.
    Unit,
    /// `BEGIN … END`, `IF … END IF`, `LOOP … END LOOP`, `CASE … END CASE`.
    Block,
    /// A parenthesized list spanning lines.
    Paren,
    /// A multi-line block comment or ≥ 3 consecutive line comments.
    Comment,
    /// A SQL statement spanning several lines in a script.
    Statement,
}

/// A foldable range of 0-based lines; `start` stays visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub start: usize,
    pub end: usize,
    pub kind: RegionKind,
    /// Nesting depth of code regions (units and blocks), 0 = outermost.
    pub depth: usize,
}

/// All foldable regions of `src`, at most one per start line (the largest),
/// sorted by start line.
pub fn regions(src: &str) -> Vec<Region> {
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(src.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let line_of = |byte: usize| line_starts.partition_point(|&s| s <= byte) - 1;

    let tokens = tokenize(src);
    let mut out: Vec<Region> = Vec::new();
    let mut push = |start: usize, end: usize, kind: RegionKind, depth: usize| {
        if end > start {
            out.push(Region {
                start,
                end,
                kind,
                depth,
            });
        }
    };

    // Comments.
    let mut comment_run: Option<(usize, usize)> = None;
    let mut code_lines = std::collections::HashSet::new();
    for t in &tokens {
        match t.kind {
            TokenKind::BlockComment | TokenKind::Hint => {
                let (a, b) = (
                    line_of(t.span.start),
                    line_of(t.span.end.saturating_sub(1).max(t.span.start)),
                );
                push(a, b, RegionKind::Comment, 0);
            }
            TokenKind::Whitespace | TokenKind::Newline | TokenKind::LineComment => {}
            _ => {
                code_lines.insert(line_of(t.span.start));
            }
        }
    }
    for t in tokens.iter().filter(|t| t.kind == TokenKind::LineComment) {
        let line = line_of(t.span.start);
        if code_lines.contains(&line) {
            // Trailing comment after code.
            continue;
        }
        comment_run = match comment_run {
            Some((a, b)) if b + 1 == line => Some((a, line)),
            Some((a, b)) => {
                if b >= a + 2 {
                    push(a, b, RegionKind::Comment, 0);
                }
                Some((line, line))
            }
            None => Some((line, line)),
        };
    }
    if let Some((a, b)) = comment_run
        && b >= a + 2
    {
        push(a, b, RegionKind::Comment, 0);
    }

    // Code structure.
    structure(src, &tokens, &line_of, &mut push);

    // Multi-line SQL statements of a script (PL/SQL units fold by structure).
    let statements = split_statements(src);
    if statements.len() > 1 {
        for s in statements.iter().filter(|s| s.kind == StatementKind::Sql) {
            let (a, b) = (
                line_of(s.range.start),
                line_of(s.range.end.saturating_sub(1).max(s.range.start)),
            );
            push(a, b, RegionKind::Statement, 0);
        }
    }

    // One region per start line: keep the largest.
    let mut by_start: BTreeMap<usize, Region> = BTreeMap::new();
    for r in out {
        by_start
            .entry(r.start)
            .and_modify(|e| {
                if r.end > e.end {
                    *e = r;
                }
            })
            .or_insert(r);
    }
    by_start.into_values().collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Open {
    /// A subprogram/package header; `begun` once its BEGIN was seen.
    Unit {
        begun: bool,
    },
    Begin,
    If,
    Loop,
    Case,
    Paren,
}

fn structure(
    src: &str,
    tokens: &[Token],
    line_of: &dyn Fn(usize) -> usize,
    push: &mut dyn FnMut(usize, usize, RegionKind, usize),
) {
    let sig: Vec<&Token> = tokens.iter().filter(|t| !t.kind.is_trivia()).collect();
    let word = |i: usize| -> Option<String> {
        sig.get(i)
            .filter(|t| t.kind.is_word())
            .map(|t| src[t.span.clone()].to_ascii_uppercase())
    };
    // (kind, start line)
    let mut stack: Vec<(Open, usize)> = Vec::new();
    // A subprogram header waiting for its IS/AS: (line, paren depth).
    let mut header: Option<(usize, usize)> = None;
    // A FOR/WHILE whose LOOP may come lines later: (line, stack depth).
    let mut loop_header: Option<(usize, usize)> = None;
    let depth = |stack: &[(Open, usize)]| {
        stack
            .iter()
            .filter(|(o, _)| !matches!(o, Open::Paren))
            .count()
    };
    let mut i = 0;
    while i < sig.len() {
        let t = sig[i];
        let line = line_of(t.span.start);
        match t.kind {
            TokenKind::LParen => stack.push((Open::Paren, line)),
            TokenKind::RParen => {
                if let Some(pos) = stack.iter().rposition(|(o, _)| *o == Open::Paren) {
                    let (_, start) = stack[pos];
                    stack.truncate(pos);
                    push(start, line, RegionKind::Paren, depth(&stack));
                }
            }
            TokenKind::Semicolon => {
                // A forward declaration (`procedure p(x number);`) has no body.
                header = None;
                loop_header = None;
            }
            _ => {}
        }
        let w = word(i);
        let prev_end = i > 0 && word(i - 1).as_deref() == Some("END");
        match w.as_deref() {
            Some("PROCEDURE" | "FUNCTION") if !prev_end => {
                header = Some((line, stack.len()));
            }
            Some("PACKAGE") if !prev_end => header = Some((line, stack.len())),
            Some("TYPE") if word(i + 1).as_deref() == Some("BODY") => {
                header = Some((line, stack.len()))
            }
            Some("IS" | "AS") if header.is_some_and(|(_, d)| d == stack.len()) => {
                let (start, _) = header.take().expect("checked");
                stack.push((Open::Unit { begun: false }, start));
            }
            Some("DECLARE") => stack.push((Open::Unit { begun: false }, line)),
            Some("BEGIN") => {
                if let Some((Open::Unit { begun }, _)) = stack.last_mut()
                    && !*begun
                {
                    *begun = true;
                } else {
                    stack.push((Open::Begin, line));
                }
            }
            Some("IF") if !prev_end => stack.push((Open::If, line)),
            Some("FOR" | "WHILE") if loop_header.is_none_or(|(_, d)| stack.len() < d) => {
                loop_header = Some((line, stack.len()));
            }
            Some("LOOP") if !prev_end => {
                let start = match loop_header.take() {
                    Some((l, d)) if d == stack.len() => l,
                    _ => line,
                };
                stack.push((Open::Loop, start));
            }
            Some("CASE") if !prev_end => stack.push((Open::Case, line)),
            Some("END") => {
                let target = match word(i + 1).as_deref() {
                    Some("IF") => Some(Open::If),
                    Some("LOOP") => Some(Open::Loop),
                    Some("CASE") => Some(Open::Case),
                    _ => None,
                };
                let found = match target {
                    Some(target) => stack.iter().rposition(|(o, _)| *o == target),
                    None => stack.iter().rposition(|(o, _)| {
                        matches!(o, Open::Unit { .. } | Open::Begin | Open::Case)
                    }),
                };
                if let Some(pos) = found {
                    let (open, start) = stack[pos];
                    stack.truncate(pos);
                    let kind = if matches!(open, Open::Unit { .. }) {
                        RegionKind::Unit
                    } else {
                        RegionKind::Block
                    };
                    push(start, line, kind, depth(&stack));
                }
                if target.is_some() {
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
}

/// Maps document lines to display rows given the hidden line ranges.
#[derive(Debug, Clone, Default)]
pub struct DisplayMap {
    /// Sorted, disjoint, inclusive ranges of hidden lines.
    hidden: Vec<(usize, usize)>,
    /// Hidden lines before range `i`.
    before: Vec<usize>,
    lines: usize,
}

impl DisplayMap {
    /// `folds` are (start, end) line pairs; lines `start+1..=end` are hidden.
    pub fn new(lines: usize, folds: impl IntoIterator<Item = (usize, usize)>) -> Self {
        let mut ranges: Vec<(usize, usize)> = folds
            .into_iter()
            .filter(|(s, e)| e > s && *s < lines)
            .map(|(s, e)| (s + 1, e.min(lines.saturating_sub(1))))
            .filter(|(a, b)| a <= b)
            .collect();
        ranges.sort_unstable();
        let mut hidden: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
        for (a, b) in ranges {
            match hidden.last_mut() {
                Some(last) if a <= last.1 + 1 => last.1 = last.1.max(b),
                _ => hidden.push((a, b)),
            }
        }
        let mut before = Vec::with_capacity(hidden.len());
        let mut total = 0;
        for (a, b) in &hidden {
            before.push(total);
            total += b - a + 1;
        }
        Self {
            hidden,
            before,
            lines,
        }
    }

    fn hidden_total(&self) -> usize {
        self.hidden.last().map_or(0, |(a, b)| {
            self.before.last().copied().unwrap_or(0) + b - a + 1
        })
    }

    /// Number of display rows.
    pub fn rows(&self) -> usize {
        self.lines - self.hidden_total()
    }

    pub fn is_hidden(&self, line: usize) -> bool {
        let i = self.hidden.partition_point(|(a, _)| *a <= line);
        i > 0 && line <= self.hidden[i - 1].1
    }

    /// Display row of a line; hidden lines map to the row of their fold start.
    pub fn line_to_row(&self, line: usize) -> usize {
        let i = self.hidden.partition_point(|(a, _)| *a <= line);
        if i == 0 {
            return line;
        }
        let (a, b) = self.hidden[i - 1];
        if line <= b {
            // Inside: the visible line just before the range.
            return a - 1 - self.before[i - 1];
        }
        line - (self.before[i - 1] + b - a + 1)
    }

    /// Document line shown on a display row.
    pub fn row_to_line(&self, row: usize) -> usize {
        // Find the first range whose start row is beyond `row`.
        let mut line = row;
        for (i, (a, b)) in self.hidden.iter().enumerate() {
            let start_row = a - self.before[i];
            if row < start_row {
                break;
            }
            line = row + self.before[i] + (b - a + 1);
        }
        line.min(self.lines.saturating_sub(1))
    }

    /// The line `delta` display rows away from `line` (clamped).
    pub fn offset_line(&self, line: usize, delta: isize) -> isize {
        let row = self.line_to_row(line) as isize + delta;
        if row < 0 {
            return -1;
        }
        if row as usize >= self.rows() {
            return self.lines as isize;
        }
        self.row_to_line(row as usize) as isize
    }
}

/// Shifts a tracked 0-based line through an edit at `line` that removed
/// `removed` and inserted `inserted` line breaks. Lines merged away by the
/// edit collapse onto `line` (returned as `Err(line)`). An edit at the very
/// start of `line` moves that line along with the ones below.
pub fn shift_line(
    x: usize,
    line: usize,
    removed: usize,
    inserted: usize,
    at_line_start: bool,
) -> Result<usize, usize> {
    if at_line_start {
        return if x < line {
            Ok(x)
        } else if x >= line + removed {
            Ok(x + inserted - removed)
        } else {
            Err(line)
        };
    }
    if x <= line {
        Ok(x)
    } else if x > line + removed {
        Ok(x + inserted - removed)
    } else {
        Err(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(src: &str) -> Vec<(usize, usize, RegionKind)> {
        regions(src)
            .into_iter()
            .map(|r| (r.start, r.end, r.kind))
            .collect()
    }

    #[test]
    fn plsql_structure() {
        let src = "create or replace package body p as\n\
                   \x20 procedure a(x number,\n\
                   \x20              y number) is\n\
                   \x20   v number;\n\
                   \x20 begin\n\
                   \x20   if x > 0 then\n\
                   \x20     null;\n\
                   \x20   end if;\n\
                   \x20   for r in (select 1\n\
                   \x20             from dual) loop\n\
                   \x20     null;\n\
                   \x20   end loop;\n\
                   \x20 end a;\n\
                   \x20 procedure fwd;\n\
                   end p;\n/\n";
        assert_eq!(
            spans(src),
            vec![
                (0, 14, RegionKind::Unit),
                (1, 12, RegionKind::Unit),
                (5, 7, RegionKind::Block),
                (8, 11, RegionKind::Block),
            ]
        );
        let depths: Vec<usize> = regions(src).iter().map(|r| r.depth).collect();
        assert_eq!(depths, [0, 1, 2, 2]);
    }

    #[test]
    fn comments_statements_and_case() {
        let src = "/* a\n   b */\n-- 1\n-- 2\n-- 3\nselect a,\n  b\nfrom t;\nselect 1 from dual;\nbegin\n  x := case\n    when 1 then 2\n  end;\nend;\n";
        assert_eq!(
            spans(src),
            vec![
                (0, 1, RegionKind::Comment),
                (2, 4, RegionKind::Comment),
                (5, 7, RegionKind::Statement),
                (9, 13, RegionKind::Block),
                (10, 12, RegionKind::Block),
            ]
        );
    }

    #[test]
    fn conditional_compilation() {
        // `$END` and the next line's IF closed the IF above as an END IF
        // (#171); directives are no words.
        let src = "begin\n  $IF x $THEN\n  if a then\n    null;\n  end if;\n  $END\n  if b then\n    null;\n  end if;\nend;\n";
        assert_eq!(
            spans(src),
            vec![
                (0, 9, RegionKind::Block),
                (2, 4, RegionKind::Block),
                (6, 8, RegionKind::Block),
            ]
        );
    }

    #[test]
    fn display_map() {
        // 10 lines; fold 2..=4 (hides 3,4) and 6..=8 (hides 7,8).
        let m = DisplayMap::new(10, [(2, 4), (6, 8)]);
        assert_eq!(m.rows(), 6);
        let rows: Vec<usize> = (0..m.rows()).map(|r| m.row_to_line(r)).collect();
        assert_eq!(rows, [0, 1, 2, 5, 6, 9]);
        assert!(m.is_hidden(3) && m.is_hidden(8) && !m.is_hidden(2) && !m.is_hidden(9));
        assert_eq!(m.line_to_row(9), 5);
        assert_eq!(m.line_to_row(4), 2);
        assert_eq!(m.offset_line(2, 1), 5);
        assert_eq!(m.offset_line(5, -1), 2);
        assert_eq!(m.offset_line(9, 1), 10);
        // Nested folds merge.
        let m = DisplayMap::new(10, [(1, 8), (3, 5)]);
        assert_eq!(m.rows(), 3);
        assert_eq!(DisplayMap::new(4, []).row_to_line(3), 3);
    }

    #[test]
    fn line_shifts() {
        assert_eq!(shift_line(3, 5, 0, 2, false), Ok(3));
        assert_eq!(shift_line(5, 5, 0, 2, false), Ok(5));
        assert_eq!(shift_line(5, 5, 0, 2, true), Ok(7));
        assert_eq!(shift_line(9, 5, 0, 2, false), Ok(11));
        assert_eq!(shift_line(9, 5, 2, 0, false), Ok(7));
        assert_eq!(shift_line(6, 5, 2, 0, false), Err(5));
        assert_eq!(shift_line(6, 5, 1, 0, true), Ok(5));
    }
}
