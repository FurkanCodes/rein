//! What the agent says can be set, and how things stand: ACP's config
//! options, and what a window shows that ACP has no word for.

use serde_json::Value;

/// One of the things a setting can be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choosable {
    pub value: String,
    pub name: String,
    /// What's said of it, in a few words.
    pub about: String,
    /// Whose it is, where choices come in groups.
    pub group: Option<String>,
}

/// Something that can be set: the model, the effort, the mode, the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub id: String,
    pub current: String,
    pub choices: Vec<Choosable>,
    /// What an effort comes to: `default` is a choice, not a level.
    pub level: Option<String>,
    /// What a switch for a command runs: the check's `cargo check`.
    pub command: Option<String>,
}

/// Marks the level an effort comes to.
const LEVEL: &str = "io.github.furkancodes/level";
/// Marks what a switch for a command runs.
const COMMAND: &str = "io.github.furkancodes/command";

impl Setting {
    /// Read from `configOptions`, which group their choices or not. One with
    /// no id or value says nothing.
    pub fn listed(options: &Value) -> Vec<Setting> {
        options.as_array().map(Vec::as_slice).unwrap_or_default().iter().filter_map(Setting::of).collect()
    }

    fn of(option: &Value) -> Option<Setting> {
        let mut choices = Vec::new();
        for one in option["options"].as_array()? {
            match one["options"].as_array() {
                Some(grouped) => choices.extend(grouped.iter().filter_map(choice).map(|choice| Choosable { group: one["name"].as_str().map(str::to_owned), ..choice })),
                None => choices.extend(choice(one)),
            }
        }
        Some(Setting {
            id: option["id"].as_str()?.to_owned(),
            current: option["currentValue"].as_str()?.to_owned(),
            choices,
            level: option["_meta"][LEVEL].as_str().map(str::to_owned),
            command: option["_meta"][COMMAND].as_str().map(str::to_owned),
        })
    }

    /// What it's set to, as it's called.
    pub fn current_name(&self) -> &str {
        self.choices.iter().find(|choice| choice.value == self.current).map_or(self.current.as_str(), |choice| choice.name.as_str())
    }

    /// How far up the levels an effort is, and how many there are. A
    /// `default` choice isn't a level.
    pub fn pips(&self) -> Option<(usize, usize)> {
        let levels: Vec<&str> = self.choices.iter().map(|choice| choice.value.as_str()).filter(|value| *value != "default").collect();
        let level = self.level.as_deref().unwrap_or(&self.current);
        let at = levels.iter().position(|one| *one == level)?;
        Some((at + 1, levels.len()))
    }
}

fn choice(one: &Value) -> Option<Choosable> {
    Some(Choosable {
        value: one["value"].as_str()?.to_owned(),
        name: one["name"].as_str()?.to_owned(),
        about: one["description"].as_str().unwrap_or_default().to_owned(),
        group: None,
    })
}

/// How much of a limit is used, as the provider says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meter {
    pub label: String,
    /// In percent, where there's a limit.
    pub used: Option<u8>,
    pub said: String,
    pub resets: Option<String>,
    pub warn: bool,
}

impl Meter {
    pub fn listed(params: &Value) -> Vec<Meter> {
        let listed = params["meters"].as_array().map(Vec::as_slice).unwrap_or_default().iter();
        listed
            .filter_map(|one| {
                Some(Meter {
                    label: one["label"].as_str()?.to_owned(),
                    used: one["used"].as_u64().and_then(|used| u8::try_from(used).ok()),
                    said: one["said"].as_str().unwrap_or_default().to_owned(),
                    resets: one["resets"].as_str().map(str::to_owned),
                    warn: one["warn"] == true,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn effort(current: &str, level: Option<&str>) -> Setting {
        let option = json!({ "id": "effort", "currentValue": current, "options": [{ "value": "default", "name": "default" }, { "value": "low", "name": "low" }, { "value": "high", "name": "high" }, { "value": "max", "name": "max" }], "_meta": { LEVEL: level } });
        Setting::of(&option).unwrap()
    }

    #[test]
    fn choices_come_grouped_or_not() {
        let options = json!([{ "id": "model", "currentValue": "openai/gpt-5", "options": [{ "group": "openai", "name": "OpenAI", "options": [{ "value": "openai/gpt-5", "name": "gpt-5" }] }, { "group": "groq", "name": "Groq", "options": [{ "value": "groq/llama", "name": "groq/llama" }] }] }]);
        let model = &Setting::listed(&options)[0];
        assert_eq!(model.choices.len(), 2, "every provider's");
        assert_eq!(model.choices[1].group.as_deref(), Some("Groq"), "and each knows whose it is");
        assert_eq!(model.current_name(), "gpt-5");
    }

    #[test]
    fn a_value_with_no_name_goes_by_itself() {
        let option = Setting::of(&json!({ "id": "model", "currentValue": "gone", "options": [] })).unwrap();
        assert_eq!(option.current_name(), "gone");
    }

    #[test]
    fn an_effort_shows_how_far_up_it_is() {
        assert_eq!(effort("high", None).pips(), Some((2, 3)));
        assert_eq!(effort("default", Some("low")).pips(), Some((1, 3)), "the default comes to a level of its own");
        assert_eq!(effort("default", None).pips(), None, "and where nobody says which, there's nothing to draw");
    }

    #[test]
    fn a_limit_nobody_set_isnt_a_share() {
        let meters = Meter::listed(&json!({ "meters": [{ "label": "5h", "used": 28, "said": "28%" }, { "label": "$3.20 spent", "said": "" }, { "said": "no label" }] }));
        assert_eq!(meters.len(), 2);
        assert_eq!((meters[0].used, meters[1].used), (Some(28), None));
    }
}
