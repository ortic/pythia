//! PL/SQL + SQL lexer.
//!
//! The lexer is lossless: every byte of the input belongs to exactly one token,
//! so it can drive syntax highlighting as well as statement splitting. Errors
//! (unterminated constructs, stray characters) never abort lexing.

use std::ops::Range;

use logos::Logos;

use crate::Syntax;
use crate::keywords::is_keyword;

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[logos(extras = Syntax)]
pub enum TokenKind {
    #[regex(r"[ \t\r\f]+")]
    Whitespace,
    #[token("\n")]
    Newline,
    #[regex(r"--[^\n]*", allow_greedy = true)]
    LineComment,
    #[token("/*", block_comment)]
    BlockComment,
    /// Optimizer hint `/*+ ... */`.
    #[token("/*+", block_comment)]
    Hint,
    /// `'...'` and `N'...'` string literals, and PostgreSQL's dollar-quoted
    /// `$$...$$` / `$tag$...$tag$` and `E'...'`.
    #[token("'", string)]
    #[regex("[nN]'", string)]
    #[token("$", dollar_string)]
    String,
    /// Alternative quoting `q'[...]'`, `nq'{...}'`.
    #[regex("[nN]?[qQ]'", q_string)]
    QString,
    #[regex(r"[0-9]+(\.[0-9]+)?([eE][+-]?[0-9]+)?[fFdD]?")]
    #[regex(r"\.[0-9]+([eE][+-]?[0-9]+)?[fFdD]?")]
    Number,
    /// Unquoted identifier. Keywords are reclassified by [`tokenize`].
    #[regex(r"[\p{L}][\p{L}\p{N}_$#]*")]
    Ident,
    /// `"Name"`, MySQL's `` `name` `` (a doubled backtick stands for one),
    /// and SQL Server's and SQLite's `[name]`.
    #[regex(r#""[^"\n]*"?"#)]
    #[regex(r"`([^`\n]|``)*`?")]
    #[token("[", bracket_ident)]
    QuotedIdent,
    #[regex(r":[\p{L}][\p{L}\p{N}_$#]*")]
    #[regex(r":[0-9]+")]
    BindVar,
    /// SQL*Plus substitution variable `&var` / `&&var`, also a numbered
    /// one, `&1`, as a script's arguments are (#435).
    #[regex(r"&&?[\p{L}\p{N}_][\p{L}\p{N}_$#]*\.?")]
    SubstVar,
    /// `::` is PostgreSQL's cast (`x::int` has no bind variable `:int`).
    #[regex(r":=|::|=>|\|\||<=|>=|<>|!=|\^=|~=|\.\.|\*\*|<<|>>|[+\-*/=<>@%|!^~?&:]")]
    Operator,
    #[token(";")]
    Semicolon,
    #[token(",")]
    Comma,
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token(".")]
    Dot,

    /// Reserved word (produced by [`tokenize`], never by the raw lexer).
    Keyword,
    /// Oracle's conditional compilation: the directives `$IF`, `$THEN`,
    /// `$ELSIF`, `$ELSE`, `$END` and `$ERROR`, and inquiry directives like
    /// `$$PLSQL_UNIT` (produced by [`tokenize`]).
    Directive,
    /// SQL*Plus command (`PROMPT`, `SET SERVEROUTPUT`, `@file`, `/`); only
    /// produced by the editor's line highlighter.
    SqlPlus,
    /// Input the lexer could not make sense of.
    Error,
}

impl TokenKind {
    /// Whitespace and comments: tokens without meaning for the parser.
    pub fn is_trivia(self) -> bool {
        matches!(
            self,
            TokenKind::Whitespace
                | TokenKind::Newline
                | TokenKind::LineComment
                | TokenKind::BlockComment
        )
    }

    pub fn is_word(self) -> bool {
        matches!(self, TokenKind::Ident | TokenKind::Keyword)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Range<usize>,
}

/// Lexes `src` into a lossless token stream.
pub fn tokenize(src: &str) -> Vec<Token> {
    tokenize_in(src, Syntax::Oracle)
}

/// [`tokenize`] for a script of a database with `syntax`. MySQL lexes four
/// things its own way: `\` escapes the next character in `'…'` and `"…"`
/// (#187), `#` starts a line comment (#188), `--` does only before
/// whitespace or a control character (#189), and `/*!…*/` is code (#216).
/// Its `"…"` is a string, and it has no dollar quotes. PostgreSQL's dollar-quoted body may start with a
/// word (#201), and `\` escapes the next character in `E'…'` (#202).
/// Neither has Oracle's directives.
pub fn tokenize_in(src: &str, syntax: Syntax) -> Vec<Token> {
    let mut lexer = TokenKind::lexer_with_extras(src, syntax);
    // Where `lexer` started: it starts again after a token MySQL or
    // PostgreSQL ends elsewhere.
    let mut base = 0;
    let mut tokens: Vec<Token> = Vec::new();
    // `[` open in PostgreSQL's syntax, where they're no names.
    let mut brackets = 0usize;
    while let Some(result) = lexer.next() {
        let mut span = base + lexer.span().start..base + lexer.span().end;
        let mut kind = match result {
            Ok(TokenKind::Ident) if is_keyword(&src[span.clone()]) => TokenKind::Keyword,
            Ok(kind) => kind,
            Err(()) => TokenKind::Error,
        };
        let own = match syntax {
            Syntax::MySql | Syntax::MySqlNoEscapes => {
                mysql_token(src, kind, span.clone(), syntax == Syntax::MySql)
            }
            Syntax::Postgres => postgres_token(src, kind, span.clone()),
            _ => None,
        };
        if let Some((own_kind, end)) = own {
            kind = own_kind;
            span.end = end;
            base = end;
            lexer = TokenKind::lexer_with_extras(&src[end..], syntax);
        }
        // Oracle's `$IF` and `$$name` (#171): the raw lexer knows `$` only
        // in dollar-quoted strings. PostgreSQL's `$$name` is one (#201).
        if !(syntax.is_mysql() || syntax == Syntax::Postgres)
            && matches!(kind, TokenKind::Ident | TokenKind::Keyword)
            && let Some(last) = tokens.last_mut().filter(|t: &&mut Token| {
                t.kind == TokenKind::Error
                    && t.span.end == span.start
                    && is_directive(&src[t.span.clone()], &src[span.clone()])
            })
        {
            last.kind = TokenKind::Directive;
            last.span.end = span.end;
            continue;
        }
        // T-SQL's temporary tables: `#name`, `##name` (#320).
        if syntax == Syntax::TSql
            && matches!(kind, TokenKind::Ident | TokenKind::Keyword)
            && let Some(last) = tokens.last_mut().filter(|t: &&mut Token| {
                t.kind == TokenKind::Error
                    && t.span.end == span.start
                    && is_temporary_prefix(&src[t.span.clone()])
            })
        {
            last.kind = TokenKind::Ident;
            last.span.end = span.end;
            continue;
        }
        if syntax == Syntax::Postgres && kind == TokenKind::Error {
            for c in src[span.clone()].chars() {
                match c {
                    '[' => brackets += 1,
                    ']' => brackets = brackets.saturating_sub(1),
                    _ => {}
                }
            }
        }
        // PostgreSQL's array slice, `a[1:n]`: its bound is no bind variable.
        // One after `[`, `,` or an operator still is (`array[:x, :y]`).
        if syntax == Syntax::Postgres
            && kind == TokenKind::BindVar
            && brackets > 0
            && tokens
                .iter()
                .rev()
                .find(|t| !t.kind.is_trivia())
                .is_some_and(|t| ends_operand(src, t))
        {
            tokens.push(Token {
                kind: TokenKind::Operator,
                span: span.start..span.start + 1,
            });
            let bound = span.start + 1..span.end;
            let text = &src[bound.clone()];
            let kind = if text.starts_with(|c: char| c.is_ascii_digit()) {
                TokenKind::Number
            } else if is_keyword(text) {
                TokenKind::Keyword
            } else {
                TokenKind::Ident
            };
            tokens.push(Token { kind, span: bound });
            continue;
        }
        // Merge consecutive error bytes into one token.
        if kind == TokenKind::Error
            && let Some(last) = tokens
                .last_mut()
                .filter(|t: &&mut Token| t.kind == TokenKind::Error && t.span.end == span.start)
        {
            last.span.end = span.end;
            continue;
        }
        tokens.push(Token { kind, span });
    }
    tokens
}

/// Whether `token` can end an operand, as the lower bound of a slice
/// before its `:`.
fn ends_operand(src: &str, token: &Token) -> bool {
    match token.kind {
        TokenKind::Ident
        | TokenKind::QuotedIdent
        | TokenKind::Keyword
        | TokenKind::Number
        | TokenKind::String
        | TokenKind::RParen => true,
        TokenKind::Error => src[token.span.clone()].ends_with(']'),
        _ => false,
    }
}

/// What starts the name of a T-SQL temporary table: `#` or `##`, and the
/// underscores after it.
fn is_temporary_prefix(text: &str) -> bool {
    let after = text.trim_start_matches('#');
    (1..=2).contains(&(text.len() - after.len())) && after.bytes().all(|b| b == b'_')
}

/// The texts of the first `n` tokens of a statement that aren't comments,
/// for telling what it does. In MySQL's syntax the code in a `/*!…*/` among
/// them counts, which the server runs (#219), whatever its version.
pub fn leading_tokens(sql: &str, syntax: Syntax, n: usize) -> Vec<&str> {
    let mut out = Vec::new();
    push_leading(sql, syntax, n, &mut out);
    out
}

fn push_leading<'a>(sql: &'a str, syntax: Syntax, n: usize, out: &mut Vec<&'a str>) {
    for t in tokenize_in(sql, syntax) {
        if out.len() >= n {
            return;
        }
        let text = &sql[t.span];
        if t.kind.is_trivia() {
            continue;
        }
        match text.strip_prefix("/*!") {
            Some(code) if syntax.is_mysql() && t.kind == TokenKind::Hint => {
                let code = code.strip_suffix("*/").unwrap_or(code);
                let code = code.trim_start_matches(|c: char| c.is_ascii_digit());
                push_leading(code, syntax, n, out);
            }
            _ => out.push(text),
        }
    }
}

/// A token MySQL reads differently from Oracle, as its kind and where it
/// ends; `None` when it reads it the same. A string has backslash escapes
/// unless the session's `sql_mode` turns them off (#412).
fn mysql_token(
    src: &str,
    kind: TokenKind,
    span: Range<usize>,
    escapes: bool,
) -> Option<(TokenKind, usize)> {
    let text = &src[span.clone()];
    match kind {
        TokenKind::LineComment => {
            // `--x` is two minus signs; `--` at the end of the text is a
            // comment.
            let after = src.as_bytes().get(span.start + 2)?;
            (!after.is_ascii_whitespace() && !after.is_ascii_control())
                .then_some((TokenKind::Operator, span.start + 1))
        }
        // An executable comment, `/*!…*/` or `/*!50003 …*/`, is code: the
        // server reads its version itself (#216). It shows as a hint.
        // MariaDB's `/*M!…*/` stays one: MySQL doesn't run it, and the
        // first line of a mariadb-dump is one for its own client.
        TokenKind::BlockComment if text.starts_with("/*!") => Some((TokenKind::Hint, span.end)),
        TokenKind::Error if text.starts_with('#') => {
            let end = src[span.start..]
                .find('\n')
                .map_or(src.len(), |i| span.start + i);
            Some((TokenKind::LineComment, end))
        }
        // MySQL's names can't hold `#`: there the comment starts.
        TokenKind::Ident | TokenKind::Keyword | TokenKind::BindVar | TokenKind::SubstVar => {
            let hash = text.find('#').filter(|&i| i > 0)?;
            let kind = match kind {
                TokenKind::Ident | TokenKind::Keyword if is_keyword(&text[..hash]) => {
                    TokenKind::Keyword
                }
                TokenKind::Keyword => TokenKind::Ident,
                other => other,
            };
            Some((kind, span.start + hash))
        }
        TokenKind::String | TokenKind::QuotedIdent if !text.starts_with(['$', '`']) => {
            let quote = text.find(['\'', '"'])?;
            let end = string_end(src, span.start + quote, escapes);
            (end != span.end || kind == TokenKind::QuotedIdent).then_some((TokenKind::String, end))
        }
        _ => None,
    }
}

/// A token PostgreSQL reads differently from Oracle, as its kind and where
/// it ends: `E'…'`, a string with backslash escapes (#202). `None` for the
/// others.
fn postgres_token(src: &str, kind: TokenKind, span: Range<usize>) -> Option<(TokenKind, usize)> {
    (kind.is_word()
        && src[span.clone()].eq_ignore_ascii_case("e")
        && src[span.end..].starts_with('\''))
    .then(|| (TokenKind::String, string_end(src, span.end, true)))
}

/// Where a string that opens at `open` ends, with backslash escapes or
/// without: after its closing quote (a doubled one stands for itself), or
/// at the end of the text.
fn string_end(src: &str, open: usize, escapes: bool) -> usize {
    let bytes = src.as_bytes();
    let quote = bytes[open];
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if escapes => i += 2,
            b if b == quote && bytes.get(i + 1) == Some(&quote) => i += 2,
            b if b == quote => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

/// `$` and a word make a selection or error directive; `$$` and a name an
/// inquiry directive.
fn is_directive(dollars: &str, word: &str) -> bool {
    const DIRECTIVES: [&str; 6] = ["IF", "THEN", "ELSIF", "ELSE", "END", "ERROR"];
    match dollars {
        "$" => DIRECTIVES.iter().any(|d| d.eq_ignore_ascii_case(word)),
        "$$" => true,
        _ => false,
    }
}

/// Consumes a block comment body up to and including `*/` (or end of input).
/// PostgreSQL and SQL Server nest them: `/* a /* b */ c */` is one comment.
fn block_comment(lex: &mut logos::Lexer<TokenKind>) {
    let rest = lex.remainder();
    let len = comment_body(rest, lex.extras).unwrap_or(rest.len());
    lex.bump(len);
}

/// The length of a block comment's body in `rest`, the text after its
/// `/*`, up to and including the `*/` that closes it; or, if `rest` ends
/// first, how many comments are still open there. Only PostgreSQL and SQL
/// Server count a `/*` inside.
fn comment_body(rest: &str, syntax: Syntax) -> Result<usize, usize> {
    let nests = matches!(syntax, Syntax::Postgres | Syntax::TSql);
    let bytes = rest.as_bytes();
    let (mut depth, mut i) = (1, 0);
    while i + 1 < bytes.len() {
        match &bytes[i..i + 2] {
            b"/*" if nests => {
                depth += 1;
                i += 2;
            }
            b"*/" => {
                depth -= 1;
                i += 2;
                if depth == 0 || !nests {
                    return Ok(i);
                }
            }
            _ => i += 1,
        }
    }
    Err(depth)
}

/// How many block comments are still open at the end of `comment`, a
/// block comment token's text (with its `/*`): 0 when it is closed.
/// Comments nest in PostgreSQL's and SQL Server's syntax.
pub fn open_comments(comment: &str, syntax: Syntax) -> usize {
    comment
        .strip_prefix("/*")
        .map_or(0, |body| comment_body(body, syntax).err().unwrap_or(0))
}

/// Consumes a string body after the opening quote, honouring `''` escapes.
fn string(lex: &mut logos::Lexer<TokenKind>) {
    let rest = lex.remainder().as_bytes();
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == b'\'' {
            if rest.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            lex.bump(i + 1);
            return;
        }
        i += 1;
    }
    lex.bump(rest.len());
}

/// Consumes the rest of a dollar-quoted string after its first `$`: the tag,
/// the body and the closing tag (or the rest of the input). A `$` that
/// doesn't open one stays an error token, as PostgreSQL's `$1` always was;
/// [`tokenize`] makes Oracle's `$IF` and `$$name` directives of it, so only
/// PostgreSQL's `$$` opens a body that starts with a word (#201). MySQL
/// has no dollar quotes: `$$` is text there, as in `DELIMITER $$`.
fn dollar_string(lex: &mut logos::Lexer<TokenKind>) -> bool {
    let syntax = lex.extras;
    if syntax.is_mysql() {
        return false;
    }
    let rest = lex.remainder();
    // A tag is a name: letters of any script too, `$função$` (#468).
    let tag_len = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    let tag = &rest[..tag_len];
    if !rest[tag_len..].starts_with('$') || tag.starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }
    let body = &rest[tag_len + 1..];
    if syntax != Syntax::Postgres
        && tag.is_empty()
        && body.starts_with(|c: char| c.is_alphanumeric() || c == '_')
    {
        return false;
    }
    let close = format!("${tag}$");
    let len = body.find(&close).map_or(body.len(), |i| i + close.len());
    lex.bump(tag_len + 1 + len);
    true
}

/// Consumes the rest of SQL Server's and SQLite's `[name]` (#208): up to
/// its `]` (a doubled one stands for itself), or the end of the line.
/// Elsewhere `[` stays an error token.
fn bracket_ident(lex: &mut logos::Lexer<TokenKind>) -> bool {
    if !matches!(lex.extras, Syntax::TSql | Syntax::Sqlite) {
        return false;
    }
    let rest = lex.remainder().as_bytes();
    let mut i = 0;
    while i < rest.len() && rest[i] != b'\n' {
        match rest[i] {
            b']' if rest.get(i + 1) == Some(&b']') => i += 2,
            b']' => {
                i += 1;
                break;
            }
            _ => i += 1,
        }
    }
    lex.bump(i);
    true
}

/// Consumes a `q'<delim>...<delim>'` body after the `q'` prefix.
fn q_string(lex: &mut logos::Lexer<TokenKind>) {
    let rest = lex.remainder();
    let Some(open) = rest.chars().next() else {
        return;
    };
    let close = match open {
        '[' => ']',
        '{' => '}',
        '(' => ')',
        '<' => '>',
        c => c,
    };
    let body = &rest[open.len_utf8()..];
    let mut terminator = [0u8; 5];
    let terminator = {
        let n = close.encode_utf8(&mut terminator).len();
        terminator[n] = b'\'';
        std::str::from_utf8(&terminator[..=n]).expect("valid utf8")
    };
    let len = body
        .find(terminator)
        .map_or(rest.len(), |i| open.len_utf8() + i + terminator.len());
    lex.bump(len);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize_ident;

    fn kinds(src: &str) -> Vec<(TokenKind, &str)> {
        tokenize(src)
            .into_iter()
            .filter(|t| !t.kind.is_trivia())
            .map(|t| (t.kind, &src[t.span]))
            .collect()
    }

    #[test]
    fn lossless() {
        let src = "select /*+ full(t) */ t.* from \"My Tab\" t -- c\nwhere x = :b1 and y = &&v. || q'[it's]' ; §";
        let tokens = tokenize(src);
        let mut pos = 0;
        for t in &tokens {
            assert_eq!(t.span.start, pos);
            pos = t.span.end;
        }
        assert_eq!(pos, src.len());
    }

    #[test]
    fn literals_and_comments() {
        insta::assert_debug_snapshot!(kinds(
            "SELECT 'it''s', q'{a}'' b}', nq'!x!', N'n', 1.5e3, .5, 1..10 FROM dual /* c */ /*+ hint */"
        ));
    }

    #[test]
    fn binds_and_operators() {
        insta::assert_debug_snapshot!(kinds("x := :new.id || &var; y => a <> b"));
    }

    #[test]
    fn unterminated() {
        assert_eq!(kinds("'abc"), vec![(TokenKind::String, "'abc")]);
        assert_eq!(
            tokenize("/* abc"),
            vec![Token {
                kind: TokenKind::BlockComment,
                span: 0..6
            }]
        );
        assert_eq!(kinds("q'[abc"), vec![(TokenKind::QString, "q'[abc")]);
        assert_eq!(kinds("$$ ab;c"), vec![(TokenKind::String, "$$ ab;c")]);
    }

    /// The bounds of PostgreSQL's array slices aren't bind variables;
    /// a `:name` as an element is one.
    #[test]
    fn postgres_slices_have_no_binds() {
        let src = "select a[1:2], a[i:n], a[f(x):n], b[1][2:3], array[:x, :y], a[1 + :z] from t \
                   where c = :w";
        let binds: Vec<&str> = tokenize_in(src, Syntax::Postgres)
            .into_iter()
            .filter(|t| t.kind == TokenKind::BindVar)
            .map(|t| &src[t.span])
            .collect();
        assert_eq!(binds, [":x", ":y", ":z", ":w"]);
        // Elsewhere they are.
        let oracle = tokenize("select a[i:n] from t");
        assert!(oracle.iter().any(|t| t.kind == TokenKind::BindVar));
    }

    /// PostgreSQL and SQL Server nest block comments; Oracle, MySQL and
    /// SQLite end one at the first `*/`.
    #[test]
    fn nested_block_comments() {
        let src = "/* off /* step 2 */ delete from t; */ select 1";
        let first = |syntax| {
            let t = &tokenize_in(src, syntax)[0];
            (t.kind, &src[t.span.clone()])
        };
        for syntax in [Syntax::Postgres, Syntax::TSql] {
            assert_eq!(
                first(syntax),
                (
                    TokenKind::BlockComment,
                    "/* off /* step 2 */ delete from t; */"
                ),
                "{syntax:?}"
            );
        }
        for syntax in [Syntax::Oracle, Syntax::MySql, Syntax::Sqlite] {
            assert_eq!(
                first(syntax),
                (TokenKind::BlockComment, "/* off /* step 2 */"),
                "{syntax:?}"
            );
        }
        assert_eq!(open_comments("/* a /* b */", Syntax::Postgres), 1);
        assert_eq!(open_comments("/* a /* b", Syntax::TSql), 2);
        assert_eq!(open_comments("/* a /* b */ */", Syntax::TSql), 0);
        assert_eq!(open_comments("/* a /* b */", Syntax::Oracle), 0);
        assert_eq!(open_comments("/* a /* b", Syntax::Oracle), 1);
        assert_eq!(open_comments("/**/", Syntax::Postgres), 0);
        assert_eq!(open_comments("/*/", Syntax::Postgres), 1);
    }

    fn mysql_kinds(src: &str) -> Vec<(TokenKind, &str)> {
        tokenize_in(src, Syntax::MySql)
            .into_iter()
            .filter(|t| !t.kind.is_trivia() || t.kind == TokenKind::LineComment)
            .map(|t| (t.kind, &src[t.span]))
            .collect()
    }

    #[test]
    fn mysql_strings_comments_and_dashes() {
        // A backslash escapes the next character (#187), in "…" too, which
        // is a string; a doubled quote still stands for one.
        assert_eq!(
            mysql_kinds(r#"select 'it\'s; here', "a\"b;c", 'x\\', 'y''z', N'\'' ;"#),
            vec![
                (TokenKind::Keyword, "select"),
                (TokenKind::String, r"'it\'s; here'"),
                (TokenKind::Comma, ","),
                (TokenKind::String, r#""a\"b;c""#),
                (TokenKind::Comma, ","),
                (TokenKind::String, r"'x\\'"),
                (TokenKind::Comma, ","),
                (TokenKind::String, "'y''z'"),
                (TokenKind::Comma, ","),
                (TokenKind::String, r"N'\''"),
                (TokenKind::Semicolon, ";"),
            ]
        );
        assert_eq!(
            mysql_kinds(r"'open\'"),
            vec![(TokenKind::String, r"'open\'")]
        );
        // `#` comments (#188), also right after a name.
        assert_eq!(
            mysql_kinds("# it's; gone\nselect a#b\n, 1"),
            vec![
                (TokenKind::LineComment, "# it's; gone"),
                (TokenKind::Keyword, "select"),
                (TokenKind::Ident, "a"),
                (TokenKind::LineComment, "#b"),
                (TokenKind::Comma, ","),
                (TokenKind::Number, "1"),
            ]
        );
        // `--` is a comment before whitespace, a control character or the
        // end, otherwise a minus (#189).
        assert_eq!(
            mysql_kinds("1 --x\n-- y\n--\t\n2--"),
            vec![
                (TokenKind::Number, "1"),
                (TokenKind::Operator, "-"),
                (TokenKind::Operator, "-"),
                (TokenKind::Ident, "x"),
                (TokenKind::LineComment, "-- y"),
                (TokenKind::LineComment, "--\t"),
                (TokenKind::Number, "2"),
                (TokenKind::LineComment, "--"),
            ]
        );
        // Backticks and Oracle's lexing stay as they were.
        assert_eq!(
            mysql_kinds("`a\\`"),
            vec![(TokenKind::QuotedIdent, "`a\\`")]
        );
        assert_eq!(
            tokenize(r"'a\' #--x")
                .into_iter()
                .map(|t| t.kind)
                .collect::<Vec<_>>(),
            [
                TokenKind::String,
                TokenKind::Whitespace,
                TokenKind::Error,
                TokenKind::LineComment
            ]
        );
    }

    #[test]
    fn mysql_executable_comments() {
        // `/*!…*/` is code the server reads (#216); MariaDB's `/*M!…*/`
        // and other comments stay comments.
        assert_eq!(
            mysql_kinds("/*!40101 SET x=1 */; /*M!999999 y */ /* z */ /*!*/"),
            vec![
                (TokenKind::Hint, "/*!40101 SET x=1 */"),
                (TokenKind::Semicolon, ";"),
                (TokenKind::Hint, "/*!*/"),
            ]
        );
        assert_eq!(
            kinds("/*!40101 SET x=1 */;"),
            vec![(TokenKind::Semicolon, ";")]
        );
    }

    #[test]
    fn leading_tokens_of_statements() {
        let mysql = |sql| leading_tokens(sql, Syntax::MySql, 2);
        assert_eq!(mysql("/* x */ update t set a = 1"), ["update", "t"]);
        assert_eq!(mysql("(select 1)"), ["(", "select"]);
        // The code in `/*!…*/` (#219), not in MariaDB's `/*M!…*/`.
        assert_eq!(
            mysql("/*!40000 ALTER TABLE `t` DISABLE KEYS */"),
            ["ALTER", "TABLE"]
        );
        assert_eq!(
            mysql("/*!50003 CREATE*/ /*!50017 DEFINER=`root`@`localhost`*/ /*!50003 TRIGGER t"),
            ["CREATE", "DEFINER"]
        );
        assert_eq!(
            mysql("/*!*/ /*!50000 */ /*M!100100 drop */ delete"),
            ["delete"]
        );
        assert_eq!(
            leading_tokens("/*!40000 ALTER TABLE t */ select 1", Syntax::Oracle, 1),
            ["select"]
        );
    }

    #[test]
    fn mysql_backticks() {
        assert_eq!(
            kinds("select `a b`.`x``y` from `t;1` `open"),
            vec![
                (TokenKind::Keyword, "select"),
                (TokenKind::QuotedIdent, "`a b`"),
                (TokenKind::Dot, "."),
                (TokenKind::QuotedIdent, "`x``y`"),
                (TokenKind::Keyword, "from"),
                (TokenKind::QuotedIdent, "`t;1`"),
                (TokenKind::QuotedIdent, "`open"),
            ]
        );
    }

    #[test]
    fn postgres_syntax() {
        assert_eq!(
            kinds("as $$ begin x := 1; end; $$, $f$ a $$ b $f$"),
            vec![
                (TokenKind::Keyword, "as"),
                (TokenKind::String, "$$ begin x := 1; end; $$"),
                (TokenKind::Comma, ","),
                (TokenKind::String, "$f$ a $$ b $f$"),
            ]
        );
        assert_eq!(
            kinds("x::int = :y"),
            vec![
                (TokenKind::Ident, "x"),
                (TokenKind::Operator, "::"),
                (TokenKind::Ident, "int"),
                (TokenKind::Operator, "="),
                (TokenKind::BindVar, ":y"),
            ]
        );
        // Oracle's conditional compilation and PostgreSQL's parameters.
        assert_eq!(
            kinds("$IF $$debug $THEN $1"),
            vec![
                (TokenKind::Directive, "$IF"),
                (TokenKind::Directive, "$$debug"),
                (TokenKind::Directive, "$THEN"),
                (TokenKind::Error, "$"),
                (TokenKind::Number, "1"),
            ]
        );
        assert_eq!(
            kinds("$else $End $error 'x' $$PLSQL_UNIT"),
            vec![
                (TokenKind::Directive, "$else"),
                (TokenKind::Directive, "$End"),
                (TokenKind::Directive, "$error"),
                (TokenKind::String, "'x'"),
                (TokenKind::Directive, "$$PLSQL_UNIT"),
            ]
        );
        // Only the directives: a `$` before another word, or apart from it,
        // stays an error.
        assert_eq!(
            kinds("$loop $ if x$if"),
            vec![
                (TokenKind::Error, "$"),
                (TokenKind::Keyword, "loop"),
                (TokenKind::Error, "$"),
                (TokenKind::Keyword, "if"),
                (TokenKind::Ident, "x$if"),
            ]
        );
    }

    #[test]
    fn bracketed_names() {
        // SQL Server's and SQLite's `[name]` (#208).
        for syntax in [Syntax::TSql, Syntax::Sqlite] {
            let src = "select [a b].[x]]y] from [t;1] [open";
            let kinds: Vec<_> = tokenize_in(src, syntax)
                .into_iter()
                .filter(|t| !t.kind.is_trivia())
                .map(|t| (t.kind, &src[t.span]))
                .collect();
            assert_eq!(
                kinds,
                vec![
                    (TokenKind::Keyword, "select"),
                    (TokenKind::QuotedIdent, "[a b]"),
                    (TokenKind::Dot, "."),
                    (TokenKind::QuotedIdent, "[x]]y]"),
                    (TokenKind::Keyword, "from"),
                    (TokenKind::QuotedIdent, "[t;1]"),
                    (TokenKind::QuotedIdent, "[open"),
                ],
                "{syntax:?}"
            );
        }
        assert_eq!(
            kinds("[a]"),
            vec![
                (TokenKind::Error, "["),
                (TokenKind::Ident, "a"),
                (TokenKind::Error, "]"),
            ]
        );
        assert_eq!(normalize_ident("[x]]y]"), ("x]y".into(), true));
        assert_eq!(normalize_ident("`a``b`"), ("a`b".into(), true));
        assert_eq!(normalize_ident("\"My\"\"T\""), ("My\"T".into(), true));
        assert_eq!(normalize_ident("emp"), ("EMP".into(), false));
    }

    #[test]
    fn tsql_temporary_tables() {
        // `#name` and `##name` are names on SQL Server (#320), `#` alone
        // isn't, and elsewhere `#` is no name's start.
        let src = "select * from #t, ##g, #_x, #table, ###no # t";
        let words = |syntax| -> Vec<(TokenKind, &str)> {
            tokenize_in(src, syntax)
                .into_iter()
                .filter(|t| !t.kind.is_trivia())
                .skip(3)
                .map(|t| (t.kind, &src[t.span]))
                .collect()
        };
        assert_eq!(
            words(Syntax::TSql),
            vec![
                (TokenKind::Ident, "#t"),
                (TokenKind::Comma, ","),
                (TokenKind::Ident, "##g"),
                (TokenKind::Comma, ","),
                (TokenKind::Ident, "#_x"),
                (TokenKind::Comma, ","),
                (TokenKind::Ident, "#table"),
                (TokenKind::Comma, ","),
                (TokenKind::Error, "###"),
                (TokenKind::Ident, "no"),
                (TokenKind::Error, "#"),
                (TokenKind::Ident, "t"),
            ]
        );
        assert_eq!(words(Syntax::Sqlite)[0], (TokenKind::Error, "#"));
    }

    fn postgres_kinds(src: &str) -> Vec<(TokenKind, &str)> {
        tokenize_in(src, Syntax::Postgres)
            .into_iter()
            .filter(|t| !t.kind.is_trivia())
            .map(|t| (t.kind, &src[t.span]))
            .collect()
    }

    #[test]
    fn postgres_dollar_quotes() {
        // A body may start with a letter or `_` right after the tag (#201).
        assert_eq!(
            postgres_kinds("as $$BEGIN RETURN 3; END$$; $$abc$$, $_x$a;$_x$, $1"),
            vec![
                (TokenKind::Keyword, "as"),
                (TokenKind::String, "$$BEGIN RETURN 3; END$$"),
                (TokenKind::Semicolon, ";"),
                (TokenKind::String, "$$abc$$"),
                (TokenKind::Comma, ","),
                (TokenKind::String, "$_x$a;$_x$"),
                (TokenKind::Comma, ","),
                (TokenKind::Error, "$"),
                (TokenKind::Number, "1"),
            ]
        );
        // PostgreSQL has no directives.
        assert_eq!(
            postgres_kinds("$IF $$x"),
            vec![
                (TokenKind::Error, "$"),
                (TokenKind::Keyword, "IF"),
                (TokenKind::String, "$$x"),
            ]
        );
        // MySQL has no dollar quotes: `$$` is text, as in `DELIMITER $$`.
        assert_eq!(
            mysql_kinds("select $$ a; $$ $$b"),
            vec![
                (TokenKind::Keyword, "select"),
                (TokenKind::Error, "$$"),
                (TokenKind::Ident, "a"),
                (TokenKind::Semicolon, ";"),
                (TokenKind::Error, "$$"),
                (TokenKind::Error, "$$"),
                (TokenKind::Ident, "b"),
            ]
        );
    }

    #[test]
    fn postgres_escape_strings() {
        // A backslash escapes the next character in `E'…'` only (#202).
        assert_eq!(
            postgres_kinds(r"select E'it\'s; here', e'\\', 'a\', E 'b', N'c'"),
            vec![
                (TokenKind::Keyword, "select"),
                (TokenKind::String, r"E'it\'s; here'"),
                (TokenKind::Comma, ","),
                (TokenKind::String, r"e'\\'"),
                (TokenKind::Comma, ","),
                (TokenKind::String, r"'a\'"),
                (TokenKind::Comma, ","),
                (TokenKind::Ident, "E"),
                (TokenKind::String, "'b'"),
                (TokenKind::Comma, ","),
                (TokenKind::String, "N'c'"),
            ]
        );
        assert_eq!(
            kinds(r"E'a\' x"),
            vec![
                (TokenKind::Ident, "E"),
                (TokenKind::String, r"'a\'"),
                (TokenKind::Ident, "x"),
            ]
        );
    }
}
