//! A conversation: starting or continuing one, its loop of messages and
//! commands, and the menus and sign-ins those open.

use crate::args::{AskArgs, Resume};
use crate::commands::{EffortSetting, Slash};
use crate::format::{local_time, shorten, shown_path, thousands, traffic};
use crate::input::{Menu, Read};
use crate::terminal::{Last, Terminal, you_said};
use crate::ui::history::{self, History};
use crate::ui::style::GUTTER;
use crate::{CREDENTIALS_FILE, CliResult, config_dir, describe, login};
use rein_agent::agent::{Agent, Llm, Outcome};
use rein_agent::compact;
use rein_agent::permissions::{Mode, Permissions, Sandboxing, Whose};
use rein_agent::sandbox::{self, Missing, Places, Sandbox};
use rein_agent::session::{self, Header, Loaded, Session};
use rein_agent::tools::Tools;
use rein_agent::{context, prompt};
use rein_llm::Item;
use rein_llm::ModelInfo;
use rein_llm::provider::{self, Provider};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub(crate) async fn ask(http: &rein_llm::reqwest::Client, args: AskArgs, compact_at: Option<String>) -> CliResult {
    let dir = config_dir()?;
    let keys = crate::connect::load_keys(&dir)?;
    let providers = crate::connect::providers(&dir)?;
    if args.worktree
        && let Some(workspace) = &args.workspace
    {
        let here = std::env::current_dir().map_err(|e| format!("can't find the current directory: {e}"))?;
        let tree = crate::peers::worktree::enter(&here, workspace).await?;
        std::env::set_current_dir(&tree).map_err(|e| format!("can't go into {}: {e}", tree.display()))?;
        say!("Working in {}, workspace {workspace}'s own worktree", tree.display());
    }
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
    let early = if args.model_given || args.resume == Resume::New { Some(wanted.clone()) } else { latest.as_deref().and_then(session::peek).map(|header| header.model) };
    if args.prompt.is_none() {
        crate::ui::logo::show(&root, early.as_deref());
    }
    let mut connected = match &early {
        Some(spec) => {
            let connected = crate::connect::connect(http, &providers, &keys, spec, args.compress).await?;
            say_pays(&connected.pays, false);
            Some(connected)
        }
        None => None,
    };
    let record = match &args.record {
        Some(path) => Some(File::create(path).map_err(|e| format!("can't create {}: {e}", path.display()))?),
        None => None,
    };
    let sandbox = std::sync::Arc::new(find_sandbox(&root, &dir).await);
    let permissions = load_permissions(&root, &dir, args.mode, args.sandbox, sandbox.clone())?;
    // What you sent from this folder before, for ↑ and Ctrl-R: only a
    // conversation has a box to bring it back in.
    let sent = if args.prompt.is_none() { History::load(history::file(&dir, &root)) } else { History::default() };
    let mut terminal = Terminal::new(root.clone(), sandbox.clone(), args.mode, args.prompt.is_none(), sent, args.bell);
    terminal.sandboxing = permissions.sandboxing();
    // The box goes up now, so typing can start while the model list loads.
    terminal.model = connected.as_ref().map(|c| provider::shown(&c.provider, &c.model)).unwrap_or_default();
    terminal.effort = args.effort;
    terminal.input.set_bar(terminal.bar());
    // Your MCP servers and the project's start now, while the rest gets
    // ready; the project's only once you've said yes to each as it is.
    let mcp = crate::mcp::open(&dir, &root, sandbox.clone(), http.clone(), &mut terminal, args.prompt.is_none()).await;
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
            say_pays(&connected.pays, true);
            connected
        }
    };
    let (client, models) = connected.models().await;
    let secrets = rein_agent::redact::Secrets::new(known_secrets(&keys).into_iter().chain(mcp.secrets()));
    // What `/undo` puts back is kept beside the conversations, by project.
    let undo = session::folder(&dir.join("undo"), &root);
    let tools = Tools::new(root.clone()).with_sandbox(sandbox.clone()).with_secrets(secrets).with_mcp(mcp.servers.clone()).with_undo(undo).with_images(folder.join("images"));
    // Notes across conversations, for this project: rein's, and Claude
    // Code's, read.
    let memory = args.memory.then(|| rein_agent::memory::Memory::for_project(&dir, std::env::home_dir().as_deref(), &root));
    let tools = match &memory {
        Some(memory) => tools.with_memory(memory.clone()),
        None => tools,
    };
    let mut setup = Setup {
        folder,
        personal: dir.join("AGENTS.md"),
        root: root.clone(),
        system: args.system.clone(),
        models,
        others: None,
        compact_at,
        max_steps: args.max_steps,
        http: http.clone(),
        provider,
        providers,
        keys,
        compress: args.compress,
        sandbox: sandbox.clone(),
        mcp,
        search: args.search.clone(),
        memory,
    };
    let source = if saved.is_some() { rein_agent::hooks::Source::Resume } else { rein_agent::hooks::Source::Startup };
    let (mut loaded, context) = setup.open(saved, &spec)?;
    if theirs.is_some_and(|theirs| theirs != spec) {
        loaded.session.append_model(&spec).map_err(describe)?;
    }
    let limit = setup.limit(&model_name)?;
    terminal.context_limit = limit;
    terminal.model = provider::shown(&setup.provider, &model_name);
    terminal.spec.clone_from(&spec);
    crate::ui::logo::update(|facts| facts.model = Some(crate::ui::logo::model_name(&spec)));
    terminal.tab_command.clone_from(&args.tab_command);
    terminal.input.set_bar(terminal.bar());
    let max_output = setup.max_output(&model_name);
    let mut agent = Agent::new(Llm { client, record }, tools, permissions, model_name, limit, loaded, context);
    terminal.input.follow_jobs(agent.tools.jobs());
    // Only a conversation joins the others: a task done on its own has no
    // one to answer what they send.
    let child = args.child.as_ref().map(|(pid, id)| (*pid, id.as_str()));
    if args.prompt.is_none()
        && crate::peers::talk::join(args.joining, args.name.as_deref(), args.workspace.as_deref(), child, args.strip, &dir, &root, args.mode, &mut terminal).await
    {
        // A sub-agent doesn't start sub-agents: one level, no further.
        agent = if child.is_some() { agent.with_child_peer_tools() } else { agent.with_peer_tools() };
    }
    agent.max_output = max_output;
    agent.reasoning_effort = args.effort;
    agent.efforts = setup.efforts(&agent.model_name);
    agent.finds_tools = setup.finds_tools(&agent.model_name);
    agent.sees = setup.sees(&agent.model_name);
    crate::search::follow(&mut agent, &setup).await;
    // A task run with -p asks nothing: a project's skills wait for a yes.
    let room = setup.skill_room(&agent.model_name);
    crate::skills::refresh(&mut agent, &mut terminal, &root, room, crate::skills::Look::Start { ask: args.prompt.is_none() }).await;
    terminal.effort = crate::effort::now(&agent);
    if let Some(why) = crate::effort::unlike_chosen(&agent) {
        say!("{}{}", " ".repeat(GUTTER), terminal.err.dim(&format!("({why})")));
    }
    agent.step_limit = step_limit(args.mode, args.prompt.is_some(), args.max_steps);
    // Your hooks, and those on a session starting.
    // A project's that wait for a yes are asked about where there's someone
    // to ask: not in a task run on its own, nor in a sub-agent's tab, which
    // has its task to do.
    crate::hooks::start(&mut agent, &mut terminal, &dir, source, args.prompt.is_none() && args.child.is_none()).await;
    // A continued conversation is as full as it was.
    terminal.context_used = agent.estimated();
    terminal.input.set_bar(terminal.bar());
    match args.prompt {
        // A task left to run on its own that didn't finish says so in how
        // rein exits, for whatever started it.
        Some(prompt) => {
            terminal.start_turn(Some(&prompt));
            for (skill, given) in crate::skills::called(&prompt, agent.skills()) {
                agent.call_skill(&skill, given);
            }
            let outcome = turn(&mut agent, Start::You(prompt), &mut terminal).await;
            crate::skills::count(agent.take_loaded());
            agent.session_ended(rein_agent::hooks::Ended::Exit, &mut terminal).await;
            match outcome? {
                Outcome::StepLimit(_) | Outcome::Stuck { .. } => Err("the task isn't finished".into()),
                Outcome::Refused(by) => Err(format!("your hook {} said no to the task: {}", by.hook, by.said)),
                Outcome::Done | Outcome::Stopped | Outcome::HookStopped(_) => Ok(()),
            }
        }
        None => conversation(&mut agent, &mut terminal, &mut setup).await,
    }
}

/// What pays for the model: beside the logo, or said, `dim` and indented
/// if `quiet`, where there's none.
fn say_pays(pays: &str, quiet: bool) {
    if crate::ui::logo::update(|facts| facts.pays = Some(crate::ui::logo::pays_briefly(pays))) {
        return;
    }
    if quiet {
        say!("{}{}", " ".repeat(GUTTER), crate::ui::style::Style::stderr().dim(pays));
    } else {
        say!("{pays}");
    }
}

/// The keys and tokens rein knows, for its tools to hide: those it took
/// out of its environment, the keys you saved, and the saved Claude Code
/// token. ChatGPT's tokens change as they're renewed, and their file is out
/// of every tool's reach.
fn known_secrets(keys: &crate::connect::Keys) -> Vec<String> {
    let kept = sandbox::secrets::all().iter().filter_map(|(_, value)| value.to_str().map(str::to_owned));
    let code = config_dir()
        .ok()
        .and_then(|dir| rein_llm::store::read_json::<crate::claude::CodeCredentials>(&dir.join(crate::claude::CODE_FILE)).ok().flatten())
        .and_then(|saved| saved.token);
    kept.chain(keys.values().cloned()).chain(code).collect()
}

/// What it takes to start or reopen a conversation in this folder, kept
/// for `/new`, `/resume` and `/model`.
pub(crate) struct Setup {
    root: PathBuf,
    /// Where this folder's conversations are saved.
    folder: PathBuf,
    /// Your own AGENTS.md, for every project.
    personal: PathBuf,
    /// The project's notes across conversations, with memory on: a new
    /// conversation starts with their index.
    memory: Option<rein_agent::memory::Memory>,
    /// `-s`: standing instructions added to the system prompt.
    system: Option<String>,
    /// The models the provider in use offers, with their context windows.
    models: Vec<ModelInfo>,
    /// The other providers you can use, and their models, once /model has
    /// looked them up.
    others: Option<Vec<(Provider, Vec<ModelInfo>)>>,
    /// `compact_at` from the settings or the environment.
    compact_at: Option<String>,
    /// How many steps a turn nobody is watching may take.
    max_steps: Option<usize>,
    /// For connecting to another provider, when `/model` names one.
    http: rein_llm::reqwest::Client,
    /// The provider in use.
    provider: Provider,
    providers: Vec<Provider>,
    keys: crate::connect::Keys,
    /// Gzip request bodies.
    compress: bool,
    /// What commands can't reach, which the project's instruction files
    /// can't bring into the conversation either.
    sandbox: std::sync::Arc<Sandbox>,
    /// The MCP servers rein started, and those it didn't: for `/mcp`.
    mcp: crate::mcp::Mcp,
    /// Who searches the web for the model, as you chose.
    search: crate::search::SearchWith,
}

impl Setup {
    pub(crate) fn providers(&self) -> &[Provider] {
        &self.providers
    }

    /// The provider in use.
    pub(crate) fn provider(&self) -> &Provider {
        &self.provider
    }

    /// The models the provider in use offers.
    pub(crate) fn models(&self) -> &[ModelInfo] {
        &self.models
    }

    pub(crate) fn http(&self) -> &rein_llm::reqwest::Client {
        &self.http
    }

    pub(crate) fn keys(&self) -> &crate::connect::Keys {
        &self.keys
    }

    /// Where the images shown in this folder's conversations are kept.
    pub(crate) fn images(&self) -> std::path::PathBuf {
        self.folder.join("images")
    }

    pub(crate) fn search(&self) -> &crate::search::SearchWith {
        &self.search
    }

    pub(crate) fn set_search(&mut self, search: crate::search::SearchWith) {
        self.search = search;
    }

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
        let files = context::find(&self.root, Some(&self.personal), &|path: &Path| self.sandbox.places().hides(path));
        let names: Vec<String> = files.iter().map(|f| shown_path(&f.path, &self.root)).collect();
        // Beside the logo, or said where there's none.
        if !crate::ui::logo::update(|facts| facts.instructions.clone_from(&names)) && !names.is_empty() {
            say!("Instructions: {}", names.join(", "));
        }
        let mut context = context::message(&self.root, std::env::consts::OS, &now.date(), &files);
        // In the first message, saved with it: the same text all the
        // conversation long, so the cache holds as notes are saved.
        if let Some(memory) = &self.memory {
            context.push_str("\n\n");
            context.push_str(&memory.index());
            let notes = memory.all();
            if !notes.is_empty() {
                let theirs = notes.iter().filter(|note| note.whose == rein_agent::memory::Whose::ClaudeCode).count();
                let of_theirs = if theirs > 0 { format!(", {theirs} of them Claude Code's") } else { String::new() };
                say!("Memory: {} note{}{of_theirs} · /memory", notes.len(), if notes.len() == 1 { "" } else { "s" });
            }
        }
        let session = Session::new(&self.folder, &stamp, header);
        Ok((Loaded { session, history: Vec::new(), full: Vec::new() }, Some(context)))
    }

    /// When to compact with `model`: see [`compact_limit`].
    fn limit(&self, model: &str) -> CliResult<u64> {
        compact_limit(self.window(model), self.compact_at.as_deref())
    }

    /// How many tokens `model` takes, when its provider says.
    fn window(&self, model: &str) -> Option<u64> {
        self.models.iter().find(|m| m.id == model).and_then(|m| m.context_window).or(self.provider.context)
    }

    /// The room the list of skills gets with `model`.
    pub(crate) fn skill_room(&self, model: &str) -> usize {
        rein_agent::skills::room(self.window(model))
    }

    /// The most one answer from `model` may take, when its provider says.
    fn max_output(&self, model: &str) -> Option<u64> {
        self.models.iter().find(|m| m.id == model).and_then(|m| m.max_output)
    }

    /// The effort levels `model` takes, as its provider lists them.
    fn efforts(&self, model: &str) -> rein_llm::Efforts {
        self.models.iter().find(|m| m.id == model).map(|m| m.efforts.clone()).unwrap_or_default()
    }

    /// Whether `model` can have tools held back until it finds them, as
    /// its provider lists it.
    fn finds_tools(&self, model: &str) -> rein_llm::FindsTools {
        self.models.iter().find(|m| m.id == model).map(|m| m.finds_tools).unwrap_or_default()
    }

    /// Whether `model` can see images, as its provider lists it.
    fn sees(&self, model: &str) -> rein_llm::Sees {
        self.models.iter().find(|m| m.id == model).map(|m| m.sees).unwrap_or_default()
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
/// Works on `prompt` until the turn ends, and says how it did if that
/// wasn't by finishing.
pub(crate) async fn turn(agent: &mut Agent<Llm>, start: Start, terminal: &mut Terminal) -> CliResult<Outcome> {
    let interrupted = terminal.interrupted.clone();
    let effort = crate::effort::watch(agent);
    let work = async {
        match &start {
            Start::You(prompt) => {
                // What was pasted goes with it, if its label is still there.
                for image in crate::paste::sent_with(prompt, std::mem::take(&mut terminal.pasted)) {
                    agent.paste(image);
                }
                agent.send(prompt.clone(), terminal).await
            }
            Start::Peer { from, text } => agent.hear(from, text, terminal).await,
            Start::Ended => agent.hear_ended(terminal).await,
        }
    };
    let outcome = tokio::select! {
        outcome = work => Some(outcome),
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
        terminal.state(crate::peers::State::Idle);
        return Ok(Outcome::Stopped);
    };
    terminal.end_turn();
    // Said once: rein's requests to this server go uncompressed.
    let refused = agent.model.client.traffic().compression == rein_llm::Compression::Refused;
    if refused && !std::mem::replace(&mut terminal.said_uncompressed, true) {
        say!("{}{}", " ".repeat(GUTTER), terminal.err.dim("(this server doesn't take compressed requests, so they go uncompressed)"));
    }
    if let Some(said) = effort.said(agent) {
        say!("{}{}", " ".repeat(GUTTER), terminal.err.dim(&said));
        terminal.effort = crate::effort::now(agent);
        terminal.input.set_bar(terminal.bar());
    }
    // Finished: the tab says so until the next turn starts. Stopped,
    // failed or cut short, it's just idle.
    terminal.state(if matches!(outcome, Ok(Outcome::Done)) { crate::peers::State::Done } else { crate::peers::State::Idle });
    // Done while you're away: your hooks on it run, as the bell rings.
    if matches!(outcome, Ok(Outcome::Done)) && terminal.input.away() {
        agent.hook(rein_agent::hooks::Event::Notification(rein_agent::hooks::Notice::Idle, "rein is waiting for your input"), terminal).await;
    }
    let outcome = outcome.map_err(describe)?;
    let said = match &outcome {
        Outcome::Done => None,
        Outcome::Stopped => Some("(stopped; what should rein do instead?)".to_owned()),
        Outcome::StepLimit(steps) => Some(format!("(stopped after {steps} steps, the most a turn may take when nobody's watching; type continue to go on)")),
        Outcome::Stuck { call, times } => {
            let (verb, what) = crate::ui::steps::describe(call);
            Some(format!("(stopped going round in circles: {verb} {what}, {times} times in a row with the same result; say what to do instead, or type continue)"))
        }
        // Not sent: what you typed is back in the box, to change.
        Outcome::Refused(by) => {
            if let Start::You(prompt) = &start {
                terminal.input.insert(prompt);
            }
            Some(format!("(your hook {} said no to this message, so it wasn't sent: {}; it's back in the box)", by.hook, by.said.trim()))
        }
        Outcome::HookStopped(by) => {
            Some(format!("(your hook {} stopped the turn{})", by.hook, if by.said.trim().is_empty() { String::new() } else { format!(": {}", by.said.trim()) }))
        }
    };
    if let Some(said) = said {
        say!("{}{}", " ".repeat(GUTTER), terminal.err.dim(&said));
    }
    Ok(outcome)
}

/// What a turn starts with.
pub(crate) enum Start {
    /// Your message.
    You(String),
    /// What another rein sent.
    Peer { from: String, text: String },
    /// A command in the background ended while nobody was talking.
    Ended,
}

/// How many steps a turn may take: no limit while you're watching, who can
/// press Esc; `max_steps` when nobody is, in auto mode or a task rein was
/// given to do on its own.
pub(crate) fn step_limit(mode: Mode, one_shot: bool, max_steps: Option<usize>) -> Option<usize> {
    if mode == Mode::Auto || one_shot { max_steps } else { None }
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
    let menu = Menu { title: "continue which conversation".to_owned(), items, current: None, at: None };
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
    // Turns other reins started since you last said something.
    let mut in_a_row = 0;
    loop {
        // The box starts each message on its own; plain lines need a gap.
        if !terminal.input.pinned() {
            say!();
        }
        // How much of your limits the last turn left, where the provider
        // can be asked; it comes while you type.
        terminal.meters.ask(&agent.model.client, &agent.model_name);
        let jobs = agent.tools.jobs().clone();
        let line = match message(terminal, &jobs, &mut last_interrupt).await {
            Said::Message(line) => {
                in_a_row = 0;
                // Typed into a sub-agent: it's yours now, and stays open.
                terminal.adopted = true;
                line
            }
            Said::Peer(incoming) => {
                if let Err(e) = crate::peers::talk::take_in(incoming, &mut in_a_row, agent, terminal).await {
                    say!("{}{}", " ".repeat(GUTTER), terminal.err.red(&format!("✗ {e}")));
                }
                if leaves(terminal) {
                    break;
                }
                continue;
            }
            // The model hears of it, and goes on from there.
            Said::Ended => {
                terminal.start_turn(None);
                let outcome = turn(agent, Start::Ended, terminal).await;
                crate::peers::talk::close_finished(terminal).await;
                if let Err(e) = outcome {
                    say!("{}{}", " ".repeat(GUTTER), terminal.err.red(&format!("✗ {e}")));
                }
                continue;
            }
            Said::NextMode => {
                let mode = agent.permissions.mode.next();
                agent.permissions.mode = mode;
                agent.step_limit = step_limit(mode, false, setup.max_steps);
                terminal.mode = mode;
                if let Some(peers) = &terminal.peers {
                    peers.follow_mode(mode);
                }
                terminal.input.set_bar(terminal.bar());
                terminal.input.notice(&format!("{} · shift-tab for the next", crate::modes::description(mode, terminal.sandboxing)));
                continue;
            }
            Said::Paste => {
                crate::paste::paste(terminal, &setup.images()).await;
                continue;
            }
            Said::Leave => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let pad = " ".repeat(GUTTER);
        // A command is rein's; anything else is a message, and the skills
        // are looked at again for it: one added or changed since is there.
        let mut parsed = match crate::commands::parse(line).filter(|_| !crate::skills::starts_with_skill(line, agent.skills())) {
            Some(Ok(command)) => Some(Ok(command)),
            // Not one of rein's: maybe a skill added since.
            other => {
                crate::skills::refresh(agent, terminal, &setup.root, setup.skill_room(&agent.model_name), crate::skills::Look::Message).await;
                other.filter(|_| !crate::skills::starts_with_skill(line, agent.skills()))
            }
        };
        // A skill of yours that can't be called as it is: turned on if you
        // say so, or why not; what you typed is back in the box otherwise.
        if !matches!(parsed, Some(Ok(_)))
            && let Some(skill) = crate::skills::not_callable(line, agent.skills()).cloned()
        {
            match crate::skills::call_anyway(agent, terminal, &setup.root, setup.skill_room(&agent.model_name), &skill).await {
                Ok(true) => parsed = None,
                Ok(false) => {
                    terminal.input.insert(line);
                    continue;
                }
                Err(e) => {
                    say!("{pad}{}", terminal.err.red(&format!("✗ {e}")));
                    terminal.input.insert(line);
                    continue;
                }
            }
        }
        if parsed.is_none() {
            for (skill, given) in crate::skills::called(line, agent.skills()) {
                agent.call_skill(&skill, given);
            }
        }
        match parsed {
            Some(Ok(Slash::Exit)) => break,
            Some(parsed) => {
                // A line that may hold a key is shown again without it.
                if terminal.input.pinned() {
                    say!("\n{}", you_said(&crate::commands::shown(line), terminal.err, terminal.wrap_width().unwrap_or(80)));
                }
                let done = match parsed {
                    Ok(command) => run_command(command, agent, terminal, setup).await,
                    Err(refused) => Err(refused.to_string()),
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
                let outcome = turn(agent, Start::You(line.to_owned()), terminal).await;
                crate::skills::count(agent.take_loaded());
                // A task that came in during the turn gets its result.
                crate::peers::talk::report(agent, terminal, outcome.as_ref().ok()).await;
                crate::peers::talk::close_finished(terminal).await;
                if let Err(e) = outcome {
                    say!("{pad}{}", terminal.err.red(&format!("✗ {e}")));
                }
            }
        }
    }
    agent.session_ended(rein_agent::hooks::Ended::Exit, terminal).await;
    // Everything was saved as it went: say how to come back to it.
    let talked = agent.full().iter().skip(1).any(|item| matches!(item, Item::User(_)));
    if talked && terminal.fancy {
        say!("\n{}", terminal.err.dim("rein -c continues this conversation"));
    }
    Ok(())
}

/// A sub-agent whose rein is done with it goes, its tab with it, unless
/// you've typed into it: then it says so, and stays.
fn leaves(terminal: &mut Terminal) -> bool {
    if !std::mem::take(&mut terminal.closing) {
        return false;
    }
    if terminal.adopted {
        say!("{}{}", " ".repeat(GUTTER), terminal.err.dim("(the rein that started this one is done with it; it's yours now)"));
        return false;
    }
    say!("{}{}", " ".repeat(GUTTER), terminal.err.dim("(its result was read: this sub-agent is done)"));
    true
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
        Slash::Mode(None) | Slash::Sandbox(None) | Slash::Model(None) | Slash::Login(None) | Slash::Logout(None) | Slash::Effort(None) => {}
        Slash::Mode(Some(mode)) => {
            agent.permissions.mode = mode;
            agent.step_limit = step_limit(mode, false, setup.max_steps);
            terminal.mode = mode;
            if let Some(peers) = &terminal.peers {
                peers.follow_mode(mode);
            }
            say!("{pad}{} {}", s.green("✓"), crate::modes::description(mode, terminal.sandboxing));
            // The bar says so while it asks whether to keep it.
            terminal.input.set_bar(terminal.bar());
            crate::modes::keep_mode(mode, terminal).await?;
        }
        Slash::Sandbox(Some(on)) => {
            agent.permissions.set_sandbox(on);
            terminal.sandboxing = agent.permissions.sandboxing();
            say!("{pad}{} {}", s.green("✓"), crate::modes::sandbox_now(terminal.sandboxing, agent.permissions.sandbox()));
            // What the mode does now, as it depends on it.
            say!("{pad}{}", s.dim(crate::modes::description(agent.permissions.mode, terminal.sandboxing)));
            terminal.input.set_bar(terminal.bar());
            crate::modes::keep_sandbox(on, terminal).await?;
        }
        Slash::Permissions(rules) => crate::rules::command(rules, agent, terminal).await?,
        Slash::Model(Some(spec)) => {
            let shown = use_model(&spec, agent, terminal, setup).await?;
            // Searching as rein decides follows the model in use.
            if *setup.search() == crate::search::SearchWith::Auto {
                crate::search::follow(agent, setup).await;
            }
            if let Some(peers) = &terminal.peers {
                peers.set_model(&terminal.model);
            }
            let current = setup.current(agent);
            terminal.spec.clone_from(&current);
            crate::ui::logo::update(|facts| facts.model = Some(crate::ui::logo::model_name(&current)));
            agent.save_model(&current).map_err(describe)?;
            say!("{pad}{} now using {shown}", s.green("✓"));
            if let Some(why) = crate::effort::unlike_chosen(agent) {
                say!("{pad}{}", s.dim(&format!("({why})")));
            }
            say!("{pad}{}", s.dim("The prompt cache starts over: it's kept per model."));
        }
        Slash::Login(Some(name)) => login_here(&name, None, agent, terminal, setup).await?,
        Slash::LoginWith(name, key) => login_here(&name, Some(key), agent, terminal, setup).await?,
        Slash::Logout(Some(name)) => {
            crate::connect::logout(&name, &|line| say!("{pad}{line}"))?;
            setup.keys = crate::connect::load_keys(&config_dir()?)?;
            setup.others = None;
        }
        Slash::Effort(Some(setting)) => {
            agent.reasoning_effort = setting.level();
            terminal.effort = crate::effort::now(agent);
            let chosen = match agent.reasoning_effort {
                Some(level) => level.to_string(),
                None => crate::effort::words(agent),
            };
            let why = crate::effort::unlike_chosen(agent).map(|why| s.dim(&format!(" · {why}"))).unwrap_or_default();
            say!("{pad}{} reasoning effort: {chosen}{why}", s.green("✓"));
        }
        Slash::Status => {
            let header = agent.session().map(|session| session.header().clone());
            let messages = agent.full().iter().skip(1).filter(|item| matches!(item, Item::User(_))).count();
            let rows = [
                ("model", terminal.model.clone()),
                ("provider", format!("{} · {}", setup.provider.title, agent.model.client.base_url())),
                ("effort", crate::effort::words(agent)),
                ("mode", crate::modes::description(agent.permissions.mode, terminal.sandboxing).to_owned()),
                ("sandbox", crate::modes::sandbox_status(terminal.sandboxing, agent.permissions.sandbox())),
                (
                    "steps",
                    match (agent.step_limit, setup.max_steps) {
                        (Some(limit), _) => format!("at most {limit} a turn, as nobody's watching (auto mode)"),
                        (None, Some(limit)) => format!("no limit while you watch; {limit} in auto mode"),
                        (None, None) => "no limit".to_owned(),
                    },
                ),
                (
                    "rules",
                    format!("{} of yours, {} from .rein/permissions · /permissions", agent.permissions.rule_count(Whose::Yours), agent.permissions.rule_count(Whose::Project)),
                ),
                ("context", format!("about {} of {} tokens before compacting", thousands(agent.estimated()), thousands(agent.limit()))),
                ("limits", terminal.meters.status(std::time::SystemTime::now())),
                ("sent", traffic(agent.model.client.traffic())),
                ("started", header.map(|h| format!("{} · {messages} message{} so far", h.started, if messages == 1 { "" } else { "s" })).unwrap_or_default()),
                ("folder", setup.root.display().to_string()),
            ];
            for (name, value) in rows {
                say!("{pad}{}  {value}", s.dim(&format!("{name:<8}")));
            }
        }
        Slash::Mcp(command) => crate::mcp::command(command, &mut setup.mcp, agent, terminal).await?,
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
        Slash::Undo => crate::undo::command(agent, terminal).await?,
        Slash::Skills => crate::skills::command(agent, terminal, &setup.root, setup.skill_room(&agent.model_name)).await?,
        Slash::Hooks => crate::hooks::command(agent, terminal, &config_dir()?).await?,
        Slash::Memory => crate::memory::command(agent, terminal).await?,
        Slash::Tasks => crate::tasks::command(agent, terminal).await?,
        Slash::Search(with) => crate::search::command(with, agent, terminal, setup).await?,
        Slash::New => {
            let (loaded, context) = setup.open(None, &setup.current(agent))?;
            agent.session_ended(rein_agent::hooks::Ended::Clear, terminal).await;
            agent.resume(loaded, context);
            agent.session_started(rein_agent::hooks::Source::Clear, terminal).await;
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
            agent.session_ended(rein_agent::hooks::Ended::Resume, terminal).await;
            agent.resume(loaded, context);
            agent.session_started(rein_agent::hooks::Source::Resume, terminal).await;
            (terminal.context_used, terminal.last_cache) = (agent.estimated(), None);
        }
        peers @ (Slash::Peers | Slash::Send(..) | Slash::Delegate(..) | Slash::Inbox | Slash::Spawn(_) | Slash::Name(_) | Slash::Workspace(_)) => {
            crate::peers::talk::command(peers, agent, terminal).await?
        }
        Slash::Exit => {}
    }
    Ok(())
}

/// What `command` is, once its choice is made: a command given no choice
/// (`/model`, `/login`, `/mode`...) shows what there is to choose from.
/// `None` when you change your mind.
pub(crate) async fn choose(command: Slash, agent: &Agent<Llm>, terminal: &mut Terminal, setup: &mut Setup) -> CliResult<Option<Slash>> {
    let menu = |title: &str, items: Vec<(String, String)>, current: Option<usize>| Menu { title: title.to_owned(), items, current, at: None };
    Ok(match command {
        Slash::Mode(None) => {
            let modes = Mode::ALL;
            // "edits: file changes ...": the name is the item's already.
            let about = |m: Mode| {
                let description = crate::modes::description(m, terminal.sandboxing);
                description.split_once(": ").map_or(description, |(_, about)| about).to_owned()
            };
            let items = modes.iter().map(|&m| (m.to_string(), about(m))).collect();
            let current = modes.iter().position(|&m| m == agent.permissions.mode);
            terminal.input.choose(menu("what runs without asking", items, current)).await.map(|i| Slash::Mode(Some(modes[i])))
        }
        Slash::Sandbox(None) => {
            let sandbox = agent.permissions.sandbox();
            let on = if sandbox.available() {
                format!("commands run in it, within what each says it needs: {}", sandbox.describe())
            } else {
                format!("{}: commands would ask first, as there are no walls to hold them", sandbox.describe())
            };
            let items = vec![("on".to_owned(), on), ("off".to_owned(), "commands run as you'd run them, and the mode decides what asks".to_owned())];
            let current = Some(if terminal.sandboxing == Sandboxing::Off { 1 } else { 0 });
            terminal.input.choose(menu("the sandbox, in any mode", items, current)).await.map(|i| Slash::Sandbox(Some(i == 0)))
        }
        Slash::Effort(None) => match crate::effort::choices(agent) {
            Some(choices) => {
                let title = format!("how hard {} thinks", agent.model_name);
                let chosen = terminal.input.choose(menu(&title, choices.items, choices.current)).await;
                chosen.map(|i| Slash::Effort(Some(choices.levels[i].map_or(EffortSetting::Default, EffortSetting::Level))))
            }
            None => {
                let none = format!("{} takes no effort setting: it thinks as it will, or not at all", agent.model_name);
                say!("{}{}", " ".repeat(GUTTER), terminal.err.dim(&none));
                None
            }
        },
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
                add(&setup.provider, &[ModelInfo::named(&agent.model_name)]);
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
            let mut items =
                vec![("chatgpt".to_owned(), if signed_in { "signed in: your ChatGPT plan pays for OpenAI's models" } else { "your ChatGPT plan, for OpenAI's models" }.to_owned())];
            for provider in setup.providers.iter().filter(|p| p.needs_key) {
                names.push(provider.name.clone());
                let how = if provider.sign_in_url.is_some() { ", signs in in the browser" } else { "" };
                let description = if provider.name == "claude-code" {
                    "Claude Code subscription: CLI login or setup-token".to_owned()
                } else {
                    format!("{}{how}", crate::connect::pays(provider, &setup.keys, false))
                };
                items.push((provider.name.clone(), description));
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
            if config_dir()?.join(crate::claude::CODE_FILE).exists() {
                names.push("claude-code".into());
            }
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
pub(crate) async fn login_here(name: &str, given: Option<crate::commands::Key>, agent: &mut Agent<Llm>, terminal: &mut Terminal, setup: &mut Setup) -> CliResult {
    let pad = " ".repeat(GUTTER);
    let s = terminal.err;
    let tell = |text: &str| {
        for line in text.trim_matches('\n').lines() {
            say!("{pad}{line}");
        }
    };
    let cancelled = || say!("{pad}{}", s.dim("(cancelled; nothing was saved)"));
    let signed_in = if name == "chatgpt" {
        if given.is_some() {
            return Err("ChatGPT is signed in to in the browser: /login chatgpt takes no key".into());
        }
        if waiting(terminal, "Waiting for you to sign in to ChatGPT in the browser", login(&setup.http, &tell)).await?.is_none() {
            cancelled();
            return Ok(());
        }
        "openai"
    } else {
        let provider = crate::connect::find(&setup.providers, name)?.clone();
        // A key typed after the name is checked and saved as one pasted is.
        let (key, made) = match given {
            Some(key) => (Some(key.into_inner()), false),
            None => {
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
                (key, made.is_some())
            }
        };
        let Some(key) = key else {
            cancelled();
            return Ok(());
        };
        crate::connect::save_key(&setup.http, &provider, key, made, &tell).await?;
        name
    };
    setup.keys = crate::connect::load_keys(&config_dir()?)?;
    setup.others = None;
    if setup.provider.name == signed_in {
        let connected = crate::connect::connect(&setup.http, &setup.providers, &setup.keys, &setup.current(agent), setup.compress).await?;
        say!("{pad}{}", s.dim(&connected.pays));
        let (client, models) = connected.models().await;
        agent.model.client = client;
        // Another sign-in may be another account's.
        terminal.meters.forget();
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
        say_pays(&connected.pays, true);
        let (client, models) = connected.models().await;
        if missing(&models) {
            return Err(format!("{} has no model called {model}; `rein models {}` lists them", provider.title, provider.name));
        }
        agent.model.client = client;
        (setup.provider, setup.models) = (provider, models);
        // The others are others now.
        setup.others = None;
        terminal.meters.forget();
    } else if missing(&setup.models) {
        return Err(format!("no model called {model}; /model lists them"));
    }
    agent.set_limit(setup.limit(&model)?);
    terminal.context_limit = agent.limit();
    agent.max_output = setup.max_output(&model);
    agent.efforts = setup.efforts(&model);
    agent.finds_tools = setup.finds_tools(&model);
    agent.sees = setup.sees(&model);
    terminal.model = provider::shown(&setup.provider, &model);
    agent.model_name = model;
    terminal.effort = crate::effort::now(agent);
    Ok(terminal.model.clone())
}

/// This computer's sandbox for the project at `root`, with rein's own
/// folder `rein` among what's hidden from commands: found whether it's on
/// or not, so that `/sandbox` can turn it on.
async fn find_sandbox(root: &Path, rein: &Path) -> Sandbox {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    Sandbox::detect(Places::new(root, home.as_deref(), Some(rein))).await
}

/// The mode plus your rules, from rein's folder `dir`, and the project's
/// `.rein/permissions`, if it has one, deciding with `sandbox` if it's
/// `on`. Says what it's using whenever that's anything other than the
/// plain default, when commands can't be sandboxed, and what rein does the
/// first time.
pub(crate) fn load_permissions(root: &Path, dir: &Path, mode: Mode, on: bool, sandbox: sandbox::Shared) -> CliResult<Permissions> {
    let path = root.join(".rein").join("permissions");
    let rules = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("can't read .rein/permissions: {e}")),
    };
    let mut permissions = Permissions::new(mode, &rules).map_err(|e| format!(".rein/permissions {e}"))?.with_sandbox(sandbox);
    permissions.set_sandbox(on);
    let sandboxing = permissions.sandboxing();
    // Turned on where there's none: said, as commands ask now.
    if sandboxing == Sandboxing::Missing {
        match permissions.sandbox().missing_why() {
            Some(Missing::Failed(said)) => say!("Sandbox: it didn't start ({said}), so commands ask before they run"),
            _ => say!("Sandbox: none on this system yet, so commands ask before they run"),
        }
    }
    let (yours, first_time) = crate::rules::load(dir)?;
    permissions.set_yours(&yours).map_err(|e| format!("{} {e}", crate::rules::shown(&dir.join(crate::rules::FILE))))?;
    if first_time {
        say!("{}", crate::rules::first_time(mode == Mode::Trust));
    }
    let count = permissions.rule_count(Whose::Project);
    if count > 0 {
        say!("Using {count} rule{} from .rein/permissions", if count == 1 { "" } else { "s" });
    }
    if mode != Mode::default() {
        let description = crate::modes::description(mode, sandboxing);
        let (name, about) = description.split_once(": ").unwrap_or((description, ""));
        say!("Mode: {name} ({about})");
    }
    Ok(permissions)
}

/// What you did at the prompt.
pub(crate) enum Said {
    Message(String),
    /// Another rein sent something while you hadn't started typing.
    Peer(crate::peers::node::Incoming),
    /// A command in the background ended, while you hadn't started typing.
    Ended,
    /// Shift-Tab: on to the next mode.
    NextMode,
    /// Ctrl-V: an image from the clipboard.
    Paste,
    /// Ctrl-D, or Ctrl-C twice.
    Leave,
}

/// The next thing another rein sends; never, for a rein not among them.
async fn from_peers(peers: &mut Option<crate::peers::Node>) -> crate::peers::node::Incoming {
    match peers {
        Some(peers) => match peers.inbox.recv().await {
            Some(incoming) => incoming,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

/// Reads your next message. A line ending in `\` continues on the next.
/// Ctrl-C clears what you typed; a second one within two seconds, or
/// Ctrl-D, ends the conversation.
pub(crate) async fn message(terminal: &mut Terminal, jobs: &rein_agent::background::Jobs, last_interrupt: &mut Option<std::time::Instant>) -> Said {
    let mut text = String::new();
    loop {
        let prompt = if text.is_empty() { "you    " } else { "…      " };
        // What another rein sends is taken in while nothing's half typed
        // over several lines; a message under way in the box stays there.
        let waiting = text.is_empty();
        let read = tokio::select! {
            read = terminal.input.read(prompt, true) => Some(read),
            // Without an editor, Ctrl-C is a signal; the terminal has
            // already thrown away what was typed.
            _ = tokio::signal::ctrl_c() => Some(Read::Interrupted),
            incoming = from_peers(&mut terminal.peers), if waiting => {
                terminal.input.stop_reading();
                return Said::Peer(incoming);
            }
            () = jobs.ended(), if waiting => {
                terminal.input.stop_reading();
                return Said::Ended;
            }
            // The box keeps what's typed, and reading picks up where it was.
            meters = terminal.meters.answered() => {
                terminal.take_meters(meters);
                None
            }
        };
        let Some(read) = read else { continue };
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
            Read::Paste => return Said::Paste,
        }
    }
}
