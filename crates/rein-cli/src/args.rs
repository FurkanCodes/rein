//! The command line: rein's help, and what its flags and settings say.

use crate::CliResult;
use crate::peers::node::{Joining, Policy};
use crate::settings::Settings;
use rein_agent::agent::DEFAULT_MAX_STEPS;
use rein_agent::permissions::Mode;
use rein_llm::Effort;
use std::path::PathBuf;

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
  rein login claude-code         Use Claude Code CLI login or paste a setup-token token
  rein logout claude-code        Forget rein's Claude Code setup (not the CLI's login)
  rein logout <provider>         Forget it (rein logout chatgpt signs out)
  rein providers                 List the providers rein can reach, and what pays
  rein models [<provider>]       List the models you can use
  rein peers                     Watch the reins running in your other terminals,
                                 and answer what they ask (y/n)

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
      --effort <level>  How hard the model thinks: off, minimal, low, medium, high,
                        xhigh or max. Each model takes some of them, and gets
                        the closest it has without going over
  -c, --continue        Continue the latest conversation in this folder
  -r, --resume          Choose which saved conversation to continue
  -s, --system <text>   Standing instructions, added to rein's system prompt
      --mode <mode>     What runs without asking:
                          trust      everything, but what ask rules name, like
                                     deleting (default)
                          ask        reading, and commands that only look in
                                     the sandbox
                          edits      file changes in this folder, and commands
                                     that stay in the sandbox
                          auto       the same, and nothing ever asks: what would
                                     go past the sandbox is refused (without it,
                                     every command runs)
                          read-only  only reading: commands look, nothing changes
      --sandbox <on|off>  Run commands in the sandbox, in any mode (default off)
      --max-steps <n>   How many steps a turn may take when nobody is
                        watching (auto mode, a one-shot task), or off;
                        default 200. While you watch there's no limit
      --record <file>   Also save the raw response stream, for tests
      --name <name>     What other reins call this one (default: the folder's)
      --workspace <ws>  The group of reins it works with (default: the git branch)
      --worktree        Work in the workspace's own git worktree, beside this
                        repository, made on first use on a branch of its name

Rules refine the mode for commands, one per line: yours, for every project,
in ~/.config/rein/permissions (or /permissions), and a project's own in
.rein/permissions:
  allow cargo test
  ask   git commit
  deny  git push
Deny beats ask beats allow, and a command only runs without asking if every
part of it is allowed. Yours start by asking before deleting (rm, git clean,
git push --force and the like). Rules decide when rein asks; they aren't a
sandbox.

With the sandbox on (--sandbox on, sandbox = on, or /sandbox), commands run
in it in every mode: macOS's own, or on Linux Landlock and seccomp. Each
says what it needs: look, write (files in this folder), network (and the
internet) or outside (no sandbox), and the sandbox holds it to that. Even
with write access a command can't change files outside this folder, read
the keys and logins in your home folder, or change what runs code later
(.git/hooks, .git/config, .rein). With it off, commands run as you'd run
them, and the mode alone decides what asks: in ask and edits, every
command. On but missing here, what it would hold asks first.

A turn goes on until the model is done, or you press Esc. One that keeps
making the same call with the same result is told, then stopped, and a turn
nobody watches stops at max_steps, with the model saying where it got to.

Every conversation joins the other reins on this computer: start rein in
another terminal tab and they can talk. Each tab's title says what its rein
is doing: a turning spinner and its time while it works, ◆ when it needs
you, ✓ and how long once it's done. Across the top of each, a strip shows
them all live, in colour (strip = off hides it). /peers shows them all, /send and /delegate write
to one or hand it a task, /name and /workspace say who this one is, and
/inbox holds what's waiting to be let in. The model can do the same with its
peers, message and delegate tools, and start a sub-agent in a new tab of its
own with spawn: you're asked first, it shows as ↳ in the strip, and its result
comes back. rein opens the tab in tmux, kitty, WezTerm, iTerm2, Ghostty and
Terminal; elsewhere it gives you the command to paste, or set tab_command. Another rein's message is never yours: it
can't approve anything, and everything it leads to asks as usual. Set
Set peers = hold to let each message in yourself (trust mode always does),
refuse to take none, or off to stay out.

Defaults go in ~/.config/rein/config, one per line: model, effort, mode,
compact_at, max_steps, peers, check, and bell, compress, sandbox, memory and strip (on or
off), like `mode = edits`. Flags win over $REIN_MODEL, $REIN_EFFORT, $REIN_MODE,
$REIN_COMPACT_AT, $REIN_MAX_STEPS, $REIN_BELL, $REIN_COMPRESS, $REIN_SANDBOX,
$REIN_MEMORY, $REIN_CHECK, $REIN_PEERS and $REIN_STRIP, which win over the file.

After a step changes files, rein runs the project's check (cargo check, tsc,
go vet, pyright or mypy, found from its files, asked about once) and tells the
model what its changes broke, at that step. /check shows it; check = off stops
it everywhere, and check = <your command> runs yours instead.

The model keeps notes across conversations, per project: corrections and why,
who you are, what the project's files don't say. Each save is a step you see,
a conversation starts with their index, and Claude Code's notes for the
project are read too, never changed. /memory lists them; memory = off stops it.

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
    /// Who searches the web for the model.
    pub(crate) search: crate::search::SearchWith,
    /// The system prompt: standing instructions sent with every request.
    pub(crate) system: Option<String>,
    pub(crate) mode: Mode,
    pub(crate) record: Option<PathBuf>,
    /// Ring the bell when rein needs you and you're away.
    pub(crate) bell: bool,
    /// Gzip request bodies.
    pub(crate) compress: bool,
    /// Run commands in the sandbox, where there is one.
    pub(crate) sandbox: bool,
    /// Keep notes across conversations, and start each with their index.
    pub(crate) memory: bool,
    /// What checks the project after edits: see [`crate::check`].
    pub(crate) check: crate::check::Setting,
    /// How many steps a turn nobody is watching may take; `None`: no limit.
    pub(crate) max_steps: Option<usize>,
    /// `None` starts a conversation instead of doing one task.
    pub(crate) prompt: Option<String>,
    /// What other reins call this one; `None`: the folder's name.
    pub(crate) name: Option<String>,
    /// Its group of reins; `None`: the git branch.
    pub(crate) workspace: Option<String>,
    /// Work in the workspace's own git worktree.
    pub(crate) worktree: bool,
    /// Whether it joins the other reins, and what it lets in.
    pub(crate) joining: Joining,
    /// The strip of all the reins across the top of the screen.
    pub(crate) strip: bool,
    /// This rein is a sub-agent: the rein that started it, and its task's id.
    pub(crate) child: Option<(u32, String)>,
    /// How to open a tab here, where rein doesn't know: a command with
    /// `{command}` in it.
    pub(crate) tab_command: Option<String>,
}

impl AskArgs {
    /// Flags first; what they don't say comes from `settings` (and the
    /// environment), then rein's defaults.
    pub(crate) fn parse(args: Vec<String>, settings: &Settings) -> CliResult<Self> {
        let mut model = settings.get("model", "REIN_MODEL");
        let effort = settings.get("effort", "REIN_EFFORT").map(|e| e.parse::<Effort>().map_err(|e| format!("{e} (in the settings)"))).transpose()?;
        let (mut effort, mut system, mut record, mut words) = (effort, None, None, Vec::new());
        let search = settings.get("search", "REIN_SEARCH").map(|with| with.parse().map_err(|e| format!("{e} (in the settings)"))).transpose()?.unwrap_or_default();
        let mut mode = match settings.get("mode", "REIN_MODE") {
            Some(name) => name.parse::<Mode>().map_err(|e| format!("{e} (in the settings)"))?,
            None => Mode::default(),
        };
        let switch = |key: &str, env: &str, default: bool| match settings.get(key, env).as_deref() {
            None => Ok(default),
            Some(value) => on_off(key, value),
        };
        let (bell, compress) = (switch("bell", "REIN_BELL", true)?, switch("compress", "REIN_COMPRESS", true)?);
        // Off unless you turn it on, as in Claude Code.
        let mut sandbox = switch("sandbox", "REIN_SANDBOX", false)?;
        let memory = switch("memory", "REIN_MEMORY", true)?;
        let check = crate::check::Setting::of(settings.get("check", "REIN_CHECK").as_deref());
        let strip = switch("strip", "REIN_STRIP", true)?;
        let tab_command = settings.get("tab_command", "REIN_TAB_COMMAND");
        let mut child = None;
        let joining = match settings.get("peers", "REIN_PEERS") {
            Some(value) => value.parse::<Joining>()?,
            None => Joining::On(Policy::Accept),
        };
        let (mut name, mut workspace, mut worktree) = (None, None, false);
        let mut max_steps = match settings.get("max_steps", "REIN_MAX_STEPS") {
            Some(value) => steps(&value)?,
            None => Some(DEFAULT_MAX_STEPS),
        };
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
                "--sandbox" => sandbox = on_off("--sandbox", &value(&mut args, &arg)?)?,
                "-s" | "--system" => system = Some(value(&mut args, &arg)?),
                "--max-steps" => max_steps = steps(&value(&mut args, &arg)?)?,
                "--record" => record = Some(PathBuf::from(value(&mut args, &arg)?)),
                "--name" => name = Some(crate::peers::clean_name(&value(&mut args, &arg)?)?),
                "--workspace" => workspace = Some(value(&mut args, &arg)?),
                "--worktree" => worktree = true,
                // Not for typing: a rein starts its sub-agents with it.
                "--child" => {
                    let value = value(&mut args, &arg)?;
                    let (pid, id) = value.split_once(':').ok_or("--child takes <pid>:<id>")?;
                    child = Some((pid.parse().map_err(|_| "--child takes <pid>:<id>")?, id.to_owned()));
                }
                "--" => words.extend(args.by_ref()),
                flag if flag.starts_with('-') && flag.len() > 1 => {
                    return Err(format!("unknown option {flag}; see `rein --help`"));
                }
                _ => words.push(arg),
            }
        }
        let prompt = if words.is_empty() { None } else { Some(words.join(" ")) };
        if worktree && workspace.is_none() {
            return Err("--worktree needs --workspace <name>: the worktree is the workspace's".into());
        }
        Ok(AskArgs {
            model,
            model_given,
            resume,
            effort,
            search,
            system,
            mode,
            record,
            bell,
            compress,
            sandbox,
            memory,
            check,
            max_steps,
            prompt,
            name,
            workspace,
            worktree,
            joining,
            strip,
            child,
            tab_command,
        })
    }
}

/// A switch as written for `key`: `on` or `off`.
pub(crate) fn on_off(key: &str, value: &str) -> CliResult<bool> {
    match value.trim() {
        "on" => Ok(true),
        "off" => Ok(false),
        other => Err(format!("{key} should be on or off, not {other}")),
    }
}

/// A step limit as written: a number of steps, or `off` for none.
fn steps(value: &str) -> CliResult<Option<usize>> {
    match value.trim() {
        "off" => Ok(None),
        number => number.parse::<usize>().ok().filter(|n| *n > 0).map(Some).ok_or_else(|| format!("max_steps should be a number of steps, or off, not {value}")),
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
        assert_eq!(args("--mode yolo hi").err().as_deref(), Some("no mode called yolo; use trust, ask, edits, auto or read-only"));
        // The sandbox is off unless it's turned on, in any mode.
        assert!(!args("--mode edits").unwrap().sandbox);
        assert!(args("--sandbox on --mode trust").unwrap().sandbox);
        assert!(!args("--sandbox off").unwrap().sandbox);
        assert_eq!(args("--sandbox maybe").err().as_deref(), Some("--sandbox should be on or off, not maybe"));
    }

    #[test]
    fn rein_starts_in_trust_mode() {
        assert_eq!(args("hi").unwrap().mode, Mode::Trust);
    }

    #[test]
    fn flags_win_over_the_settings() {
        let settings = Settings::parse("mode = edits\nmodel = from-file\neffort = low").unwrap();
        let parsed = AskArgs::parse(vec!["hi".into()], &settings).unwrap();
        assert_eq!((parsed.mode, parsed.effort), (Mode::Edits, Some(Effort::Low)));
        assert_eq!(args("--effort loud hi").err().as_deref(), Some("no effort called loud; use off, minimal, low, medium, high, xhigh or max"));
        let parsed = AskArgs::parse(["--mode", "trust", "-m", "flagged", "hi"].map(str::to_owned).to_vec(), &settings).unwrap();
        assert_eq!((parsed.mode, parsed.model.as_deref()), (Mode::Trust, Some("flagged")));
    }

    #[test]
    fn the_sandbox_is_off_unless_turned_on() {
        assert!(!args("hi").unwrap().sandbox);
        assert!(AskArgs::parse(vec!["hi".into()], &Settings::parse("sandbox = on").unwrap()).unwrap().sandbox);
        // A flag wins over the file.
        assert!(!AskArgs::parse(vec!["--sandbox".into(), "off".into()], &Settings::parse("sandbox = on").unwrap()).unwrap().sandbox);
        let wrong = AskArgs::parse(vec!["hi".into()], &Settings::parse("sandbox = maybe").unwrap());
        assert_eq!(wrong.err().as_deref(), Some("sandbox should be on or off, not maybe"));
        assert_eq!(args("--mode auto").unwrap().mode, Mode::Auto);
    }

    #[test]
    fn a_step_limit_is_a_number_or_off() {
        assert_eq!(args("hi").unwrap().max_steps, Some(DEFAULT_MAX_STEPS));
        assert_eq!(args("--max-steps 50 hi").unwrap().max_steps, Some(50));
        assert_eq!(args("--max-steps off hi").unwrap().max_steps, None);
        assert_eq!(AskArgs::parse(vec!["hi".into()], &Settings::parse("max_steps = 30").unwrap()).unwrap().max_steps, Some(30));
        for wrong in ["0", "many", "-3"] {
            assert_eq!(args(&format!("--max-steps {wrong} hi")).err(), Some(format!("max_steps should be a number of steps, or off, not {wrong}")));
        }
    }

    #[test]
    fn who_this_rein_is_among_the_others() {
        let parsed = args("--name Web --workspace health").unwrap();
        assert_eq!(
            (parsed.name.as_deref(), parsed.workspace.as_deref(), parsed.joining),
            (Some("web"), Some("health"), Joining::On(Policy::Accept)),
            "let in, unless you say otherwise"
        );
        assert_eq!(args("--worktree").err().as_deref(), Some("--worktree needs --workspace <name>: the worktree is the workspace's"));
        assert!(args("--name a/b").is_err());
        let held = AskArgs::parse(vec![], &Settings::parse("peers = hold").unwrap()).unwrap();
        assert_eq!(held.joining, Joining::On(Policy::Hold));
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
