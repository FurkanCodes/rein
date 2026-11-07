//! How an agent stands: what you'd want to know of each at a glance, and
//! which of them needs you. Kept here, with no toolkit in sight, so a window
//! only has to say what happened.

/// What's shown for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    Idle,
    Working,
    /// It's waiting for your answer.
    Asking,
    /// A turn ended somewhere you weren't looking.
    Unread,
    /// Its last turn failed.
    Failed,
    /// It has gone, and nothing more will come of it.
    Ended,
}

impl Standing {
    /// In words, so that a colour is never all that says it.
    pub const fn word(self) -> &'static str {
        match self {
            Standing::Idle => "idle",
            Standing::Working => "working",
            Standing::Asking => "needs you",
            Standing::Unread => "done",
            Standing::Failed => "failed",
            Standing::Ended => "ended",
        }
    }
}

/// How the last turn stands. A question goes on top of this, not instead of
/// it: answering one puts the agent back to work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Turn {
    #[default]
    Idle,
    Working,
    Unread,
    Failed,
    Ended,
}

/// What's happened to one agent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Watch {
    turn: Turn,
    /// Questions waiting for an answer.
    asking: usize,
}

impl Watch {
    pub const fn standing(&self) -> Standing {
        match (self.turn, self.asking) {
            (Turn::Ended, _) => Standing::Ended,
            (_, 1..) => Standing::Asking,
            (Turn::Idle, _) => Standing::Idle,
            (Turn::Working, _) => Standing::Working,
            (Turn::Unread, _) => Standing::Unread,
            (Turn::Failed, _) => Standing::Failed,
        }
    }

    /// A turn started, whoever started it.
    pub fn started(&mut self) {
        if self.turn != Turn::Ended {
            self.turn = Turn::Working;
        }
    }

    /// A turn came to its end, with you `looking` at it or not. Whatever it
    /// was asking went with it.
    pub fn turn_over(&mut self, looking: bool) {
        self.asking = 0;
        if self.turn == Turn::Working {
            self.turn = if looking { Turn::Idle } else { Turn::Unread };
        }
    }

    pub fn failed(&mut self) {
        self.asking = 0;
        if self.turn != Turn::Ended {
            self.turn = Turn::Failed;
        }
    }

    pub fn gone(&mut self) {
        self.asking = 0;
        self.turn = Turn::Ended;
    }

    pub fn asked(&mut self) {
        self.asking = self.asking.saturating_add(1);
    }

    pub fn answered(&mut self) {
        self.asking = self.asking.saturating_sub(1);
    }

    /// You've looked. What failed stays: it's still the last thing that
    /// happened, and looking doesn't change that.
    pub fn seen(&mut self) {
        if self.turn == Turn::Unread {
            self.turn = Turn::Idle;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_works_then_ends_read_or_unread() {
        let mut watch = Watch::default();
        assert_eq!(watch.standing(), Standing::Idle);
        watch.started();
        assert_eq!(watch.standing(), Standing::Working);
        watch.turn_over(false);
        assert_eq!(watch.standing(), Standing::Unread, "you weren't looking");
        watch.seen();
        assert_eq!(watch.standing(), Standing::Idle);
        watch.started();
        watch.turn_over(true);
        assert_eq!(watch.standing(), Standing::Idle, "you saw it end");
    }

    #[test]
    fn a_question_needs_you_until_each_is_answered_and_then_it_works_on() {
        let mut watch = Watch::default();
        watch.started();
        watch.asked();
        watch.asked();
        assert_eq!(watch.standing(), Standing::Asking);
        watch.answered();
        assert_eq!(watch.standing(), Standing::Asking, "one's still waiting");
        watch.answered();
        assert_eq!(watch.standing(), Standing::Working);
        watch.answered();
        assert_eq!(watch.standing(), Standing::Working, "an extra answer isn't a question");
        watch.asked();
        watch.answered();
        assert_eq!(watch.standing(), Standing::Working, "and the next one still takes one answer");
    }

    #[test]
    fn a_stopped_turn_takes_its_questions_with_it() {
        let mut watch = Watch::default();
        watch.started();
        watch.asked();
        watch.turn_over(true);
        assert_eq!(watch.standing(), Standing::Idle);
        watch.started();
        watch.asked();
        watch.failed();
        assert_eq!(watch.standing(), Standing::Failed);
    }

    #[test]
    fn a_failure_stays_until_the_next_turn_and_looking_doesnt_clear_it() {
        let mut watch = Watch::default();
        watch.started();
        watch.failed();
        watch.seen();
        assert_eq!(watch.standing(), Standing::Failed);
        watch.started();
        assert_eq!(watch.standing(), Standing::Working);
    }

    #[test]
    fn an_agent_thats_gone_stays_gone() {
        let mut watch = Watch::default();
        watch.started();
        watch.asked();
        watch.gone();
        assert_eq!(watch.standing(), Standing::Ended, "nothing can answer it now");
        watch.started();
        watch.failed();
        watch.asked();
        assert_eq!(watch.standing(), Standing::Ended, "what's said late changes nothing");
    }

    #[test]
    fn each_standing_says_itself_in_words() {
        for standing in [Standing::Idle, Standing::Working, Standing::Asking, Standing::Unread, Standing::Failed, Standing::Ended] {
            assert!(!standing.word().is_empty());
        }
        assert_eq!(Standing::Asking.word(), "needs you");
    }
}
