//! The command line: rein's help, and what its flags and settings say.

use std::path::PathBuf;
use rein_agent::permissions::Mode;
use rein_llm::Effort;
use crate::settings::Settings;
use crate::{CliResult};

pub(crate) const HELP: &str = "\
rein: a tiny, fast agent harness. You hold the reins.

Usage:
  rein [options]                 Start a conversation in this folder
  rein [options] <prompt...>     Do one task and exit
  rein -c [<prompt...>]          Continue the latest conversation in this folder
  rein -r [<prompt...>]          Choose a saved conversation to continue
  rein login                     Continue with ChatGPT (use your ChatGPT plan)
  rein login <provider> [--key]  Save an API key for a provider, like anthropic;
                                 openrouter signs in in the browser (--key: paste one)
  rein logout <provider>         Forget it (rein logout chatgpt signs out)
  rein providers                 List the providers rein can reach, and what pays
  rein models [<provider>]       List the models you can use

rein can read, write and edit files in this folder and run commands. In a
conversation, type /exit or press Ctrl-D to leave. Conversations are saved in
~/.config/rein/sessions, and what you send from each folder, for ↑ and
Ctrl-R, in ~/.config/rein/history, both readable only by you.

rein rings the terminal's bell when it needs you and you're away: it asks
something, or it's done. Away means its window isn't in front, or, where the
terminal doesn't say, that you haven't touched a key for 10 seconds.

A long conversation is compacted when it nears $REIN_COMPACT_AT tokens
(default 150000): stale tool output goes first, then the model summarizes.
Type /compact [focus] to do it now. Nothing is lost: the model can search
the whole conversation with its recall tool.

rein tells the model about the project with its AGENTS.md files, from the
repository's root down to this folder (CLAUDE.md where a folder has no
AGENTS.md), plus your own ~/.config/rein/AGENTS.md. 32 KB at most.

Options:
  -m, --model <id>      Model to use, like claude-opus-5-5 or anthropic/claude-sonnet-5-5
                        (default: $REIN_MODEL, or gpt-6.1-sol; claude-opus-5-5 when
                        Anthropic is the only provider you have a key for)
      --effort <level>  How hard the model thinks: minimal, low, medium, high,
                        xhigh or max (where the model has them)
  -c, --continue        Continue the latest conversation in this folder
  -r, --resume          Choose which saved conversation to continue
  -s, --system <text>   Standing instructions, added to rein's system prompt
      --mode <mode>     What runs without asking:
                          ask        nothing; ask about every change (default)
                          edits      file changes in this folder
                          read-only  nothing runs or changes; only reading
                          trust      everything, except what deny rules block
      --record <file>   Also save the raw response stream, for tests

Rules in .rein/permissions refine the mode for commands, one per line:
  allow cargo test
  ask   git commit
  deny  git push
Deny beats ask beats allow, and a command only runs without asking if every
part of it is allowed. Rules decide when rein asks; they aren't a sandbox.

Defaults go in ~/.config/rein/config, one per line: model, effort, mode,
compact_at, bell and compress (on or off), like `mode = edits`. Flags win
over $REIN_MODEL, $REIN_EFFORT, $REIN_MODE, $REIN_COMPACT_AT, $REIN_BELL and
$REIN_COMPRESS, which win over the file.

Each request carries the whole conversation, so rein gzips it on the way:
about three times fewer bytes, and the same text for the model. A server that
doesn't take compressed requests gets them uncompressed instead.

A model's name says who serves it (claude-* is Anthropic's, gpt-* OpenAI's,
gemini-* Google's), or name the provider first: openrouter/qwen/qwen3-coder,
ollama/qwen3:8b. `rein providers` lists the providers and what pays for each;
add your own in ~/.config/rein/providers. Keys come from each provider's
variable, like $ANTHROPIC_API_KEY, or from `rein login <provider>`. Signed in
with ChatGPT, OpenAI's models use your plan.";

/// Which conversation to work in.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resume {
    New,
    /// The most recently used one in this folder.
    Latest,
    /// One the user picks from a list.
    Pick,
}

pub(crate) struct AskArgs {
    /// From `-m`, the environment or the settings; `None` lets rein choose.
    pub(crate) model: Option<String>,
    /// `-m` was given, so a continued conversation switches to `model`.
    pub(crate) model_given: bool,
    pub(crate) resume: Resume,
    pub(crate) effort: Option<Effort>,
    /// The system prompt: standing instructions sent with every request.
    pub(crate) system: Option<String>,
    pub(crate) mode: Mode,
    pub(crate) record: Option<PathBuf>,
    /// Ring the bell when rein needs you and you're away.
    pub(crate) bell: bool,
    /// Gzip request bodies.
    pub(crate) compress: bool,
    /// `None` starts a conversation instead of doing one task.
    pub(crate) prompt: Option<String>,
}

impl AskArgs {
    /// Flags first; what they don't say comes from `settings` (and the
    /// environment), then rein's defaults.
    pub(crate) fn parse(args: Vec<String>, settings: &Settings) -> CliResult<Self> {
        let mut model = settings.get("model", "REIN_MODEL");
        let effort = settings.get("effort", "REIN_EFFORT").map(|e| e.parse::<Effort>().map_err(|e| format!("{e} (in the settings)"))).transpose()?;
        let (mut effort, mut system, mut record, mut words) = (effort, None, None, Vec::new());
        let mut mode = match settings.get("mode", "REIN_MODE") {
            Some(name) => name.parse::<Mode>().map_err(|e| format!("{e} (in the settings)"))?,
            None => Mode::Ask,
        };
        let switch = |key: &str, env: &str| match settings.get(key, env).as_deref() {
            None | Some("on") => Ok(true),
            Some("off") => Ok(false),
            Some(other) => Err(format!("{key} should be on or off, not {other}")),
        };
        let (bell, compress) = (switch("bell", "REIN_BELL")?, switch("compress", "REIN_COMPRESS")?);
        let (mut model_given, mut resume) = (false, Resume::New);
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-m" | "--model" => {
                    model = Some(value(&mut args, &arg)?);
                    model_given = true;
                }
                "-c" | "--continue" => resume = Resume::Latest,
                "-r" | "--resume" => resume = Resume::Pick,
                "--effort" => effort = Some(value(&mut args, &arg)?.parse()?),
                "--mode" => {
                    let name = value(&mut args, &arg)?;
                    mode = name.parse()?;
                }
                "-s" | "--system" => system = Some(value(&mut args, &arg)?),
                "--record" => record = Some(PathBuf::from(value(&mut args, &arg)?)),
                "--" => words.extend(args.by_ref()),
                flag if flag.starts_with('-') && flag.len() > 1 => {
                    return Err(format!("unknown option {flag}; see `rein --help`"));
                }
                _ => words.push(arg),
            }
        }
        let prompt = if words.is_empty() { None } else { Some(words.join(" ")) };
        Ok(AskArgs { model, model_given, resume, effort, system, mode, record, bell, compress, prompt })
    }
}

pub(crate) fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> CliResult<String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> CliResult<AskArgs> {
        AskArgs::parse(line.split(' ').map(str::to_owned).collect(), &Settings::default())
    }

    #[test]
    fn reads_modes_and_the_task() {
        let parsed = args("--mode edits fix the bug").unwrap();
        assert_eq!((parsed.mode, parsed.prompt.as_deref()), (Mode::Edits, Some("fix the bug")));
        assert_eq!(args("--mode edits").unwrap().prompt, None, "no task: a conversation");
        assert_eq!(args("--mode yolo hi").err().as_deref(), Some("no mode called yolo; use ask, edits, read-only or trust"));
    }

    #[test]
    fn flags_win_over_the_settings() {
        let settings = Settings::parse("mode = edits\nmodel = from-file\neffort = low").unwrap();
        let parsed = AskArgs::parse(vec!["hi".into()], &settings).unwrap();
        assert_eq!((parsed.mode, parsed.effort), (Mode::Edits, Some(Effort::Low)));
        assert_eq!(args("--effort loud hi").err().as_deref(), Some("no effort called loud; use minimal, low, medium, high, xhigh or max"));
        let parsed = AskArgs::parse(["--mode", "trust", "-m", "flagged", "hi"].map(str::to_owned).to_vec(), &settings).unwrap();
        assert_eq!((parsed.mode, parsed.model.as_deref()), (Mode::Trust, Some("flagged")));
    }

    #[test]
    fn parses_a_system_prompt() {
        let parsed = args("-s Be-brief what is a harness").unwrap();
        assert_eq!(parsed.system.as_deref(), Some("Be-brief"));
        assert_eq!(parsed.prompt.as_deref(), Some("what is a harness"));

        assert_eq!(args("what is a harness").unwrap().system, None);
        assert_eq!(args("--system").err().as_deref(), Some("--system needs a value"));
    }
}
