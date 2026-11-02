//! A conversation: starting or continuing one, its loop of messages and
//! commands, and the menus and sign-ins those open.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use rein_agent::agent::{Agent, Llm, MAX_TURNS, Outcome};
use rein_agent::compact;
use rein_agent::permissions::{Mode, Permissions};
use rein_agent::session::{self, Header, Loaded, Session};
use rein_agent::tools::Tools;
use rein_agent::{context, prompt};
use rein_llm::Item;
use rein_llm::provider::{self, Provider};
use rein_llm::{Effort, ModelInfo};
use crate::commands::{EffortSetting, Slash};
use crate::input::{Menu, Read};
use crate::ui::history::{self, History};
use crate::ui::style::GUTTER;
use crate::args::{AskArgs, Resume};
use crate::terminal::{Last, Terminal, you_said};
use crate::format::{local_time, shorten, shown_path, thousands, traffic};
use crate::{login, CREDENTIALS_FILE, CliResult, config_dir, describe};

pub(crate) async fn ask(http: &rein_llm::reqwest::Client, args: AskArgs, compact_at: Option<String>) -> CliResult {
    let dir = config_dir()?;
    let keys = crate::connect::load_keys(&dir)?;
    let providers = crate::connect::providers(&dir)?;
    let root = std::env::current_dir().map_err(|e| format!("can't find the current directory: {e}"))?;
    let folder = session::folder(&dir.join("sessions"), &root);
    let latest = match args.resume {
        Resume::Latest => match session::list(&folder, 1).into_iter().next() {
            Some(latest) => Some(latest.path),
            None => return Err("no saved conversation in this folder yet".into()),
        },
        _ => None,
    };
    let wanted = args.model.clone().unwrap_or_else(|| crate::connect::default_model(&providers, &keys));
    // Whoever serves the model, as soon as that's known: the model list,
    // which says how big each model's context window is, loads while
    // everything else gets ready.
    let early = if args.model_given || args.resume == Resume::New {
        Some(wanted.clone())
    } else {
        latest.as_deref().and_then(session::peek).map(|header| header.model)
    };
    let mut connected = match &early {
        Some(spec) => {
            let connected = crate::connect::connect(http, &providers, &keys, spec, args.compress).await?;
            say!("{}", connected.pays);
            Some(connected)
        }
        None => None,
    };
    let record = match &args.record {
        Some(path) => Some(File::create(path).map_err(|e| format!("can't create {}: {e}", path.display()))?),
        None => None,
    };
    let permissions = load_permissions(&root, args.mode)?;
    // What you sent from this folder before, for ↑ and Ctrl-R: only a
    // conversation has a box to bring it back in.
    let sent = if args.prompt.is_none() { History::load(history::file(&dir, &root)) } else { History::default() };
    let mut terminal = Terminal::new(root.clone(), args.mode, args.prompt.is_none(), sent, args.bell);
    // The box goes up now, so typing can start while the model list loads.
    terminal.model = connected.as_ref().map(|c| provider::shown(&c.provider, &c.model)).unwrap_or_default();
    terminal.effort = args.effort;
    terminal.input.set_bar(terminal.bar());
    let saved = match args.resume {
        Resume::New => None,
        Resume::Latest => latest,
        Resume::Pick => match pick(&folder, &mut terminal).await? {
            Some(path) => Some(path),
            None => return Ok(()),
        },
    };
    if saved.is_some() && args.system.is_some() {
        return Err("-s can't change a saved conversation's system prompt; start a new one instead".into());
    }
    let canonical = |spec: &str| crate::connect::resolve(spec, &providers).map(|(p, model)| provider::spec(p, &model));
    let wanted = canonical(&wanted)?;
    // A continued conversation keeps its model unless -m says otherwise,
    // and then it's saved with it.
    let theirs = saved.as_deref().and_then(session::peek).map(|header| canonical(&header.model)).transpose()?;
    let spec = match &theirs {
        Some(theirs) if !args.model_given => theirs.clone(),
        _ => wanted,
    };
    let (provider, model_name) = crate::connect::resolve(&spec, &providers)?;
    let provider = provider.clone();
    // Connected to another provider than the conversation's, or to none yet.
    let connected = match connected.take() {
        Some(connected) if connected.provider.name == provider.name => connected,
        _ => {
            let connected = crate::connect::connect(http, &providers, &keys, &spec, args.compress).await?;
            say!("{}{}", " ".repeat(GUTTER), terminal.err.dim(&connected.pays));
            connected
        }
    };
    let (client, models) = connected.models().await;
    let mut setup = Setup {
        folder,
        personal: dir.join("AGENTS.md"),
        root: root.clone(),
        system: args.system.clone(),
        models,
        others: None,
        compact_at,
        http: http.clone(),
        provider,
        providers,
        keys,
        compress: args.compress,
    };
    let (mut loaded, context) = setup.open(saved, &spec)?;
    if theirs.is_some_and(|theirs| theirs != spec) {
        loaded.session.append_model(&spec).map_err(describe)?;
    }
    let limit = setup.limit(&model_name)?;
    terminal.context_limit = limit;
    terminal.model = provider::shown(&setup.provider, &model_name);
    terminal.input.set_bar(terminal.bar());
    let max_output = setup.max_output(&model_name);
    let mut agent = Agent::new(Llm { client, record }, Tools::new(root), permissions, model_name, limit, loaded, context);
    agent.max_output = max_output;
    agent.reasoning_effort = args.effort;
    // A continued conversation is as full as it was.
    terminal.context_used = agent.estimated();
    terminal.input.set_bar(terminal.bar());
    match args.prompt {
        Some(prompt) => {
            terminal.start_turn(Some(&prompt));
            turn(&mut agent, prompt, &mut terminal).await
        }
        None => conversation(&mut agent, &mut terminal, &mut setup).await,
    }
}

/// What it takes to start or reopen a conversation in this folder, kept
/// for `/new`, `/resume` and `/model`.
pub(crate) struct Setup {
    root: PathBuf,
    /// Where this folder's conversations are saved.
    folder: PathBuf,
    /// Your own AGENTS.md, for every project.
    personal: PathBuf,
    /// `-s`: standing instructions added to the system prompt.
    system: Option<String>,
    /// The models the provider in use offers, with their context windows.
    models: Vec<ModelInfo>,
    /// The other providers you can use, and their models, once /model has
    /// looked them up.
    others: Option<Vec<(Provider, Vec<ModelInfo>)>>,
    /// `compact_at` from the settings or the environment.
    compact_at: Option<String>,
    /// For connecting to another provider, when `/model` names one.
    http: rein_llm::reqwest::Client,
    /// The provider in use.
    provider: Provider,
    providers: Vec<Provider>,
    keys: crate::connect::Keys,
    /// Gzip request bodies.
    compress: bool,
}

impl Setup {
    /// A saved conversation (`saved`), or a new one: a header, the system
    /// prompt, and the first message with the project's instructions.
    fn open(&self, saved: Option<PathBuf>, model: &str) -> CliResult<(Loaded, Option<String>)> {
        if let Some(path) = saved {
            let loaded = Session::load(&path).map_err(describe)?;
            say_where_we_were(loaded.session.header(), &loaded.full);
            return Ok((loaded, None));
        }
        let now = local_time(SystemTime::now());
        let header = Header {
            id: rein_llm::new_session_id().map_err(describe)?,
            cwd: self.root.clone(),
            started: format!("{} {:02}:{:02}", now.date(), now.hour, now.minute),
            model: model.to_owned(),
            instructions: Some(prompt::system(self.system.as_deref())),
        };
        let stamp = format!("{}T{:02}-{:02}-{:02}", now.date(), now.hour, now.minute, now.second);
        let files = context::find(&self.root, Some(&self.personal));
        if !files.is_empty() {
            let names: Vec<String> = files.iter().map(|f| shown_path(&f.path, &self.root)).collect();
            say!("Instructions: {}", names.join(", "));
        }
        let context = context::message(&self.root, std::env::consts::OS, &now.date(), &files);
        let session = Session::new(&self.folder, &stamp, header);
        Ok((Loaded { session, history: Vec::new(), full: Vec::new() }, Some(context)))
    }

    /// When to compact with `model`: see [`compact_limit`].
    fn limit(&self, model: &str) -> CliResult<u64> {
        let window = self.models.iter().find(|m| m.id == model).and_then(|m| m.context_window).or(self.provider.context);
        compact_limit(window, self.compact_at.as_deref())
    }

    /// The most one answer from `model` may take, when its provider says.
    fn max_output(&self, model: &str) -> Option<u64> {
        self.models.iter().find(|m| m.id == model).and_then(|m| m.max_output)
    }

    /// `spec` as saved: `provider/model`.
    fn canonical(&self, spec: &str) -> CliResult<String> {
        let (provider, model) = crate::connect::resolve(spec, &self.providers)?;
        Ok(provider::spec(provider, &model))
    }

    /// The model in use, as saved.
    fn current(&self, agent: &Agent<Llm>) -> String {
        provider::spec(&self.provider, &agent.model_name)
    }
}

/// One message from you, worked on until the model is done, or until
/// Ctrl-C. Interrupting drops the turn mid-flight: the request is closed,
/// a running command is killed, and the history is repaired so the
/// conversation can go on.
pub(crate) async fn turn(agent: &mut Agent<Llm>, prompt: String, terminal: &mut Terminal) -> CliResult {
    let interrupted = terminal.interrupted.clone();
    let outcome = tokio::select! {
        outcome = agent.send(prompt, terminal) => Some(outcome),
        _ = tokio::signal::ctrl_c() => None,
        _ = interrupted.notified() => None,
    };
    terminal.end_line();
    if std::mem::take(&mut terminal.last) == Last::You && terminal.fancy {
        say!();
    }
    let Some(outcome) = outcome else {
        agent.repair();
        say!("{}{}", " ".repeat(GUTTER), terminal.err.dim("(interrupted; what should rein do instead?)"));
        terminal.end_turn();
        return Ok(());
    };
    terminal.end_turn();
    // Said once: rein's requests to this server go uncompressed.
    let refused = agent.model.client.traffic().compression == rein_llm::Compression::Refused;
    if refused && !std::mem::replace(&mut terminal.said_uncompressed, true) {
        say!("{}{}", " ".repeat(GUTTER), terminal.err.dim("(this server doesn't take compressed requests, so they go uncompressed)"));
    }
    match outcome.map_err(describe)? {
        Outcome::Done => Ok(()),
        Outcome::Stopped => {
            say!("{}{}", " ".repeat(GUTTER), terminal.err.dim("(stopped; what should rein do instead?)"));
            Ok(())
        }
        Outcome::TurnLimit => Err(format!("stopped after {MAX_TURNS} turns of tool calls; ask again to continue")),
    }
}

/// When to compact: `compact_at` tokens if set (the settings, or
/// $REIN_COMPACT_AT), otherwise rein's default, and never later than 90% of
/// the model's window, when known.
pub(crate) fn compact_limit(window: Option<u64>, compact_at: Option<&str>) -> CliResult<u64> {
    let wanted = match compact_at {
        Some(value) => value.trim().parse::<u64>().map_err(|_| format!("compact_at should be a number of tokens, not {value}"))?,
        None => compact::DEFAULT_LIMIT,
    };
    Ok(window.map_or(wanted, |window| wanted.min(window / 10 * 9)))
}

/// Compacts the conversation now, keeping `focus` in mind. Ctrl-C stops it
/// and leaves the conversation as it was.
pub(crate) async fn compact_now(agent: &mut Agent<Llm>, focus: Option<&str>, terminal: &mut Terminal) -> CliResult {
    terminal.turn.started = Some(std::time::Instant::now());
    let interrupted = terminal.interrupted.clone();
    let done = tokio::select! {
        done = agent.compact(focus, true, terminal) => Some(done),
        _ = tokio::signal::ctrl_c() => None,
        _ = interrupted.notified() => None,
    };
    terminal.end_turn();
    match done {
        Some(done) => done.map(|_| ()).map_err(describe),
        None => {
            say!("(interrupted; nothing was compacted)");
            Ok(())
        }
    }
}

/// Lists this folder's recent conversations and asks which to continue.
/// `None` if the user changes their mind.
pub(crate) async fn pick(folder: &Path, terminal: &mut Terminal) -> CliResult<Option<PathBuf>> {
    let saved = session::list(folder, 30);
    if saved.is_empty() {
        return Err("no saved conversations in this folder yet".into());
    }
    let items = saved
        .iter()
        .map(|conversation| {
            let when = local_time(conversation.modified);
            let first = conversation.first.as_deref().map(|text| shorten(text, 44)).unwrap_or_default();
            let messages = format!("{} message{}", conversation.messages, if conversation.messages == 1 { "" } else { "s" });
            (format!("{} {:02}:{:02}  {first}", when.date(), when.hour, when.minute), messages)
        })
        .collect();
    let menu = Menu { title: "continue which conversation".to_owned(), items, current: None };
    Ok(terminal.input.choose(menu).await.map(|i| saved[i].path.clone()))
}

/// A reminder of where a continued conversation left off.
pub(crate) fn say_where_we_were(header: &Header, history: &[Item]) {
    // The first message is the context rein added, not something typed.
    let typed: Vec<&str> = history
        .iter()
        .skip(1)
        .filter_map(|item| match item {
            Item::User(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    say!("Continuing the conversation from {} ({} message{} so far).", header.started, typed.len(), if typed.len() == 1 { "" } else { "s" });
    if let Some(last) = typed.last() {
        say!("Last message: {}", shorten(last, 70));
    }
}

/// Message after message with the same agent, so it remembers the whole
/// conversation, until /exit or Ctrl-D. A failed turn is reported and the
/// conversation goes on.
pub(crate) async fn conversation(agent: &mut Agent<Llm>, terminal: &mut Terminal, setup: &mut Setup) -> CliResult {
    if !terminal.input.pinned() {
        say!("Type a message, or /help for commands. End a line with \\ to keep typing on the next.");
        say!("Ctrl-C stops an answer; /exit or Ctrl-D leaves.");
    }
    let mut last_interrupt: Option<std::time::Instant> = None;
    loop {
        // The box starts each message on its own; plain lines need a gap.
        if !terminal.input.pinned() {
            say!();
        }
        let line = match message(terminal, &mut last_interrupt).await {
            Said::Message(line) => line,
            Said::NextMode => {
                let mode = next_mode(agent.permissions.mode);
                agent.permissions.mode = mode;
                terminal.mode = mode;
                terminal.input.set_bar(terminal.bar());
                terminal.input.notice(&format!("{} · shift-tab for the next", mode_description(mode)));
                continue;
            }
            Said::Leave => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let pad = " ".repeat(GUTTER);
        match crate::commands::parse(line) {
            Some(Ok(Slash::Exit)) => break,
            Some(parsed) => {
                if terminal.input.pinned() {
                    say!("\n{}", you_said(line, terminal.err, terminal.wrap_width().unwrap_or(80)));
                }
                let done = match parsed {
                    Ok(command) => run_command(command, agent, terminal, setup).await,
                    Err(e) => Err(e),
                };
                if let Err(e) = done {
                    say!("{pad}{}", terminal.err.red(&format!("✗ {e}")));
                }
                terminal.input.set_bar(terminal.bar());
            }
            None => {
                // A plain terminal already shows what was typed; the box doesn't.
                let echo = terminal.input.pinned().then_some(line);
                terminal.start_turn(echo);
                if let Err(e) = turn(agent, line.to_owned(), terminal).await {
                    say!("{pad}{}", terminal.err.red(&format!("✗ {e}")));
                }
            }
        }
    }
    // Everything was saved as it went: say how to come back to it.
    let talked = agent.full().iter().skip(1).any(|item| matches!(item, Item::User(_)));
    if talked && terminal.fancy {
        say!("\n{}", terminal.err.dim("rein -c continues this conversation"));
    }
    Ok(())
}

/// Shift-Tab's order: asking about everything, then about commands only,
/// then nothing running at all. `trust` is only for asking by name.
pub(crate) fn next_mode(mode: Mode) -> Mode {
    match mode {
        Mode::Ask => Mode::Edits,
        Mode::Edits => Mode::ReadOnly,
        Mode::ReadOnly | Mode::Trust => Mode::Ask,
    }
}

/// Does what a slash command asks. What it says goes under the gutter,
/// like a step's result.
pub(crate) async fn run_command(command: Slash, agent: &mut Agent<Llm>, terminal: &mut Terminal, setup: &mut Setup) -> CliResult {
    let pad = " ".repeat(GUTTER);
    let s = terminal.err;
    // A command that takes a choice and was given none shows the choices.
    let Some(command) = choose(command, agent, terminal, setup).await? else { return Ok(()) };
    match command {
        Slash::Help => {
            for line in crate::commands::help() {
                say!("{pad}{line}");
            }
            for keys in [
                "shift-enter, ctrl-j or \\ at the end starts a new line · shift-tab changes the mode",
                "↑ brings back what you sent before, from this folder · ctrl-r finds it by its words",
                "@ points at a file or folder: it's read with your message · @file:10-20 for some lines",
                "enter while rein works steers it: your message goes in at its next step",
                "tab while rein works sends it after the turn instead · esc or ctrl-c stops the turn",
                "wheel, pgup/pgdn or shift-↑↓ scroll · drag over text to copy it",
                "ctrl-o shows every step's full output · ctrl-l draws the window again",
            ] {
                say!("{pad}{}", s.dim(keys));
            }
        }
        // Chosen above.
        Slash::Mode(None) | Slash::Model(None) | Slash::Login(None) | Slash::Logout(None) | Slash::Effort(None) => {}
        Slash::Mode(Some(mode)) => {
            agent.permissions.mode = mode;
            terminal.mode = mode;
            say!("{pad}{} {}", s.green("✓"), mode_description(mode));
        }
        Slash::Model(Some(spec)) => {
            let shown = use_model(&spec, agent, terminal, setup).await?;
            let current = setup.current(agent);
            agent.save_model(&current).map_err(describe)?;
            say!("{pad}{} now using {shown}", s.green("✓"));
            say!("{pad}{}", s.dim("The prompt cache starts over: it's kept per model."));
        }
        Slash::Login(Some(name)) => login_here(&name, agent, terminal, setup).await?,
        Slash::Logout(Some(name)) => {
            crate::connect::logout(&name, &|line| say!("{pad}{line}"))?;
            setup.keys = crate::connect::load_keys(&config_dir()?)?;
            setup.others = None;
        }
        Slash::Effort(Some(setting)) => {
            agent.reasoning_effort = setting.level();
            terminal.effort = agent.reasoning_effort;
            say!("{pad}{} reasoning effort: {}", s.green("✓"), agent.reasoning_effort.map_or("the model's default", Effort::as_str));
        }
        Slash::Status => {
            let header = agent.session().map(|session| session.header().clone());
            let messages = agent.full().iter().skip(1).filter(|item| matches!(item, Item::User(_))).count();
            let rows = [
                ("model", terminal.model.clone()),
                ("provider", format!("{} · {}", setup.provider.title, agent.model.client.base_url())),
                ("effort", agent.reasoning_effort.map_or("the model's default", Effort::as_str).to_owned()),
                ("mode", mode_description(agent.permissions.mode).to_owned()),
                ("rules", format!("{} from .rein/permissions", agent.permissions.rule_count())),
                ("context", format!("about {} of {} tokens before compacting", thousands(agent.estimated()), thousands(agent.limit()))),
                ("sent", traffic(agent.model.client.traffic())),
                ("started", header.map(|h| format!("{} · {messages} message{} so far", h.started, if messages == 1 { "" } else { "s" })).unwrap_or_default()),
                ("folder", setup.root.display().to_string()),
            ];
            for (name, value) in rows {
                say!("{pad}{}  {value}", s.dim(&format!("{name:<8}")));
            }
        }
        Slash::Compact(focus) => compact_now(agent, focus.as_deref(), terminal).await?,
        Slash::Copy => {
            let last = agent.full().iter().rev().find_map(|item| match item {
                Item::Assistant(text) if !text.trim().is_empty() => Some(text.trim()),
                _ => None,
            });
            let Some(text) = last else { return Err("nothing to copy yet: rein hasn't answered in this conversation".into()) };
            let lines = text.lines().count();
            let lines = format!("{lines} line{}", if lines == 1 { "" } else { "s" });
            match crate::input::copy(text) {
                crate::input::Copied::Clipboard => say!("{pad}{} copied rein's last answer ({lines})", s.green("✓")),
                crate::input::Copied::AskedTerminal => say!("{pad}{} asked the terminal to copy rein's last answer ({lines}); some terminals don't allow it", s.green("✓")),
            }
        }
        Slash::Undo => {
            let undone = agent.undo().map_err(describe)?;
            if undone.is_empty() {
                say!("{pad}nothing to undo: rein hasn't changed any files in this session");
            } else {
                say!("{pad}{} put back {}", s.green("✓"), undone.join(", "));
                say!("{pad}{}", s.dim("Commands rein ran aren't undone; the model has been told."));
            }
        }
        Slash::New => {
            let (loaded, context) = setup.open(None, &setup.current(agent))?;
            agent.resume(loaded, context);
            (terminal.context_used, terminal.last_cache) = (0, None);
            say!("{pad}{} a fresh conversation; the last one is saved, /resume brings it back", s.green("✓"));
        }
        Slash::Resume => {
            let Some(path) = pick(&setup.folder, terminal).await? else { return Ok(()) };
            let (loaded, context) = setup.open(Some(path), &setup.current(agent))?;
            // Its own model, from whichever provider serves it: connected
            // first, so a missing key leaves everything as it was.
            let model = setup.canonical(&loaded.session.header().model)?;
            if model != setup.current(agent) {
                use_model(&model, agent, terminal, setup).await?;
            }
            agent.resume(loaded, context);
            (terminal.context_used, terminal.last_cache) = (agent.estimated(), None);
        }
        Slash::Exit => {}
    }
    Ok(())
}

/// What `command` is, once its choice is made: a command given no choice
/// (`/model`, `/login`, `/mode`...) shows what there is to choose from.
/// `None` when you change your mind.
pub(crate) async fn choose(command: Slash, agent: &Agent<Llm>, terminal: &mut Terminal, setup: &mut Setup) -> CliResult<Option<Slash>> {
    let menu = |title: &str, items: Vec<(String, String)>, current: Option<usize>| Menu { title: title.to_owned(), items, current };
    Ok(match command {
        Slash::Mode(None) => {
            let modes = Mode::ALL;
            // "edits: file changes ...": the name is the item's already.
            let about = |m: Mode| mode_description(m).split_once(": ").map_or(mode_description(m), |(_, about)| about).to_owned();
            let items = modes.iter().map(|&m| (m.to_string(), about(m))).collect();
            let current = modes.iter().position(|&m| m == agent.permissions.mode);
            terminal.input.choose(menu("what runs without asking", items, current)).await.map(|i| Slash::Mode(Some(modes[i])))
        }
        Slash::Effort(None) => {
            let about = |effort: Option<Effort>| match effort {
                None => "the model's own",
                Some(Effort::Minimal) => "as little thinking as there can be",
                Some(Effort::Low) => "quick: for simple work",
                Some(Effort::Medium) => "a balance",
                Some(Effort::High) => "thorough",
                Some(Effort::XHigh) => "more thorough, where the model has it",
                Some(Effort::Max) => "the most thinking, where the model has it",
            };
            let levels: Vec<Option<Effort>> = std::iter::once(None).chain(Effort::ALL.map(Some)).collect();
            let items = levels.iter().map(|&level| (level.map_or("default", Effort::as_str).to_owned(), about(level).to_owned())).collect();
            let current = levels.iter().position(|&level| level == agent.reasoning_effort);
            let chosen = terminal.input.choose(menu("how hard the model thinks", items, current)).await;
            chosen.map(|i| Slash::Effort(Some(levels[i].map_or(EffortSetting::Default, EffortSetting::Level))))
        }
        Slash::Model(None) => {
            let mut specs = Vec::new();
            let mut items = Vec::new();
            let mut add = |provider: &Provider, models: &[ModelInfo]| {
                for model in models {
                    let window = model.context_window.map(|w| format!(" · {} tokens", thousands(w))).unwrap_or_default();
                    specs.push(provider::spec(provider, &model.id));
                    items.push((provider::shown(provider, &model.id), format!("{}{window}", provider.title)));
                }
            };
            if setup.models.is_empty() {
                // The list couldn't be had: the one in use, at least.
                add(&setup.provider, &[ModelInfo { id: agent.model_name.clone(), name: agent.model_name.clone(), context_window: None, max_output: None }]);
            } else {
                add(&setup.provider, &setup.models);
            }
            for (provider, models) in other_models(setup, terminal).await {
                add(&provider, &models);
            }
            let current = specs.iter().position(|spec| *spec == setup.current(agent));
            terminal.input.choose(menu("choose a model", items, current)).await.map(|i| Slash::Model(Some(specs[i].clone())))
        }
        Slash::Login(None) => {
            // Keys may have come or gone in another terminal since.
            setup.keys = crate::connect::load_keys(&config_dir()?)?;
            let signed_in = config_dir()?.join(CREDENTIALS_FILE).exists();
            let mut names = vec!["chatgpt".to_owned()];
            let mut items = vec![("chatgpt".to_owned(), if signed_in { "signed in: your ChatGPT plan pays for OpenAI's models" } else { "your ChatGPT plan, for OpenAI's models" }.to_owned())];
            for provider in setup.providers.iter().filter(|p| p.needs_key) {
                names.push(provider.name.clone());
                let how = if provider.sign_in_url.is_some() { ", signs in in the browser" } else { "" };
                items.push((provider.name.clone(), format!("{}{how}", crate::connect::pays(provider, &setup.keys, false))));
            }
            let current = names.iter().position(|name| *name == setup.provider.name);
            terminal.input.choose(menu("sign in to a provider", items, current)).await.map(|i| Slash::Login(Some(names[i].clone())))
        }
        Slash::Logout(None) => {
            setup.keys = crate::connect::load_keys(&config_dir()?)?;
            let mut names: Vec<String> = Vec::new();
            if config_dir()?.join(CREDENTIALS_FILE).exists() {
                names.push("chatgpt".into());
            }
            names.extend(setup.keys.keys().cloned());
            if names.is_empty() {
                return Err("nothing to forget: no saved keys, and not signed in with ChatGPT".into());
            }
            let items = names.iter().map(|name| (name.clone(), if name == "chatgpt" { "sign out of ChatGPT" } else { "forget the saved key" }.to_owned())).collect();
            terminal.input.choose(menu("forget which", items, None)).await.map(|i| Slash::Logout(Some(names[i].clone())))
        }
        command => Some(command),
    })
}

/// The other providers you can use and their models, looked up all at
/// once the first time, a few seconds at most.
pub(crate) async fn other_models(setup: &mut Setup, terminal: &mut Terminal) -> Vec<(Provider, Vec<ModelInfo>)> {
    if let Some(others) = &setup.others {
        return others.clone();
    }
    let others: Vec<Provider> = setup.providers.iter().filter(|p| p.name != setup.provider.name && crate::connect::usable(p, &setup.keys)).cloned().collect();
    if others.is_empty() {
        return Vec::new();
    }
    terminal.doing("Looking up the other providers' models");
    let lookups: Vec<_> = others
        .into_iter()
        .map(|provider| {
            let (http, keys) = (setup.http.clone(), setup.keys.clone());
            tokio::spawn(async move {
                let (client, _) = crate::connect::client_for(&http, &provider, &keys).await.ok()?;
                let models = tokio::time::timeout(Duration::from_secs(5), client.list_models()).await.ok()?.ok()?;
                Some((provider, models))
            })
        })
        .collect();
    let mut found = Vec::new();
    for lookup in lookups {
        if let Ok(Some(other)) = lookup.await {
            found.push(other);
        }
    }
    terminal.quiet();
    terminal.input.busy(None, std::time::Instant::now());
    setup.others = Some(found.clone());
    found
}

/// `/login <name>`: signs in without leaving the conversation. A key is
/// pasted in the box, where it shows as dots; a provider with a browser
/// sign-in, and ChatGPT, sign in there. Once done, it's ready for /model,
/// and if it's the provider in use, its next request pays the new way.
pub(crate) async fn login_here(name: &str, agent: &mut Agent<Llm>, terminal: &mut Terminal, setup: &mut Setup) -> CliResult {
    let pad = " ".repeat(GUTTER);
    let s = terminal.err;
    let tell = |text: &str| {
        for line in text.trim_matches('\n').lines() {
            say!("{pad}{line}");
        }
    };
    let cancelled = || say!("{pad}{}", s.dim("(cancelled; nothing was saved)"));
    let signed_in = if name == "chatgpt" {
        if waiting(terminal, "Waiting for you to sign in to ChatGPT in the browser", login(&setup.http, &tell)).await?.is_none() {
            cancelled();
            return Ok(());
        }
        "openai"
    } else {
        let provider = crate::connect::find(&setup.providers, name)?.clone();
        let made = crate::connect::sign_in(&provider, false)?;
        let key = match made {
            Some(auth_url) => {
                let what = format!("Waiting for you to sign in to {} in the browser", provider.title);
                waiting(terminal, &what, crate::connect::key_from_browser(&setup.http, &provider, auth_url, &tell)).await?
            }
            None => match terminal.input.read_secret(&crate::connect::paste_prompt(&provider)).await {
                Some(Read::Line(key)) => Some(key),
                Some(_) => None,
                None => return Err(format!("a key typed here would show; run `rein login {name}` in a terminal instead")),
            },
        };
        let Some(key) = key else {
            cancelled();
            return Ok(());
        };
        crate::connect::save_key(&setup.http, &provider, key, made.is_some(), &tell).await?;
        name
    };
    setup.keys = crate::connect::load_keys(&config_dir()?)?;
    setup.others = None;
    if setup.provider.name == signed_in {
        let connected = crate::connect::connect(&setup.http, &setup.providers, &setup.keys, &setup.current(agent), setup.compress).await?;
        say!("{pad}{}", s.dim(&connected.pays));
        let (client, models) = connected.models().await;
        agent.model.client = client;
        if !models.is_empty() {
            setup.models = models;
        }
    } else {
        say!("{pad}{}", s.dim(&format!("/model {signed_in}/<model> switches to it; `rein models {signed_in}` lists them")));
    }
    Ok(())
}

/// Runs `work`, saying `what` over the box, until it's done or you stop it
/// with Esc or Ctrl-C (`None`).
pub(crate) async fn waiting<T>(terminal: &mut Terminal, what: &str, work: impl std::future::Future<Output = CliResult<T>>) -> CliResult<Option<T>> {
    terminal.turn.started = Some(std::time::Instant::now());
    terminal.doing(what);
    let interrupted = terminal.interrupted.clone();
    let done = tokio::select! {
        done = work => Some(done),
        _ = tokio::signal::ctrl_c() => None,
        _ = interrupted.notified() => None,
    };
    terminal.quiet();
    terminal.input.busy(None, std::time::Instant::now());
    done.transpose()
}

/// Switches to the model `spec`, connecting to its provider if that's
/// another one. Returns the model as shown.
pub(crate) async fn use_model(spec: &str, agent: &mut Agent<Llm>, terminal: &mut Terminal, setup: &mut Setup) -> CliResult<String> {
    let (provider, model) = crate::connect::resolve(spec, &setup.providers)?;
    let provider = provider.clone();
    let missing = |models: &[ModelInfo]| !models.is_empty() && !models.iter().any(|m| m.id == model);
    if provider.name != setup.provider.name {
        let connected = crate::connect::connect(&setup.http, &setup.providers, &setup.keys, spec, setup.compress).await?;
        say!("{}{}", " ".repeat(GUTTER), terminal.err.dim(&connected.pays));
        let (client, models) = connected.models().await;
        if missing(&models) {
            return Err(format!("{} has no model called {model}; `rein models {}` lists them", provider.title, provider.name));
        }
        agent.model.client = client;
        (setup.provider, setup.models) = (provider, models);
        // The others are others now.
        setup.others = None;
    } else if missing(&setup.models) {
        return Err(format!("no model called {model}; /model lists them"));
    }
    agent.set_limit(setup.limit(&model)?);
    terminal.context_limit = agent.limit();
    agent.max_output = setup.max_output(&model);
    terminal.model = provider::shown(&setup.provider, &model);
    agent.model_name = model;
    Ok(terminal.model.clone())
}

pub(crate) fn mode_description(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask => "ask: file changes and commands ask first",
        Mode::Edits => "edits: file changes in this folder don't ask; commands do",
        Mode::ReadOnly => "read-only: nothing runs or changes",
        Mode::Trust => "trust: everything runs without asking; only deny rules stop it",
    }
}

/// The mode plus the project's `.rein/permissions`, if it has one. Says what
/// it's using whenever that's anything other than the plain default.
pub(crate) fn load_permissions(root: &Path, mode: Mode) -> CliResult<Permissions> {
    let path = root.join(".rein").join("permissions");
    let rules = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("can't read .rein/permissions: {e}")),
    };
    let permissions = Permissions::new(mode, &rules).map_err(|e| format!(".rein/permissions {e}"))?;
    let count = permissions.rule_count();
    if count > 0 {
        say!("Using {count} rule{} from .rein/permissions", if count == 1 { "" } else { "s" });
    }
    match mode {
        Mode::Ask => {}
        Mode::Edits => say!("Mode: edits (file changes in this folder don't ask; commands do)"),
        Mode::ReadOnly => say!("Mode: read-only (nothing runs or changes)"),
        Mode::Trust => say!("Mode: trust. Commands run and files change without asking; only deny rules stop them"),
    }
    Ok(permissions)
}

/// What you did at the prompt.
pub(crate) enum Said {
    Message(String),
    /// Shift-Tab: on to the next mode.
    NextMode,
    /// Ctrl-D, or Ctrl-C twice.
    Leave,
}

/// Reads your next message. A line ending in `\` continues on the next.
/// Ctrl-C clears what you typed; a second one within two seconds, or
/// Ctrl-D, ends the conversation.
pub(crate) async fn message(terminal: &mut Terminal, last_interrupt: &mut Option<std::time::Instant>) -> Said {
    let mut text = String::new();
    loop {
        let prompt = if text.is_empty() { "you    " } else { "…      " };
        let read = tokio::select! {
            read = terminal.input.read(prompt, true) => read,
            // Without an editor, Ctrl-C is a signal; the terminal has
            // already thrown away what was typed.
            _ = tokio::signal::ctrl_c() => Read::Interrupted,
        };
        match read {
            Read::Line(line) => match line.strip_suffix('\\') {
                Some(more) => {
                    text.push_str(more);
                    text.push('\n');
                }
                None => {
                    text.push_str(&line);
                    return Said::Message(text);
                }
            },
            // A message is typed, never picked from a menu.
            Read::Picked(_) => {}
            Read::Interrupted => {
                if last_interrupt.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                    return Said::Leave;
                }
                *last_interrupt = Some(std::time::Instant::now());
                if terminal.input.pinned() {
                    terminal.input.notice("press ctrl-c again to leave");
                } else {
                    say!("{}", terminal.err.dim("(press Ctrl-C again to leave)"));
                }
                text.clear();
            }
            Read::Eof => return Said::Leave,
            Read::NextMode => return Said::NextMode,
        }
    }
}
