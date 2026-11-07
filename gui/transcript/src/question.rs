//! What a step would do, put to you as a question, and the answers to it.

use serde_json::Value;

/// The line an edit starts on, which ACP's `diff` has no place for.
const LINE: &str = "io.github.furkancodes/line";
/// Marks the answer that says no and hands the conversation back to you.
const INSTEAD: &str = "io.github.furkancodes/instead";
/// What rein asks, in its own words.
const ASKS: &str = "io.github.furkancodes/question";
/// Why it's asked.
const WHY: &str = "io.github.furkancodes/why";

/// What a step would do, put to you as a question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub title: String,
    /// All of what it would run or write, where the title shows less: a
    /// title has one line of a command, and a yes goes to the whole of it.
    pub detail: Option<String>,
    /// What it would change in a file, where that's what it does.
    pub change: Option<FileChange>,
    /// A sub-agent's task, whole, where it's all the step says: its role
    /// is in the title. `None` if it says more, which `detail` holds.
    pub task: Option<String>,
}

impl Question {
    /// Read from a `tool_call`. One with no title doesn't say what it is.
    pub(crate) fn of(update: &Value) -> Option<Question> {
        let title = update["title"].as_str()?.to_owned();
        let input = &update["rawInput"];
        let change = FileChange::of(update);
        // Only a command step is all in its command: any other tool's
        // arguments are what you'd be saying yes to, each of them.
        let command = if update["kind"] == "execute" { input["command"].as_str() } else { None };
        let whole = (!input.is_null()).then(|| command.or(input.as_str()).map_or_else(|| pretty(input), str::to_owned));
        let detail = whole.filter(|whole| change.is_none() && !title.contains(whole.as_str()));
        let only_task = input.as_object().is_some_and(|fields| fields.keys().all(|key| key == "role" || key == "task"));
        let task = input["task"].as_str().filter(|_| only_task).map(str::to_owned);
        Some(Question { title, detail, change, task })
    }
}

/// What a step would change in a file: the lines that differ, all of them.
/// Models repeat a few around a change to make it unique, and only what
/// changes is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub lines: Vec<ChangedLine>,
    /// It goes for every place the old text is, not only the first.
    pub everywhere: bool,
}

/// A line a step would take out or put in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedLine {
    pub added: bool,
    /// Where it is in the file, if the agent can tell.
    pub number: Option<usize>,
    pub text: String,
}

impl FileChange {
    /// Read from ACP's `diff`.
    fn of(update: &Value) -> Option<FileChange> {
        let diff = update["content"].as_array()?.iter().find(|block| block["type"] == "diff")?;
        let new: Vec<&str> = diff["newText"].as_str()?.lines().collect();
        let old: Vec<&str> = diff["oldText"].as_str().unwrap_or_default().lines().collect();
        let same_start = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
        let same_end = old[same_start..].iter().rev().zip(new[same_start..].iter().rev()).take_while(|(a, b)| a == b).count();
        let first = diff["_meta"][LINE].as_u64().and_then(|line| usize::try_from(line).ok()).map(|line| line.saturating_add(same_start));
        let side = |lines: &[&str], added| -> Vec<ChangedLine> {
            lines.iter().enumerate().map(|(at, text)| ChangedLine { added, number: first.map(|first| first.saturating_add(at)), text: (*text).to_owned() }).collect()
        };
        let mut lines = side(&old[same_start..old.len() - same_end], false);
        lines.extend(side(&new[same_start..new.len() - same_end], true));
        Some(FileChange { lines, everywhere: update["rawInput"]["replace_all"] == true })
    }
}

/// How an agent puts a question, past the step it's about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Words {
    /// What it asks, if it says.
    pub asks: Option<String>,
    /// Why it's asking.
    pub why: Option<String>,
}

impl Words {
    /// Read from a `session/request_permission`.
    pub fn of(params: &Value) -> Words {
        let text = |key: &str| params["_meta"][key].as_str().map(str::to_owned);
        Words { asks: text(ASKS), why: text(WHY) }
    }
}

/// An answer an agent offers to a question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// What's sent back if it's picked.
    pub id: String,
    pub name: String,
    pub kind: Answering,
}

/// What a choice says to a question: ACP's four, and rein's own no.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answering {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    /// No, and the turn stops so you can say what to do instead. It goes as
    /// a `reject_once`, which a client that doesn't know it reads as a no.
    Instead,
}

impl Answering {
    fn of(option: &Value) -> Option<Answering> {
        match option["kind"].as_str()? {
            "allow_once" => Some(Answering::AllowOnce),
            "allow_always" => Some(Answering::AllowAlways),
            "reject_once" if option["_meta"][INSTEAD] == true => Some(Answering::Instead),
            "reject_once" => Some(Answering::RejectOnce),
            "reject_always" => Some(Answering::RejectAlways),
            _ => None,
        }
    }
}

impl Choice {
    pub const fn allows(&self) -> bool {
        matches!(self.kind, Answering::AllowOnce | Answering::AllowAlways)
    }

    /// The answers to give to a question whose `options` the agent listed.
    /// A step nobody can describe can't be allowed, however it's offered, a
    /// way of answering nobody knows isn't offered, whatever it would do, and
    /// an agent that offers no way out still gets a no.
    pub fn offered(options: &Value, known: bool) -> Vec<Choice> {
        let listed = options
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|option| Some(Choice { id: option["optionId"].as_str()?.to_owned(), name: option["name"].as_str()?.to_owned(), kind: Answering::of(option)? }));
        let mut choices: Vec<Choice> = listed.filter(|choice| known || !choice.allows()).collect();
        if !choices.iter().any(|choice| !choice.allows()) {
            choices.push(Choice { id: "reject".to_owned(), name: "Reject".to_owned(), kind: Answering::RejectOnce });
        }
        choices
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn step(content: Value, input: Value) -> Value {
        json!({ "sessionUpdate": "tool_call", "toolCallId": "a", "title": "Edit a.rs", "kind": "edit", "status": "pending", "content": content, "rawInput": input })
    }

    fn line(added: bool, number: Option<usize>, text: &str) -> ChangedLine {
        ChangedLine { added, number, text: text.to_owned() }
    }

    #[test]
    fn a_file_change_shows_the_lines_that_differ_where_they_are() {
        let diff = json!([{ "type": "diff", "path": "/p/a.rs", "oldText": "same\nold\nend\n", "newText": "same\nnew\nnewer\nend\n", "_meta": { LINE: 10 } }]);
        let question = Question::of(&step(diff, json!({ "path": "a.rs" }))).unwrap();
        let change = question.change.unwrap();
        assert_eq!(change.lines, [line(false, Some(11), "old"), line(true, Some(11), "new"), line(true, Some(12), "newer")]);
        assert!(!change.everywhere);
        assert_eq!(question.detail, None, "the diff says it, not its arguments as JSON");
    }

    #[test]
    fn a_file_written_whole_is_all_new_and_a_change_may_not_know_its_place() {
        let written = json!([{ "type": "diff", "path": "/p/a.rs", "oldText": null, "newText": "one\ntwo\n" }]);
        let change = FileChange::of(&step(written, Value::Null)).unwrap();
        assert_eq!(change.lines, [line(true, None, "one"), line(true, None, "two")]);
    }

    #[test]
    fn a_change_to_every_place_says_so_and_nothing_is_a_change_of_nothing() {
        let diff = json!([{ "type": "diff", "path": "/p/a.rs", "oldText": "a", "newText": "b" }]);
        assert!(FileChange::of(&step(diff, json!({ "replace_all": true }))).unwrap().everywhere);
        assert_eq!(FileChange::of(&step(json!([]), Value::Null)), None);
        let same = json!([{ "type": "diff", "path": "/p/a.rs", "oldText": "a\na\n", "newText": "a\n" }]);
        assert_eq!(FileChange::of(&step(same, Value::Null)).unwrap().lines, [line(false, None, "a")], "lines alike at both ends count once");
    }

    #[test]
    fn a_step_with_no_diff_is_all_in_its_arguments() {
        let question = Question::of(&step(json!([]), json!({ "name": "x" }))).unwrap();
        assert_eq!(question.change, None);
        assert!(question.detail.is_some_and(|detail| detail.contains("\"name\": \"x\"")));
    }

    #[test]
    fn rein_s_own_no_is_only_ever_a_no() {
        let options = json!([
            { "optionId": "a", "name": "Yes", "kind": "allow_once", "_meta": { INSTEAD: true } },
            { "optionId": "b", "name": "No, and tell rein what to do instead", "kind": "reject_once", "_meta": { INSTEAD: true } },
        ]);
        let kinds: Vec<Answering> = Choice::offered(&options, true).iter().map(|choice| choice.kind).collect();
        assert_eq!(kinds, [Answering::AllowOnce, Answering::Instead], "the mark doesn't turn a yes into anything");
        assert!(!Choice::offered(&options, true)[1].allows());
    }

    #[test]
    fn how_a_question_is_put_is_rein_s_own_and_may_not_be_said() {
        let words = Words::of(&json!({ "_meta": { ASKS: "Allow this edit?", WHY: "Why: it must" } }));
        assert_eq!(words, Words { asks: Some("Allow this edit?".to_owned()), why: Some("Why: it must".to_owned()) });
        assert_eq!(Words::of(&json!({})), Words::default());
    }

    #[test]
    fn a_sub_agents_task_is_kept_whole_when_its_all_the_step_says() {
        let asked = |input: Value| Question::of(&serde_json::json!({ "title": "Sub-agent linux: run them", "kind": "other", "rawInput": input })).unwrap();
        let task = "Run the whole suite.\nThen report.";
        assert_eq!(asked(serde_json::json!({ "role": "linux", "task": task })).task.as_deref(), Some(task));
        let more = asked(serde_json::json!({ "role": "linux", "task": task, "model": "opus" }));
        assert_eq!(more.task, None, "what else it says would go unseen");
        assert!(more.detail.is_some_and(|detail| detail.contains("opus")));
    }
}
