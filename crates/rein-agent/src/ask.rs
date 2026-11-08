//! Asking the user: a question or a few, each with choices, when the
//! answer changes the work. Each model gets the tool its own harness gave
//! it: GPT models Codex's `request_user_input`, the rest Claude Code's
//! AskUserQuestion, as `ask`. Both come to one [`Question`], and the
//! answers go back as that harness gives them.
//!
//! You can always write your own answer instead of a choice, or skip:
//! then the model goes on with its best judgment and says what it assumed.
//! Where nobody's there to answer (a task run on its own), it's told so.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::ToolName;
use rein_llm::{Loading, Tool, ToolCall};

/// Your answers, as `told` gave them to the model, a line each, for
/// showing under the step: `"Which database?"="SQLite"`, or `SQLite`
/// where Codex's are keyed by id.
pub fn shown(output: &str) -> Vec<String> {
    if let Some(rest) = output.strip_prefix(ANSWERED) {
        let said = rest.split(CARRY_ON).next().unwrap_or_default();
        return said.split("\", \"").map(|pair| pair.trim_matches('"').replace("\"=\"", " → ")).collect();
    }
    let first = output.lines().next().unwrap_or_default();
    let Ok(Value::Object(read)) = serde_json::from_str::<Value>(first) else { return Vec::new() };
    let Some(Value::Object(by_id)) = read.get("answers") else { return Vec::new() };
    by_id.iter().map(|(id, answer)| format!("{id} → {}", answer["answers"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))).collect()
}

/// What the model is told when you don't answer.
pub const UNANSWERED: &str = "The user didn't answer. Go on with your best judgment, and say what you assumed.";

/// How Claude Code tells the model your answers: this, the answers, then
/// [`CARRY_ON`].
pub const ANSWERED: &str = "User has answered your questions: ";
pub const CARRY_ON: &str = ". You can now continue with the user's answers in mind.";

/// What the model is told when nobody's there to answer.
pub const NOBODY: &str = "Nobody is here to answer: this runs on its own. Go on with your best judgment, and say what you assumed.";

/// One question, as either harness asks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// Codex's, to key its answer by; Claude Code's go by the question.
    pub id: Option<String>,
    /// A word or two: `Auth method`.
    pub header: String,
    pub question: String,
    pub choices: Vec<Choice>,
    /// More than one may be picked.
    pub many: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Choice {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

/// How you answered one question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The labels of what you picked.
    Picked(Vec<String>),
    /// What you wrote instead.
    Wrote(String),
    Skipped,
}

/// Claude Code's AskUserQuestion, as `ask` takes it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Asked {
    questions: Vec<ClaudeQuestion>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeQuestion {
    question: String,
    header: String,
    options: Vec<Choice>,
    #[serde(default)]
    multi_select: Option<bool>,
}

/// Codex's `request_user_input`.
#[derive(Deserialize)]
struct Requested {
    questions: Vec<CodexQuestion>,
}

#[derive(Deserialize)]
struct CodexQuestion {
    id: String,
    header: String,
    question: String,
    options: Vec<Choice>,
}

/// The most questions asked at once, and the most choices for one.
const MOST_QUESTIONS: usize = 4;
const MOST_CHOICES: usize = 4;

/// The questions `call` asks, or what's wrong with them.
pub fn questions(call: &ToolCall) -> Result<Vec<Question>, String> {
    let bad = |e: serde_json::Error| format!("bad arguments: {e}");
    let questions: Vec<Question> = match ToolName::of(call) {
        Some(ToolName::RequestUserInput) => serde_json::from_str::<Requested>(&call.arguments)
            .map_err(bad)?
            .questions
            .into_iter()
            .map(|q| Question { id: Some(q.id), header: q.header, question: q.question, choices: q.options, many: false })
            .collect(),
        _ => serde_json::from_str::<Asked>(&call.arguments)
            .map_err(bad)?
            .questions
            .into_iter()
            .map(|q| Question { id: None, header: q.header, question: q.question, choices: q.options, many: q.multi_select.unwrap_or(false) })
            .collect(),
    };
    if questions.is_empty() || questions.len() > MOST_QUESTIONS {
        return Err(format!("ask 1 to {MOST_QUESTIONS} questions at once, not {}", questions.len()));
    }
    for question in &questions {
        if question.question.trim().is_empty() {
            return Err("each question needs its question".into());
        }
        if !(2..=MOST_CHOICES).contains(&question.choices.len()) {
            return Err(format!("\"{}\" needs 2 to {MOST_CHOICES} choices, not {}; the user can always write their own", question.question, question.choices.len()));
        }
    }
    Ok(questions)
}

/// What the model is told of `answers` to `questions`, as the harness
/// `call` comes from says it: Codex's JSON by question id, or Claude
/// Code's line of each question and its answer.
pub fn told(call: &ToolCall, questions: &[Question], answers: &[Answer]) -> String {
    let answered: Vec<(&Question, &Answer)> = questions.iter().zip(answers).filter(|(_, answer)| **answer != Answer::Skipped).collect();
    if answered.is_empty() {
        return UNANSWERED.to_owned();
    }
    let skipped = questions.len() - answered.len();
    let rest = if skipped == 0 { String::new() } else { format!("\n{skipped} not answered: go on with your best judgment for those, and say what you assumed.") };
    if ToolName::of(call) == Some(ToolName::RequestUserInput) {
        let by_id: serde_json::Map<String, Value> = answered
            .iter()
            .map(|(question, answer)| {
                let said = match answer {
                    Answer::Picked(labels) => labels.clone(),
                    Answer::Wrote(text) => vec![format!("user_note: {text}")],
                    Answer::Skipped => Vec::new(),
                };
                (question.id.clone().unwrap_or_default(), json!({ "answers": said }))
            })
            .collect();
        return format!("{}{rest}", json!({ "answers": by_id }));
    }
    let pairs: Vec<String> = answered
        .iter()
        .map(|(question, answer)| {
            let said = match answer {
                Answer::Picked(labels) => labels.join(", "),
                Answer::Wrote(text) => text.clone(),
                Answer::Skipped => String::new(),
            };
            format!("\"{}\"=\"{said}\"", question.question)
        })
        .collect();
    format!("{ANSWERED}{}{CARRY_ON}{rest}", pairs.join(", "))
}

const WHEN: &str = "Ask the user short questions with choices, and wait for the answers: only for a choice \
                    you can't settle from the request, the code or a sensible default, that changes what you \
                    do next. Prefer a reasonable assumption; never ask permission to run something. The user \
                    can write their own answer or skip: then use your best judgment, and say what you assumed.";

/// Claude Code's AskUserQuestion, as rein's `ask`.
pub fn spec() -> Tool {
    let choice = json!({
        "type": "object",
        "properties": {
            "label": { "type": "string", "description": "1 to 5 words." },
            "description": { "type": "string", "description": "Its trade-off, in a few words." }
        },
        "required": ["label", "description"],
        "additionalProperties": false
    });
    Tool {
        grammar: None,
        loading: Loading::UpFront,
        strict: true,
        name: ToolName::Ask.as_str().into(),
        description: format!("{WHEN} Put the choice you recommend first, its label ending \"(Recommended)\"; there's always an Other."),
        parameters: json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "description": "1 to 4.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": { "type": "string", "description": "Clear on its own, ending with ?" },
                            "header": { "type": "string", "description": "At most 12 characters." },
                            "options": { "type": "array", "description": "2 to 4.", "items": choice },
                            "multiSelect": { "type": "boolean", "description": "Several may be picked." }
                        },
                        "required": ["question", "header", "options", "multiSelect"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["questions"],
            "additionalProperties": false
        }),
    }
}

/// Codex's `request_user_input`, as Codex gives it to its models.
pub fn codex_spec() -> Tool {
    let choice = json!({
        "type": "object",
        "properties": {
            "label": { "type": "string", "description": "User-facing label (1-5 words)." },
            "description": { "type": "string", "description": "One short sentence explaining impact/tradeoff if selected." }
        },
        "required": ["label", "description"],
        "additionalProperties": false
    });
    Tool {
        grammar: None,
        loading: Loading::UpFront,
        strict: false,
        name: ToolName::RequestUserInput.as_str().into(),
        description: format!("Request user input for one to three short questions and wait for the response. {WHEN}"),
        parameters: json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "description": "Questions to show the user. Prefer 1 and do not exceed 3",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string", "description": "Stable identifier for mapping answers (snake_case)." },
                            "header": { "type": "string", "description": "Short header label shown in the UI (12 or fewer chars)." },
                            "question": { "type": "string", "description": "Single-sentence prompt shown to the user." },
                            "options": {
                                "type": "array",
                                "description": "Provide 2-3 mutually exclusive choices. Put the recommended option first and suffix its label with \"(Recommended)\". Do not include an \"Other\" option in this list; the client will add a free-form \"Other\" option automatically.",
                                "items": choice
                            }
                        },
                        "required": ["id", "header", "question", "options"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["questions"],
            "additionalProperties": false
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: ToolName, arguments: Value) -> ToolCall {
        ToolCall { call_id: "c1".into(), name: name.as_str().into(), arguments: arguments.to_string() }
    }

    fn claude_asks() -> ToolCall {
        call(
            ToolName::Ask,
            json!({ "questions": [
                { "question": "Which database?", "header": "Database", "options": [{ "label": "Postgres (Recommended)", "description": "What you run already." }, { "label": "SQLite", "description": "One file." }], "multiSelect": false },
                { "question": "Which checks?", "header": "Checks", "options": [{ "label": "Tests", "description": "" }, { "label": "Lint", "description": "" }], "multiSelect": true }
            ] }),
        )
    }

    fn codex_asks() -> ToolCall {
        call(
            ToolName::RequestUserInput,
            json!({ "questions": [
                { "id": "db", "header": "Database", "question": "Which database?", "options": [{ "label": "Postgres", "description": "" }, { "label": "SQLite", "description": "" }] },
                { "id": "name", "header": "Name", "question": "What name?", "options": [{ "label": "rein", "description": "" }, { "label": "reins", "description": "" }] }
            ] }),
        )
    }

    #[test]
    fn both_harnesses_questions_come_to_one_shape() {
        let claude = questions(&claude_asks()).unwrap();
        assert_eq!((claude[0].id.as_deref(), claude[0].header.as_str(), claude[0].many, claude[1].many), (None, "Database", false, true));
        assert_eq!(claude[0].choices[0].label, "Postgres (Recommended)");
        let codex = questions(&codex_asks()).unwrap();
        assert_eq!((codex[0].id.as_deref(), codex[1].question.as_str(), codex[1].many), (Some("db"), "What name?", false));
    }

    #[test]
    fn answers_go_back_as_each_harness_gives_them() {
        let asked = claude_asks();
        let said = told(&asked, &questions(&asked).unwrap(), &[Answer::Picked(vec!["SQLite".into()]), Answer::Picked(vec!["Tests".into(), "Lint".into()])]);
        assert_eq!(
            said,
            "User has answered your questions: \"Which database?\"=\"SQLite\", \"Which checks?\"=\"Tests, Lint\". You can now continue with the user's answers in mind."
        );

        let asked = codex_asks();
        let said = told(&asked, &questions(&asked).unwrap(), &[Answer::Picked(vec!["Postgres".into()]), Answer::Wrote("rein-x".into())]);
        let read: Value = serde_json::from_str(&said).unwrap();
        assert_eq!(read, json!({ "answers": { "db": { "answers": ["Postgres"] }, "name": { "answers": ["user_note: rein-x"] } } }));
    }

    #[test]
    fn the_answers_are_shown_as_they_went() {
        let asked = claude_asks();
        let said = told(&asked, &questions(&asked).unwrap(), &[Answer::Picked(vec!["SQLite".into()]), Answer::Picked(vec!["Tests".into(), "Lint".into()])]);
        assert_eq!(shown(&said), ["Which database? → SQLite", "Which checks? → Tests, Lint"]);
        let asked = codex_asks();
        let said = told(&asked, &questions(&asked).unwrap(), &[Answer::Picked(vec!["Postgres".into()]), Answer::Skipped]);
        assert_eq!(shown(&said), ["db → Postgres"]);
        assert!(shown(UNANSWERED).is_empty());
    }

    #[test]
    fn what_wasnt_answered_is_left_to_the_models_judgment() {
        let asked = claude_asks();
        let questions = questions(&asked).unwrap();
        assert_eq!(told(&asked, &questions, &[Answer::Skipped, Answer::Skipped]), UNANSWERED);
        let said = told(&asked, &questions, &[Answer::Wrote("MySQL".into()), Answer::Skipped]);
        assert!(
            said.contains("\"Which database?\"=\"MySQL\"")
                && !said.contains("Which checks?")
                && said.ends_with("1 not answered: go on with your best judgment for those, and say what you assumed."),
            "{said}"
        );
    }

    #[test]
    fn a_question_without_real_choices_is_sent_back() {
        let one = call(ToolName::Ask, json!({ "questions": [{ "question": "Go?", "header": "Go", "options": [{ "label": "Yes", "description": "" }], "multiSelect": false }] }));
        assert!(questions(&one).unwrap_err().contains("needs 2 to 4 choices"));
        let none = call(ToolName::Ask, json!({ "questions": [] }));
        assert!(questions(&none).unwrap_err().contains("1 to 4 questions"));
        let codex_shape_to_claude = call(ToolName::Ask, json!({ "questions": [{ "id": "x", "question": "Go?", "header": "Go", "options": [] }] }));
        assert!(questions(&codex_shape_to_claude).is_err());
    }
}
