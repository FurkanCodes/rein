//! Keys and tokens rein knows, kept out of what its tools say.
//!
//! A command that runs outside the sandbox gets your keys, and one that
//! prints them (`env`, a build script's log, `cat .env`) would put them in
//! the conversation, which goes to the model and into the session file.
//! So in whatever a tool says, each key rein knows is replaced before
//! anyone sees it. Only a key written out as it is gets found: an encoded
//! one (base64, say) gets through. The sandbox is the wall; this is a
//! second one behind it.

use std::sync::Arc;

/// What a hidden key is replaced with. Written into a file, it would take
/// the place of your key there, so `write` and `edit` refuse text with it.
pub const HIDDEN: &str = "[a key rein hides]";

/// Shorter values could turn up by chance, and hiding them would garble
/// what tools say: a key or a token is longer.
const SHORTEST: usize = 8;

/// The values to hide, longest first, so a key that holds another is
/// hidden whole. Cloned cheaply: every clone shares one list.
#[derive(Clone, Default)]
pub struct Secrets(Arc<[String]>);

impl Secrets {
    pub fn new(values: impl IntoIterator<Item = String>) -> Self {
        let mut values: Vec<String> = values.into_iter().map(|value| value.trim().to_owned()).filter(|value| value.chars().count() >= SHORTEST).collect();
        values.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        values.dedup();
        Secrets(values.into())
    }

    /// `text` with every key in it replaced by [`HIDDEN`].
    pub fn hide(&self, text: String) -> String {
        if !self.0.iter().any(|value| text.contains(value.as_str())) {
            return text;
        }
        self.0.iter().fold(text, |text, value| text.replace(value.as_str(), HIDDEN))
    }
}

/// Never the values: that's what they're kept from.
impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secrets({} hidden)", self.0.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_each_key_whole_and_leaves_the_rest() {
        let secrets = Secrets::new(["sk-test-FAKE-1".to_owned(), "sk-test-FAKE-1-longer".to_owned(), " gh-FAKE-token\n".to_owned(), "short".to_owned()]);
        let said = secrets.hide("a=sk-test-FAKE-1-longer b=sk-test-FAKE-1 c=gh-FAKE-token short".to_owned());
        assert_eq!(said, format!("a={HIDDEN} b={HIDDEN} c={HIDDEN} short"), "too short to be a key: left");
        assert_eq!(secrets.hide("nothing here".to_owned()), "nothing here");
        assert_eq!(format!("{secrets:?}"), "Secrets(3 hidden)");
    }
}
