//! Opening a tab for a sub-agent, in whatever terminal rein runs in.
//!
//! No escape sequence asks a terminal for a new tab, so each one has its own
//! way: tmux and kitty and WezTerm a command, iTerm2, Ghostty and Terminal
//! an AppleScript (macOS asks you once whether that's allowed). Which one
//! rein is in is read from what each sets in the environment, checking a
//! multiplexer first: inside tmux, the terminal around it still says it's
//! there, but a tab of its own would be outside tmux.
//!
//! The new tab runs a short script, never anything typed into it: it starts
//! `rein --child <pid>:<id>`, and the sub-agent fetches its task from this
//! rein over the peers socket. Nothing of the task is on a command line.
//! Where no tab can be opened, the command is shown and copied, for you to
//! paste into one.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How a tab gets opened here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Opener {
    /// The `tab_command` setting: your own command, with `{command}` and
    /// `{title}` in it.
    Yours(String),
    Tmux,
    Zellij,
    /// GNU screen.
    Screen,
    Kitty,
    /// The pane rein is in, to go back to it.
    WezTerm(String),
    ITerm,
    /// Ghostty on macOS, by AppleScript.
    Ghostty,
    /// Ghostty on Linux, by its own command (1.4 and later).
    GhosttyLinux,
    /// Terminal.app: a window, as it has no command for a tab.
    Terminal,
    /// Windows Terminal, from WSL, in this distribution.
    WindowsTerminal(String),
    GnomeTerminal,
    Konsole,
    /// A window: Alacritty has no tabs.
    Alacritty,
    /// A window of foot's server: foot has no tabs.
    Foot,
    /// None: you're shown the command to paste into a new tab.
    Paste,
}

impl Opener {
    /// What `env` says rein is running in. Never a guess: a terminal that
    /// isn't recognised for sure gets [`Opener::Paste`].
    pub(crate) fn detect(env: &dyn Fn(&str) -> Option<String>, tab_command: Option<&str>) -> Opener {
        if let Some(command) = tab_command.filter(|c| c.contains("{command}")) {
            return Opener::Yours(command.to_owned());
        }
        let set = |name: &str| env(name).is_some_and(|value| !value.is_empty());
        // A multiplexer first: the terminal around it is out of reach.
        if set("TMUX") {
            return Opener::Tmux;
        }
        if set("ZELLIJ") {
            return Opener::Zellij;
        }
        if set("STY") {
            return Opener::Screen;
        }
        // Over ssh, the terminal is on another computer.
        if set("SSH_CONNECTION") || set("SSH_TTY") {
            return Opener::Paste;
        }
        if set("KITTY_WINDOW_ID") {
            return Opener::Kitty;
        }
        if let Some(pane) = env("WEZTERM_PANE").filter(|pane| !pane.is_empty()) {
            return Opener::WezTerm(pane);
        }
        if set("WT_SESSION")
            && let Some(distro) = env("WSL_DISTRO_NAME").filter(|d| !d.is_empty())
        {
            return Opener::WindowsTerminal(distro);
        }
        match env("TERM_PROGRAM").as_deref() {
            Some("iTerm.app") if set("ITERM_SESSION_ID") => return Opener::ITerm,
            Some("ghostty") if cfg!(target_os = "macos") => return Opener::Ghostty,
            Some("ghostty") => return Opener::GhosttyLinux,
            Some("Apple_Terminal") => return Opener::Terminal,
            _ => {}
        }
        if set("KONSOLE_VERSION") {
            return Opener::Konsole;
        }
        if set("GNOME_TERMINAL_SCREEN") || set("GNOME_TERMINAL_SERVICE") {
            return Opener::GnomeTerminal;
        }
        if set("ALACRITTY_WINDOW_ID") || set("ALACRITTY_SOCKET") {
            return Opener::Alacritty;
        }
        if env("TERM").is_some_and(|term| term == "foot" || term.starts_with("foot-")) {
            return Opener::Foot;
        }
        Opener::Paste
    }

    /// What it's called, for saying where the sub-agent went.
    pub(crate) fn place(&self) -> &'static str {
        match self {
            Opener::Yours(_) => "a new tab (tab_command)",
            Opener::Tmux => "a new tmux window",
            Opener::Zellij => "a new zellij tab",
            Opener::Screen => "a new screen window",
            Opener::Kitty => "a new kitty tab",
            Opener::WezTerm(_) => "a new WezTerm tab",
            Opener::ITerm => "a new iTerm2 tab",
            Opener::Ghostty | Opener::GhosttyLinux => "a new Ghostty tab",
            Opener::Terminal => "a new Terminal window",
            Opener::WindowsTerminal(_) => "a new Windows Terminal tab",
            Opener::GnomeTerminal => "a new GNOME Terminal tab",
            Opener::Konsole => "a new Konsole tab",
            Opener::Alacritty => "a new Alacritty window",
            Opener::Foot => "a new foot window",
            Opener::Paste => "a tab you open",
        }
    }
}

/// `text` as one word for `sh`, quoted so nothing in it is read.
pub(crate) fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// `text` inside an AppleScript string.
fn apple(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The script the new tab runs: into `cwd`, then rein, with `env`. It
/// removes itself first, as it's of no use after.
pub(crate) fn script(opener: &Opener, cwd: &Path, env: &[(String, String)], argv: &[String]) -> String {
    let env: String = env.iter().map(|(key, value)| format!(" {key}={}", quoted(value))).collect();
    let argv: Vec<String> = argv.iter().map(|arg| quoted(arg)).collect();
    let run = format!("env{env} {}", argv.join(" "));
    let start = format!("#!/bin/sh\nrm -f \"$0\"\ncd {} || exit 1\n", quoted(&cwd.to_string_lossy()));
    match opener {
        // Terminal keeps a window whose program has ended: once rein has,
        // the window is closed, found by its terminal, a moment later, when
        // nothing runs in it any more to ask about.
        // `here` goes to the closer in its environment: a shell variable
        // alone isn't seen by the shell it starts.
        Opener::Terminal => format!("{start}{run}\nnohup env here=\"$(tty)\" sh -c {} >/dev/null 2>&1 &\n", quoted(&terminal_closer())),
        _ => format!("{start}exec {run}\n"),
    }
}

/// What closes a Terminal window once its sub-agent is done: a second
/// later, the window whose terminal is `$here`.
fn terminal_closer() -> String {
    "sleep 1; osascript -e \"tell application \\\"Terminal\\\" to close (every window whose tty of selected tab is \\\"$here\\\")\"".to_owned()
}

/// Writes `script` where only you can read it, as a file of its own, and
/// says where.
fn save(folder: &Path, id: &str, script: &str) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let path = folder.join(format!("start-{id}.sh"));
    rein_llm::store::write_private(&path, script.as_bytes()).map_err(|e| format!("can't write {}: {e}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Opens a tab titled `title` running `script`, saved in `folder`. `Ok`
/// with the command for you to paste when this terminal can't.
pub(crate) async fn open(opener: &Opener, folder: &Path, id: &str, title: &str, script: &str) -> Result<Opened, String> {
    let path = save(folder, id, script)?;
    // Quoted only if it must be: iTerm2 and Ghostty take it as one line.
    let file = path.to_string_lossy().into_owned();
    let plain = file.chars().all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c));
    let run = format!("/bin/sh {}", if plain { file.clone() } else { quoted(&file) });
    if *opener == Opener::Paste {
        return Ok(Opened::Paste(run));
    }
    // Each step's output is there for the next, as `{out}`: WezTerm says
    // which pane it opened, to name it.
    let mut out = String::new();
    for step in steps(opener, &file, &run, title) {
        let args: Vec<String> = step.args.iter().map(|arg| arg.replace("{out}", out.trim())).collect();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match program(step.program, &args).await {
            Ok(said) => out = said,
            Err(e) if step.needed => return Err(e),
            Err(_) => {}
        }
    }
    Ok(Opened::Tab)
}

/// One program to run, to open a tab.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Step {
    pub(crate) program: &'static str,
    pub(crate) args: Vec<String>,
    /// Opening failed if this did; otherwise it's a nicety, like a title.
    pub(crate) needed: bool,
}

fn step(program: &'static str, args: &[&str]) -> Step {
    Step { program, args: args.iter().map(|arg| (*arg).to_owned()).collect(), needed: true }
}

fn nicety(program: &'static str, args: &[&str]) -> Step {
    Step { needed: false, ..step(program, args) }
}

/// What `opener` runs to open a tab titled `title` running the script at
/// `file` (`run` is `/bin/sh <file>`, as one line). No shell reads any of
/// it, but for `tab_command`, which is yours.
pub(crate) fn steps(opener: &Opener, file: &str, run: &str, title: &str) -> Vec<Step> {
    match opener {
        Opener::Paste => Vec::new(),
        Opener::Yours(template) => vec![step("sh", &["-c", &template.replace("{command}", run).replace("{title}", &quoted(title))])],
        // In the background: the tab you're in stays in front.
        Opener::Tmux => vec![step("tmux", &["new-window", "-d", "-n", title, run])],
        Opener::Zellij => vec![step("zellij", &["action", "new-tab", "--name", title, "--", "/bin/sh", file]), nicety("zellij", &["action", "go-to-previous-tab"])],
        Opener::Screen => vec![step("screen", &["-X", "screen", "-t", title, "/bin/sh", file]), nicety("screen", &["-X", "other"])],
        // These take their title from the rein in them, which moves and
        // follows /name: a title given here would pin it.
        Opener::Kitty => vec![step("kitten", &["@", "launch", "--type=tab", "--keep-focus", "/bin/sh", file])],
        Opener::WezTerm(back) => vec![step("wezterm", &["cli", "spawn", "--", "/bin/sh", file]), nicety("wezterm", &["cli", "activate-pane", "--pane-id", back])],
        Opener::ITerm => vec![step("osascript", &["-e", &iterm_script(run)])],
        Opener::Ghostty => vec![step("osascript", &["-e", &ghostty_script(run)])],
        Opener::GhosttyLinux => vec![step("ghostty", &["+new-tab", "-e", "/bin/sh", file])],
        // `exec`: your shell doesn't stay on once the sub-agent is done.
        Opener::Terminal => vec![step("osascript", &["-e", &format!("tell application \"Terminal\" to do script {}", apple(&format!("exec {run}")))])],
        Opener::WindowsTerminal(distro) => {
            vec![step("cmd.exe", &["/c", "wt.exe", "-w", "0", "nt", "--title", title, "wsl.exe", "-d", distro, "--", "/bin/sh", file])]
        }
        Opener::GnomeTerminal => vec![step("gnome-terminal", &["--tab", &format!("--title={title}"), "--", "/bin/sh", file])],
        Opener::Konsole => vec![step("konsole", &["--new-tab", "-p", "tabtitle=%w", "-e", "/bin/sh", file])],
        Opener::Alacritty => vec![step("alacritty", &["msg", "create-window", "-T", title, "-e", "/bin/sh", file])],
        Opener::Foot => vec![step("footclient", &["--no-wait", &format!("--title={title}"), "--", "/bin/sh", file])],
    }
}

/// The AppleScript that opens a Ghostty tab running `run` (Ghostty 1.3
/// and later), in its front window, then goes back to the tab you were in.
fn ghostty_script(run: &str) -> String {
    format!(
        "tell application \"Ghostty\"\n\
         set theWindow to front window\n\
         set yourTab to selected tab of theWindow\n\
         set newConfig to new surface configuration\n\
         set command of newConfig to {}\n\
         new tab in theWindow with configuration newConfig\n\
         select tab yourTab\n\
         end tell",
        apple(run)
    )
}

/// The AppleScript that opens an iTerm2 tab running `run`, then goes back to the tab you were in. No variable may be named like a
/// word of AppleScript's own: `back` (as in front and back) once made
/// iTerm2 try to set its "last insertion point", and fail.
fn iterm_script(run: &str) -> String {
    format!(
        "tell application \"iTerm2\"\n\
         set theWindow to current window\n\
         set yourTab to current tab of theWindow\n\
         tell theWindow to set newTab to (create tab with default profile command {})\n\
         tell yourTab to select\n\
         end tell",
        apple(run)
    )
}

/// Renames the window rein opened for this sub-agent, where the
/// multiplexer names windows itself and doesn't take a program's title:
/// tmux and screen. Everywhere else the tab follows rein's own title.
pub(crate) async fn rename_tab(env: &impl Fn(&str) -> Option<String>, name: &str) {
    if let Some(step) = renaming(env, name) {
        let args: Vec<&str> = step.args.iter().map(String::as_str).collect();
        let _ = program(step.program, &args).await;
    }
}

/// What renames the window this rein is in, to `name`, if anything must.
fn renaming(env: &impl Fn(&str) -> Option<String>, name: &str) -> Option<Step> {
    if let Some(pane) = env("TMUX_PANE") {
        Some(step("tmux", &["rename-window", "-t", &pane, name]))
    } else if let (Some(session), Some(window)) = (env("STY"), env("WINDOW")) {
        Some(step("screen", &["-S", &session, "-p", &window, "-X", "title", name]))
    } else {
        None
    }
}

/// macOS opens new windows as tabs: Desktop & Dock's "Prefer tabs when
/// opening documents" is Always. Terminal's own scripting can only open a
/// window, and this makes it a tab.
pub(crate) async fn windows_open_as_tabs() -> bool {
    program("defaults", &["read", "-g", "AppleWindowTabbingMode"]).await.is_ok_and(|mode| mode.trim() == "always")
}

/// How opening went.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Opened {
    Tab,
    /// The command to paste into a new tab, as this terminal can't open one.
    Paste(String),
}

/// What `name args` prints, run without a shell, in ten seconds at most.
async fn program(name: &str, args: &[&str]) -> Result<String, String> {
    let mut command = tokio::process::Command::new(name);
    command.args(args).stdin(std::process::Stdio::null()).kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .map_err(|_| format!("{name} took too long to open the tab"))?
        .map_err(|e| format!("can't run {name}: {e}"))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{name} couldn't open the tab: {}", said.lines().next().unwrap_or("it failed")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        move |name| pairs.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    #[test]
    fn the_terminal_is_read_from_what_it_sets_and_a_multiplexer_comes_first() {
        let detect = |pairs: &[(&str, &str)]| Opener::detect(&env(pairs), None);
        // Inside tmux in iTerm2, iTerm2 still says it's there: tmux it is.
        assert_eq!(detect(&[("TMUX", "/tmp/tmux-501/default,1,0"), ("TERM_PROGRAM", "tmux"), ("ITERM_SESSION_ID", "w0t1p0:x")]), Opener::Tmux);
        assert_eq!(detect(&[("TERM_PROGRAM", "iTerm.app"), ("ITERM_SESSION_ID", "w0t1p0:x")]), Opener::ITerm);
        assert_eq!(detect(&[("TERM_PROGRAM", "iTerm.app")]), Opener::Paste, "iTerm2's own id must be there too");
        assert_eq!(detect(&[("KITTY_WINDOW_ID", "3")]), Opener::Kitty);
        assert_eq!(detect(&[("WEZTERM_PANE", "7"), ("TERM_PROGRAM", "WezTerm")]), Opener::WezTerm("7".into()));
        assert_eq!(detect(&[("TERM_PROGRAM", "Apple_Terminal")]), Opener::Terminal);
        assert_eq!(detect(&[("TERM_PROGRAM", "iTerm.app"), ("ITERM_SESSION_ID", "x"), ("SSH_CONNECTION", "1 2 3 4")]), Opener::Paste, "over ssh, the terminal is elsewhere");
        assert_eq!(detect(&[("TERM_PROGRAM", "vscode")]), Opener::Paste);
        assert_eq!(Opener::detect(&env(&[("TMUX", "x")]), Some("my-term --tab {command}")), Opener::Yours("my-term --tab {command}".into()), "the setting wins");
    }

    #[test]
    fn the_script_quotes_everything_and_carries_no_task() {
        let script = script(
            &Opener::Tmux,
            Path::new("/p/it's here"),
            &[("REIN_HOME".into(), "/h".into())],
            &["/bin/rein".into(), "--name".into(), "tests".into(), "--child".into(), "12:ab".into()],
        );
        assert!(script.contains("cd '/p/it'\\''s here' || exit 1"), "{script}");
        assert!(script.ends_with("exec env REIN_HOME='/h' '/bin/rein' '--name' 'tests' '--child' '12:ab'\n"), "{script}");
        assert!(script.contains("rm -f \"$0\""), "it goes once it has run");
        assert_eq!(apple("say \"hi\" \\ there"), "\"say \\\"hi\\\" \\\\ there\"");
    }

    #[test]
    fn more_terminals_are_known_by_what_they_set() {
        let detect = |pairs: &[(&str, &str)]| Opener::detect(&env(pairs), None);
        assert_eq!(detect(&[("ZELLIJ", "0"), ("TERM_PROGRAM", "iTerm.app"), ("ITERM_SESSION_ID", "x")]), Opener::Zellij, "a multiplexer first");
        assert_eq!(detect(&[("STY", "123.pts-0")]), Opener::Screen);
        assert_eq!(detect(&[("WT_SESSION", "x"), ("WSL_DISTRO_NAME", "Ubuntu")]), Opener::WindowsTerminal("Ubuntu".into()));
        assert_eq!(detect(&[("WT_SESSION", "x")]), Opener::Paste, "Windows Terminal is reached from WSL");
        assert_eq!(detect(&[("KONSOLE_VERSION", "240802")]), Opener::Konsole);
        assert_eq!(detect(&[("GNOME_TERMINAL_SCREEN", "/org/gnome/Terminal/screen/1")]), Opener::GnomeTerminal);
        assert_eq!(detect(&[("ALACRITTY_WINDOW_ID", "1")]), Opener::Alacritty);
        assert_eq!(detect(&[("TERM", "foot")]), Opener::Foot);
        assert_eq!(detect(&[("TERM", "xterm-256color")]), Opener::Paste);
        if !cfg!(target_os = "macos") {
            assert_eq!(detect(&[("TERM_PROGRAM", "ghostty")]), Opener::GhosttyLinux);
        }
    }

    #[test]
    fn each_terminal_is_given_the_script_and_a_name_never_a_shell_line() {
        let (file, run) = ("/h/peers/start-1.sh", "/bin/sh /h/peers/start-1.sh");
        let first = |opener: Opener| {
            let steps = steps(&opener, file, run, "tests");
            assert!(!steps.is_empty(), "{opener:?}");
            let first = &steps[0];
            (first.program, first.args.clone())
        };
        let words = |list: &[&str]| list.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        assert_eq!(first(Opener::Tmux), ("tmux", words(&["new-window", "-d", "-n", "tests", run])));
        assert_eq!(first(Opener::Zellij), ("zellij", words(&["action", "new-tab", "--name", "tests", "--", "/bin/sh", file])));
        assert_eq!(first(Opener::Screen), ("screen", words(&["-X", "screen", "-t", "tests", "/bin/sh", file])));
        assert_eq!(first(Opener::Kitty), ("kitten", words(&["@", "launch", "--type=tab", "--keep-focus", "/bin/sh", file])));
        assert_eq!(first(Opener::GnomeTerminal), ("gnome-terminal", words(&["--tab", "--title=tests", "--", "/bin/sh", file])));
        assert_eq!(first(Opener::Konsole), ("konsole", words(&["--new-tab", "-p", "tabtitle=%w", "-e", "/bin/sh", file])));
        assert_eq!(first(Opener::GhosttyLinux), ("ghostty", words(&["+new-tab", "-e", "/bin/sh", file])));
        assert_eq!(first(Opener::Alacritty), ("alacritty", words(&["msg", "create-window", "-T", "tests", "-e", "/bin/sh", file])));
        assert_eq!(first(Opener::Foot), ("footclient", words(&["--no-wait", "--title=tests", "--", "/bin/sh", file])));
        let (wt, args) = first(Opener::WindowsTerminal("Ubuntu".into()));
        assert_eq!((wt, &args[..2], args.last().map(String::as_str)), ("cmd.exe", &words(&["/c", "wt.exe"])[..], Some(file)));
        // WezTerm goes back to yours once it's opened one.
        let wez = steps(&Opener::WezTerm("7".into()), file, run, "tests");
        assert_eq!(wez[1].args, words(&["cli", "activate-pane", "--pane-id", "7"]));
        assert!(wez[0].needed && !wez[1].needed, "only opening it must work");
        // None that would take the title from rein is given one to keep.
        for opener in [Opener::Kitty, Opener::WezTerm("7".into()), Opener::ITerm, Opener::Ghostty, Opener::GhosttyLinux, Opener::Konsole, Opener::WindowsTerminal("Ubuntu".into())]
        {
            let all = steps(&opener, file, run, "tests").iter().flat_map(|s| s.args.clone()).collect::<Vec<_>>().join(" ");
            assert!(!all.contains("tab-title") && !all.contains("set name") && !all.contains("suppress") && !all.contains("=tests"), "{opener:?}: {all}");
        }
        let (osa, script) = first(Opener::Ghostty);
        assert!(osa == "osascript" && script[1].contains("set command of newConfig to \"/bin/sh /h/peers/start-1.sh\""), "{script:?}");
        let yours = steps(&Opener::Yours("my-term --tab --title {title} -- {command}".into()), file, run, "tests");
        assert_eq!(yours[0].args, words(&["-c", "my-term --tab --title 'tests' -- /bin/sh /h/peers/start-1.sh"]));
    }

    #[test]
    fn a_terminal_window_closes_itself_once_its_sub_agent_is_done() {
        let argv: Vec<String> = ["/bin/rein", "--name", "tests"].map(str::to_owned).to_vec();
        let started = script(&Opener::Terminal, Path::new("/p"), &[], &argv);
        assert!(!started.contains("exec env"), "rein runs, then the window is closed:\n{started}");
        assert!(started.contains("nohup env here=\"$(tty)\" sh -c"), "{started}");
        // The closer sees which terminal it's for: run as the script runs it,
        // with osascript standing in for an echo.
        let line = started.lines().find(|line| line.starts_with("nohup")).unwrap().replace("nohup ", "").replace(" >/dev/null 2>&1 &", "").replace("$(tty)", "/dev/ttys042");
        let line = line.replace("sleep 1; osascript -e", "echo");
        let out = std::process::Command::new("sh").args(["-c", &line]).output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("is \"/dev/ttys042\""), "the window's terminal reaches it: {}", String::from_utf8_lossy(&out.stdout));
        // It reads as sh, and so does what it runs in the background.
        let parses = |text: &str| std::process::Command::new("sh").args(["-n", "-c", text]).status().unwrap().success();
        assert!(parses(&started), "{started}");
        assert!(parses(&terminal_closer()), "{}", terminal_closer());
        assert!(terminal_closer().contains("close (every window whose tty of selected tab is \\\"$here\\\")"), "{}", terminal_closer());
        assert!(script(&Opener::ITerm, Path::new("/p"), &[], &argv).contains("exec env"), "other terminals close the tab themselves");
    }

    #[test]
    fn a_renamed_sub_agent_renames_its_window_where_the_title_wont() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |key: &str| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| (*v).to_owned());
        let tmux = renaming(&env(&[("TMUX_PANE", "%3")]), "api").map(|s| (s.program, s.args));
        assert_eq!(tmux, Some(("tmux", ["rename-window", "-t", "%3", "api"].map(str::to_owned).to_vec())));
        let screen = renaming(&env(&[("STY", "12.pts"), ("WINDOW", "2")]), "api").map(|s| (s.program, s.args));
        assert_eq!(screen, Some(("screen", ["-S", "12.pts", "-p", "2", "-X", "title", "api"].map(str::to_owned).to_vec())));
        assert!(renaming(&env(&[("TERM_PROGRAM", "iTerm.app")]), "api").is_none(), "elsewhere the title says it");
    }

    #[test]
    fn the_iterm_script_names_nothing_like_a_word_of_applescripts() {
        let script = iterm_script("/bin/sh /p/start-1.sh");
        for word in ["back", "front", "last", "first", "end", "beginning", "it", "me", "result", "window", "tab", "name"] {
            for line in script.lines() {
                assert!(!line.trim_start().starts_with(&format!("set {word} to")), "a variable named {word}: {line}");
            }
        }
        assert!(script.contains("create tab with default profile command \"/bin/sh /p/start-1.sh\""), "{script}");
        assert!(script.contains("tell yourTab to select"), "back to your tab: {script}");
    }

    #[tokio::test]
    async fn where_no_tab_can_be_opened_the_command_is_given_to_paste() {
        let folder = std::env::temp_dir().join(format!("rein-spawn-{}", std::process::id()));
        let opened = open(&Opener::Paste, &folder, "ab12", "tests", "#!/bin/sh\n").await.unwrap();
        let Opened::Paste(command) = opened else { panic!("{opened:?}") };
        assert!(command.starts_with("/bin/sh /") && command.ends_with("start-ab12.sh"), "{command}");
        assert!(folder.join("start-ab12.sh").exists());
        std::fs::remove_dir_all(folder).unwrap();
    }
}
