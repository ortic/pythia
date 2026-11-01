//! Bracket and keyword-pair matching (`(`↔`)`, `BEGIN`↔`END`, `IF`↔`END IF`, ...).
//!
//! Works on the token stream of the whole document, so brackets inside
//! strings and comments are ignored. Positions are byte offsets.

use std::ops::Range;

use pythia_plsql::{Syntax, Token, TokenKind, tokenize_in};

/// A matched pair: the opening and closing token ranges (`END IF` spans both words).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pair {
    pub open: Range<usize>,
    pub close: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Begin,
    If,
    Loop,
    Case,
}

/// All bracket and keyword pairs of `src`.
#[cfg(test)]
pub fn find_pairs(src: &str) -> Vec<Pair> {
    find_pairs_in(src, Syntax::Oracle)
}

/// [`find_pairs`] of a script of a database with `syntax`.
pub fn find_pairs_in(src: &str, syntax: Syntax) -> Vec<Pair> {
    let tokens: Vec<Token> = tokenize_in(src, syntax)
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .collect();
    let word = |t: &Token| {
        t.kind
            .is_word()
            .then(|| src[t.span.clone()].to_ascii_uppercase())
    };

    let mut pairs = Vec::new();
    let mut parens: Vec<(char, Range<usize>)> = Vec::new();
    let mut blocks: Vec<(Block, Range<usize>)> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        let text = &src[t.span.clone()];
        match t.kind {
            TokenKind::LParen => parens.push(('(', t.span.clone())),
            TokenKind::RParen => {
                if let Some(pos) = parens.iter().rposition(|(c, _)| *c == '(') {
                    let (_, open) = parens.remove(pos);
                    parens.truncate(pos);
                    pairs.push(Pair {
                        open,
                        close: t.span.clone(),
                    });
                }
            }
            TokenKind::Operator | TokenKind::Error if text == "[" => {
                parens.push(('[', t.span.clone()))
            }
            TokenKind::Operator | TokenKind::Error if text == "]" => {
                if let Some(pos) = parens.iter().rposition(|(c, _)| *c == '[') {
                    let (_, open) = parens.remove(pos);
                    parens.truncate(pos);
                    pairs.push(Pair {
                        open,
                        close: t.span.clone(),
                    });
                }
            }
            _ => {}
        }
        match word(t).as_deref() {
            Some("BEGIN") => blocks.push((Block::Begin, t.span.clone())),
            Some("IF") => blocks.push((Block::If, t.span.clone())),
            Some("LOOP") => blocks.push((Block::Loop, t.span.clone())),
            Some("CASE") => blocks.push((Block::Case, t.span.clone())),
            Some("END") => {
                let next = tokens.get(i + 1);
                let target = match next.and_then(word).as_deref() {
                    Some("IF") => Some(Block::If),
                    Some("LOOP") => Some(Block::Loop),
                    Some("CASE") => Some(Block::Case),
                    _ => None,
                };
                let close = match target {
                    Some(_) => {
                        i += 1;
                        t.span.start..tokens[i].span.end
                    }
                    None => t.span.clone(),
                };
                // A plain END closes a BEGIN block or a CASE expression.
                let accepts = |b: &Block| match target {
                    Some(target) => *b == target,
                    None => matches!(b, Block::Begin | Block::Case),
                };
                if let Some(pos) = blocks.iter().rposition(|(b, _)| accepts(b)) {
                    let (_, open) = blocks.remove(pos);
                    blocks.truncate(pos);
                    pairs.push(Pair { open, close });
                }
            }
            _ => {}
        }
        i += 1;
    }
    pairs
}

/// The pair whose open or close token contains (or touches) byte `pos`.
pub fn pair_at(pairs: &[Pair], pos: usize) -> Option<&Pair> {
    let hit = |r: &Range<usize>| r.start <= pos && pos <= r.end;
    // Prefer the innermost (shortest) pair when several touch the cursor, e.g. `((`.
    pairs
        .iter()
        .filter(|p| hit(&p.open) || hit(&p.close))
        .min_by_key(|p| p.close.end - p.open.start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts<'a>(src: &'a str, p: &Pair) -> (&'a str, &'a str) {
        (&src[p.open.clone()], &src[p.close.clone()])
    }

    #[test]
    fn brackets_ignore_strings() {
        let src = "f(a, ')', (b))";
        let pairs = find_pairs(src);
        assert_eq!(pairs.len(), 2);
        assert_eq!(
            pairs[1],
            Pair {
                open: 1..2,
                close: 13..14
            }
        );
        assert_eq!(pair_at(&pairs, 10), Some(&pairs[0]));
    }

    #[test]
    fn keyword_pairs() {
        let src = "begin\n if x then\n  loop null; end loop;\n end if;\n y := case when a then 1 end;\nend;";
        let pairs = find_pairs(src);
        let words: Vec<_> = pairs
            .iter()
            .map(|p| texts(src, p))
            .filter(|(o, _)| *o != "(")
            .collect();
        assert_eq!(
            words,
            vec![
                ("loop", "end loop"),
                ("if", "end if"),
                ("case", "end"),
                ("begin", "end")
            ]
        );
        let begin = pair_at(&pairs, 2).unwrap();
        assert_eq!(texts(src, begin), ("begin", "end"));
        let end_if = src.find("end if").unwrap() + 5;
        assert_eq!(
            texts(src, pair_at(&pairs, end_if).unwrap()),
            ("if", "end if")
        );
    }

    #[test]
    fn unbalanced_does_not_panic() {
        assert!(find_pairs("end; ) ] end if; begin (").is_empty());
    }
}
