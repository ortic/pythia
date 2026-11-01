//! What is sent to the model.
//!
//! Context comes in [`Part`]s labeled with what they contain. Metadata
//! (definitions, keys, indexes, statistics, plans, source code, errors) is
//! always sent; query results and bind variable values only when the user
//! allowed them in Settings → AI. [`render`] and [`request`] are the only
//! ways parts become text for a model, so the policy holds for every request.

use pythia_config::AiSettings;

use crate::client::{ChatRequest, Message, Role};

/// Instructions that apply to every request.
pub const SYSTEM: &str = "You are the assistant built into Pythia, an IDE for Oracle SQL and PL/SQL \
that also works with MySQL, PostgreSQL, SQL Server and SQLite. You help developers write queries and PL/SQL \
and find out why SQL is slow.\n\
- Answer for the database and version given in the context; without one, for Oracle Database.\n\
- Base your answer on the context. Use only tables and columns it lists; when something you need \
is missing, say so instead of guessing names.\n\
- Put SQL and PL/SQL in fenced code blocks marked sql, each complete and runnable on its own.\n\
- Be concise: lead with the answer, then the reasoning that matters.\n\
- You can't run anything; the user decides what to execute.";

/// Marks text shortened to fit the model.
const SHORTENED: &str = "\n… (shortened)";

/// What a context part contains, which decides whether it may be sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sensitivity {
    /// Definitions, statistics, plans, source code, errors: always sent.
    Metadata,
    /// Rows of query results.
    ResultData,
    /// Values entered or captured for bind variables.
    BindValues,
}

/// A piece of context, e.g. the statement under the cursor or a table's
/// definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub title: String,
    pub text: String,
    pub sensitivity: Sensitivity,
    /// Sent as a fenced SQL block.
    pub code: bool,
}

impl Part {
    pub fn text(title: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(title, text, Sensitivity::Metadata, false)
    }

    pub fn code(title: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(title, text, Sensitivity::Metadata, true)
    }

    pub fn result_data(title: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(title, text, Sensitivity::ResultData, false)
    }

    pub fn bind_values(title: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(title, text, Sensitivity::BindValues, false)
    }

    fn new(
        title: impl Into<String>,
        text: impl Into<String>,
        sensitivity: Sensitivity,
        code: bool,
    ) -> Self {
        Self {
            title: title.into(),
            text: text.into(),
            sensitivity,
            code,
        }
    }

    fn section(&self, text: &str) -> String {
        if self.code {
            format!("## {}\n```sql\n{}\n```\n\n", self.title, text.trim_end())
        } else {
            format!("## {}\n{}\n\n", self.title, text.trim_end())
        }
    }
}

/// What may be sent besides metadata.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Policy {
    pub result_data: bool,
    pub bind_values: bool,
}

impl Policy {
    pub fn new(settings: &AiSettings) -> Self {
        Self {
            result_data: settings.send_result_data,
            bind_values: settings.send_bind_values,
        }
    }

    pub fn allows(self, sensitivity: Sensitivity) -> bool {
        match sensitivity {
            Sensitivity::Metadata => true,
            Sensitivity::ResultData => self.result_data,
            Sensitivity::BindValues => self.bind_values,
        }
    }
}

/// What the assistant is asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Task {
    /// An open question.
    Ask(String),
    /// SQL from a plain-language description.
    Generate(String),
    Explain,
    Fix,
    Rewrite,
    /// Explain an execution plan and point out problems.
    AnalyzePlan,
    /// Slow SQL, a session or a report, e.g. "top SQL 7h35uxf5uhmm1".
    AnalyzeSlow(String),
    /// Lay out a statement by these rules (one per line), changing nothing
    /// but whitespace and case.
    Format(String),
}

impl Task {
    /// How the request appears in the conversation.
    pub fn title(&self) -> String {
        match self {
            Task::Ask(question) => question.clone(),
            Task::Generate(description) => format!("Generate SQL: {description}"),
            Task::Explain => "Explain the statement".into(),
            Task::Fix => "Fix the statement".into(),
            Task::Rewrite => "Rewrite the statement".into(),
            Task::AnalyzePlan => "Analyze the execution plan".into(),
            Task::AnalyzeSlow(what) => format!("Analyze {what}"),
            Task::Format(_) => "Format the statement".into(),
        }
    }

    fn instruction(&self) -> String {
        match self {
            Task::Ask(question) => question.clone(),
            Task::Generate(description) => format!(
                "Write a SQL query for this request, for the database in the context:\n{description}\n\n\
                 Use the tables, columns and foreign keys from the context. Answer with the query \
                 in one sql code block, then state any assumptions in a sentence or two."
            ),
            Task::Explain => "Explain the statement in the context in plain language: what it \
                does, step by step, and anything surprising or risky about it."
                .into(),
            Task::Fix => "The statement in the context fails or doesn't compile; the errors are \
                listed. Find the cause and give the corrected statement in one sql code block (for \
                a long program unit, only the corrected subprograms), then explain the fix briefly."
                .into(),
            Task::Rewrite => "Rewrite the statement in the context so it runs faster and reads \
                better while returning exactly the same result. Give the rewritten statement in \
                one sql code block, then explain each change and why it helps."
                .into(),
            Task::AnalyzePlan => "Explain the execution plan in the context in plain language. \
                Point out likely problems: full scans of large tables, bad cardinality estimates, \
                missing or unused indexes, implicit conversions, expensive sorts and joins. \
                Suggest concrete improvements (rewrites, indexes, statistics to gather), each with \
                its reasoning; put SQL and DDL in sql code blocks."
                .into(),
            Task::AnalyzeSlow(what) => format!(
                "Analyze {what}. Using the statistics, plans, waits and reports in the context, \
                 explain where the time goes and why, and suggest concrete improvements \
                 (rewrites, indexes, statistics to gather, configuration), each with its \
                 reasoning; put SQL and DDL in sql code blocks."
            ),
            Task::Format(rules) => format!(
                "Lay out the statement in the context by these rules:\n{rules}\n\n\
                 Change only whitespace, line breaks and the case of keywords and unquoted \
                 identifiers. Never add, remove, reorder or rename anything, and keep comments, \
                 quoted identifiers and string literals exactly as written. Answer with the \
                 statement in one sql code block and nothing else."
            ),
        }
    }
}

/// One request and its answer, with the context it was asked with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub task: Task,
    pub parts: Vec<Part>,
    pub answer: Option<String>,
}

impl Turn {
    pub fn new(task: Task, parts: Vec<Part>) -> Self {
        Self {
            task,
            parts,
            answer: None,
        }
    }
}

/// A turn's message as sent, and what was left out of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    /// Titles of parts the policy kept back.
    pub withheld: Vec<String>,
    /// Titles of parts shortened or left out to fit the model.
    pub shortened: Vec<String>,
}

/// The message of a turn: the context the policy allows (within `limit`
/// characters), then the instruction.
pub fn render(turn: &Turn, policy: Policy, limit: usize) -> Rendered {
    let instruction = turn.task.instruction();
    let mut budget = limit.saturating_sub(instruction.len());
    let mut out = Rendered::default();
    let mut context = String::new();
    for part in &turn.parts {
        if !policy.allows(part.sensitivity) {
            out.withheld.push(part.title.clone());
            continue;
        }
        let section = part.section(&part.text);
        if section.len() <= budget {
            budget -= section.len();
            context.push_str(&section);
            continue;
        }
        out.shortened.push(part.title.clone());
        let overhead = part.section("").len() + SHORTENED.len();
        if budget > overhead + 200 {
            let text = format!("{}{SHORTENED}", cut(&part.text, budget - overhead));
            let section = part.section(&text);
            budget = budget.saturating_sub(section.len());
            context.push_str(&section);
        }
    }
    out.text = if context.is_empty() {
        instruction
    } else {
        format!("{context}{instruction}")
    };
    out
}

/// The request for the last turn, after as many earlier answered turns as
/// fit in `limit` characters. Earlier turns are rendered again, so they
/// follow the current policy too. Returns the last turn's message as well.
pub fn request(turns: &[Turn], policy: Policy, limit: usize) -> (ChatRequest, Rendered) {
    let Some((last, earlier)) = turns.split_last() else {
        return Default::default();
    };
    let current = render(last, policy, limit);
    let mut budget = limit.saturating_sub(current.text.len());
    let mut history = Vec::new();
    for turn in earlier.iter().rev() {
        let Some(answer) = &turn.answer else {
            continue;
        };
        let asked = render(turn, policy, limit).text;
        let size = asked.len() + answer.len();
        if size > budget {
            break;
        }
        budget -= size;
        history.push((asked, answer.clone()));
    }
    let mut messages = Vec::new();
    for (asked, answer) in history.into_iter().rev() {
        messages.push(Message {
            role: Role::User,
            content: asked,
        });
        messages.push(Message {
            role: Role::Assistant,
            content: answer,
        });
    }
    messages.push(Message {
        role: Role::User,
        content: current.text.clone(),
    });
    let request = ChatRequest {
        system: SYSTEM.to_owned(),
        messages,
    };
    (request, current)
}

/// The start of `text` up to `max` bytes, ending at a line break if one is
/// in the second half.
fn cut(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    match text[..end].rfind('\n') {
        Some(line) if line >= end / 2 => &text[..line],
        _ => &text[..end],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_text(request: &ChatRequest) -> String {
        let mut text = request.system.clone();
        for m in &request.messages {
            text.push_str(&m.content);
        }
        text
    }

    fn turn() -> Turn {
        Turn::new(
            Task::Explain,
            vec![
                Part::code("Statement", "select * from emp where id = :id"),
                Part::text("Table SCOTT.EMP", "EMPNO NUMBER(4) NOT NULL PK"),
                Part::result_data("Result rows", "7839 | KING | 5000"),
                Part::bind_values("Bind values", ":id = 7839"),
            ],
        )
    }

    #[test]
    fn data_is_withheld_unless_allowed() {
        let (request, rendered) = request(&[turn()], Policy::default(), 100_000);
        let text = all_text(&request);
        assert!(text.contains("select * from emp where id = :id"));
        assert!(text.contains("EMPNO NUMBER(4) NOT NULL PK"));
        assert!(!text.contains("KING"), "result rows sent: {text}");
        assert!(!text.contains("7839"), "bind value sent: {text}");
        assert_eq!(rendered.withheld, ["Result rows", "Bind values"]);
        assert!(rendered.shortened.is_empty());
        // The instruction comes after the context.
        let message = &request.messages[0].content;
        assert!(message.find("## Statement").unwrap() < message.find("Explain the").unwrap());
        assert!(message.contains("```sql\nselect * from emp where id = :id\n```"));
    }

    #[test]
    fn data_is_sent_when_allowed() {
        let results = Policy {
            result_data: true,
            bind_values: false,
        };
        let text = all_text(&request(&[turn()], results, 100_000).0);
        assert!(text.contains("KING"));
        assert!(!text.contains(":id = 7839"));

        let binds = Policy {
            result_data: false,
            bind_values: true,
        };
        let text = all_text(&request(&[turn()], binds, 100_000).0);
        assert!(!text.contains("KING"));
        assert!(text.contains(":id = 7839"));
    }

    #[test]
    fn policy_follows_settings() {
        let mut settings = AiSettings::default();
        assert_eq!(Policy::new(&settings), Policy::default());
        settings.send_bind_values = true;
        assert!(Policy::new(&settings).allows(Sensitivity::BindValues));
        assert!(!Policy::new(&settings).allows(Sensitivity::ResultData));
        assert!(Policy::new(&settings).allows(Sensitivity::Metadata));
    }

    #[test]
    fn earlier_turns_follow_the_current_policy() {
        let mut first = turn();
        first.answer = Some("It reads one employee.".into());
        let follow_up = Turn::new(Task::Ask("And with an index?".into()), Vec::new());
        let (request, _) = request(&[first, follow_up], Policy::default(), 100_000);
        let roles: Vec<Role> = request.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, [Role::User, Role::Assistant, Role::User]);
        assert_eq!(request.messages[1].content, "It reads one employee.");
        assert_eq!(request.messages[2].content, "And with an index?");
        let text = all_text(&request);
        assert!(text.contains("from emp"));
        assert!(!text.contains("KING") && !text.contains("7839"));
    }

    #[test]
    fn unanswered_and_old_turns_are_left_out() {
        let mut failed = turn();
        failed.answer = None;
        let mut old = Turn::new(Task::Ask("x".repeat(500)), Vec::new());
        old.answer = Some("old answer".into());
        let mut recent = Turn::new(Task::Ask("recent".into()), Vec::new());
        recent.answer = Some("recent answer".into());
        let last = Turn::new(Task::Ask("now".into()), Vec::new());
        let turns = [old, failed, recent, last];
        let (request, _) = request(&turns, Policy::default(), 300);
        let contents: Vec<&str> = request
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(contents, ["recent", "recent answer", "now"]);
    }

    #[test]
    fn long_context_is_shortened() {
        let source = (1..=2000)
            .map(|i| format!("  l_{i} number;"))
            .collect::<Vec<_>>()
            .join("\n");
        let turn = Turn::new(
            Task::Fix,
            vec![
                Part::text("Errors", "PLS-00201: identifier 'X' must be declared"),
                Part::code("Source", source),
                Part::text("Table T", "never fits"),
            ],
        );
        let rendered = render(&turn, Policy::default(), 4000);
        assert!(rendered.text.len() <= 4000, "{}", rendered.text.len());
        assert!(rendered.text.contains("PLS-00201"));
        assert!(rendered.text.contains("… (shortened)\n```"));
        assert!(rendered.text.ends_with("explain the fix briefly."));
        assert!(!rendered.text.contains("never fits"));
        assert_eq!(rendered.shortened, ["Source", "Table T"]);
    }

    #[test]
    fn cuts_at_lines_and_char_boundaries() {
        assert_eq!(cut("short", 10), "short");
        assert_eq!(cut("line one\nline two", 12), "line one");
        assert_eq!(cut("äöü", 3), "ä");
    }

    #[test]
    fn titles() {
        assert_eq!(
            Task::Generate("orders without invoice".into()).title(),
            "Generate SQL: orders without invoice"
        );
        assert_eq!(Task::AnalyzePlan.title(), "Analyze the execution plan");
    }
}
