//! What the list under the box offers for a `/` word: rein's commands and
//! the skills you may call, while the first word is typed; the skills
//! alone for a `/` word later in the message, which loads that skill with
//! it.

use crate::commands;

/// A skill you may call, as the list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub name: String,
    /// What it takes, as it says (`argument-hint`).
    pub hint: String,
    /// What it's for, and whose when it's not yours.
    pub about: String,
    /// rein has a command of its name, so as the first word it's called
    /// `/skill:name`.
    pub clash: bool,
}

/// A line of the list: what goes in the box, what it takes, what it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub args: String,
    pub what: String,
    /// It's a skill's: what you type after it is what it's given.
    pub skill: bool,
}

/// The commands and skills that match `typed`, the first word, while it's
/// being typed: rein's commands first.
pub fn matching(typed: &str, offers: &[Offer]) -> Vec<Entry> {
    if !typed.starts_with('/') || typed.contains(char::is_whitespace) {
        return Vec::new();
    }
    let commands = commands::matching(typed).into_iter().map(|(name, args, what)| Entry { name: name.into(), args: args.into(), what: what.into(), skill: false });
    let wanted = typed.trim_start_matches('/');
    let wanted = wanted.strip_prefix("skill:").unwrap_or(wanted);
    let skills = skills(wanted, offers).into_iter().map(|offer| entry(offer, if offer.clash { "/skill:" } else { "/" }));
    commands.chain(skills).collect()
}

/// The skills that match a `/` word after the first, `typed` what's after
/// its `/`.
pub fn later(typed: &str, offers: &[Offer]) -> Vec<Entry> {
    skills(typed, offers).into_iter().map(|offer| entry(offer, "/")).collect()
}

fn entry(offer: &Offer, mark: &str) -> Entry {
    Entry { name: format!("{mark}{}", offer.name), args: offer.hint.clone(), what: offer.about.clone(), skill: true }
}

/// The skills whose names start with `typed`, then those a part of whose
/// name after a `:` does (`/senior` finds `engineering:senior-backend`),
/// then, for two letters or more, those that have it anywhere.
fn skills<'a>(typed: &str, offers: &'a [Offer]) -> Vec<&'a Offer> {
    let typed = typed.to_lowercase();
    let rank = |offer: &Offer| {
        let name = offer.name.to_lowercase();
        if name.starts_with(&typed) {
            Some(0)
        } else if name.split(':').skip(1).any(|part| part.starts_with(&typed)) {
            Some(1)
        } else if typed.chars().count() >= 2 && name.contains(&typed) {
            Some(2)
        } else {
            None
        }
    };
    let mut found: Vec<(usize, &Offer)> = offers.iter().filter_map(|offer| rank(offer).map(|rank| (rank, offer))).collect();
    found.sort_by_key(|(rank, _)| *rank);
    found.into_iter().map(|(_, offer)| offer).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(name: &str, clash: bool) -> Offer {
        Offer { name: name.into(), hint: String::new(), about: format!("about {name}"), clash }
    }

    #[test]
    fn the_first_word_lists_commands_then_skills_and_a_later_one_skills() {
        let offers = [offer("graphify", false), offer("search", true), offer("engineering:senior-backend", false), offer("migrate", false)];
        let names = |entries: Vec<Entry>| entries.into_iter().map(|entry| entry.name).collect::<Vec<_>>();
        assert_eq!(names(matching("/graph", &offers)), ["/graphify"]);
        assert_eq!(names(matching("/gra", &offers)), ["/graphify", "/migrate"], "a start first, then anywhere in it");
        assert_eq!(names(matching("/se", &offers)), ["/search", "/send", "/skill:search", "/engineering:senior-backend"], "rein's own first; the skill by its full name");
        assert_eq!(names(matching("/skill:se", &offers)), ["/skill:search", "/engineering:senior-backend"]);
        assert_eq!(names(matching("/grate", &offers)), ["/migrate"], "anywhere in it, for two letters or more");
        assert!(matching("/graphify now", &offers).is_empty(), "the first word only");
        assert_eq!(names(later("sea", &offers)), ["/search"], "later on, no command has the name");
        assert_eq!(names(later("", &offers)).len(), 4);
    }
}
