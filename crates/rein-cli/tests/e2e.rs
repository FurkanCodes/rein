//! rein against real providers: the real binary, your real keys, a few
//! tiny requests each. Off unless you ask, since it costs a little and needs
//! the network:
//!
//! ```sh
//! REIN_E2E=1 cargo test --test e2e -- --nocapture --test-threads 1
//! ```
//!
//! A provider rein has nothing to pay with is skipped, and says so; Ollama
//! is used when it's running. Each run works in a folder of its own, and
//! the conversations it leaves are deleted afterwards. Choose the models
//! with `REIN_E2E_<PROVIDER>_MODEL`, like `REIN_E2E_ANTHROPIC_MODEL`.
//! OpenRouter gets one of its free models unless you choose, and Gemini's
//! free tier and Ollama cost nothing either.

use std::path::{Path, PathBuf};
use std::process::Command;

use rein_llm::serde_json::{self, Value};

const WORD: &str = "kestrel-7031";

fn enabled() -> bool {
    let on = std::env::var("REIN_E2E").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("skipped: set REIN_E2E=1 to run rein against real providers");
    }
    on
}

/// The providers tried, and a cheap model of each that calls tools.
const PROVIDERS: [(&str, &str); 5] = [
    ("openai", "gpt-6.1-sol"),
    ("anthropic", "claude-haiku-5-5"),
    ("gemini", "gemini-3.8-flash"),
    // A free one from its list, when it can be had; see below.
    ("openrouter", "openai/gpt-oss-120b"),
    ("ollama", "qwen3:1.7b"),
];

/// One of OpenRouter's free models that calls tools, from its public list,
/// so a free account costs nothing.
fn free_openrouter_model() -> Option<String> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
    let list: Value = runtime.block_on(async { rein_llm::reqwest::get("https://openrouter.ai/api/v1/models").await.ok()?.json().await.ok() })?;
    let free = list["data"].as_array()?.iter().find(|m| {
        let tools = m["supported_parameters"].as_array().is_some_and(|p| p.iter().any(|x| x == "tools"));
        m["id"].as_str().is_some_and(|id| id.ends_with(":free")) && tools
    })?;
    free["id"].as_str().map(str::to_owned)
}

/// `provider/model` for each provider rein can pay (or that's running, for
/// Ollama), and why the others were skipped.
fn usable() -> Vec<String> {
    let listed = String::from_utf8(Command::new(env!("CARGO_BIN_EXE_rein")).arg("providers").output().unwrap().stdout).unwrap();
    let mut specs = Vec::new();
    for (name, model) in PROVIDERS {
        let chosen = std::env::var(format!("REIN_E2E_{}_MODEL", name.to_uppercase())).ok();
        let model = match chosen {
            Some(model) => model,
            None if name == "openrouter" => free_openrouter_model().unwrap_or_else(|| model.to_owned()),
            None => model.to_owned(),
        };
        let line = listed.lines().find(|l| l.split_whitespace().next() == Some(name)).unwrap_or_default();
        let ready = if name == "ollama" { std::net::TcpStream::connect("127.0.0.1:11434").is_ok() } else { !line.contains("no key yet") };
        if ready {
            specs.push(format!("{name}/{model}"));
        } else {
            eprintln!("skipped {name}: {}", if name == "ollama" { "not running" } else { "no key" });
        }
    }
    specs
}

/// A project folder with a note to read and a big file to compact, and the
/// conversations rein saves for it, both gone when it's dropped.
struct Project {
    dir: PathBuf,
}

impl Project {
    fn new(name: &str) -> Project {
        let dir = std::env::temp_dir().join(format!("rein-e2e-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        std::fs::write(dir.join("note.txt"), format!("The word is: {WORD}\n")).unwrap();
        let line = "lorem ipsum dolor sit amet ".repeat(30);
        let big: String = (1..=60).map(|i| format!("{i:03} {line}\n")).collect();
        std::fs::write(dir.join("big.txt"), big).unwrap();
        Project { dir }
    }

    /// rein, one task, in this project. Its output, said and printed.
    fn rein(&self, args: &[&str], env: &[(&str, &str)]) -> (bool, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_rein"))
            .args(["--effort", "low", "--mode", "read-only"])
            .args(args)
            .current_dir(&self.dir)
            .env("BROWSER", "false")
            .envs(env.iter().copied())
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        eprintln!("$ rein {}\n{said}", args.join(" "));
        (out.status.success(), said)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        // Where rein keeps this folder's conversations; see `session::folder`.
        let home = std::env::var_os("REIN_HOME").map(PathBuf::from).unwrap_or_else(|| std::env::home_dir().unwrap().join(".config/rein"));
        let name: String = self.dir.to_string_lossy().chars().map(|c| if matches!(c, '/' | '\\' | ':') { '-' } else { c }).collect();
        let _ = std::fs::remove_dir_all(home.join("sessions").join(name));
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The usage of every answer in a stream rein recorded with `--record`, as
/// (input, cached) tokens, in each format's words.
fn usages(record: &Path) -> Vec<(u64, u64)> {
    let text = std::fs::read_to_string(record).unwrap_or_default();
    let events = text.lines().filter_map(|line| line.strip_prefix("data:")).filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok());
    events
        .filter_map(|v| {
            let n = |v: &Value| v.as_u64().unwrap_or(0);
            if v["type"] == "message_start" {
                let u = &v["message"]["usage"];
                Some((n(&u["input_tokens"]) + n(&u["cache_creation_input_tokens"]) + n(&u["cache_read_input_tokens"]), n(&u["cache_read_input_tokens"])))
            } else if v["type"] == "response.completed" {
                let u = &v["response"]["usage"];
                Some((n(&u["input_tokens"]), n(&u["input_tokens_details"]["cached_tokens"])))
            } else if v["usage"]["prompt_tokens"].is_u64() {
                Some((n(&v["usage"]["prompt_tokens"]), n(&v["usage"]["prompt_tokens_details"]["cached_tokens"])))
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn every_provider_answers_and_calls_a_tool() {
    if !enabled() {
        return;
    }
    for spec in usable() {
        let project = Project::new(&spec.replace(['/', ':', '.'], "-"));
        let record = project.dir.join("stream.sse");
        let task = "Use the read tool to read note.txt, then reply with only the word written in it.";
        let (ok, said) = project.rein(&["-m", &spec, "--record", record.to_str().unwrap(), task], &[]);
        assert!(ok, "{spec} failed:\n{said}");
        assert!(said.contains("Read") && said.contains(WORD), "{spec} didn't read the note:\n{said}");
        let compressed = !said.contains("doesn't take compressed requests");
        let usages = usages(&record);
        eprintln!("{spec}: gzip {}, usage per request (input, cached): {usages:?}", if compressed { "taken" } else { "refused" });
        if spec.starts_with("anthropic/") {
            // The second request starts like the first: Claude's cache has it.
            assert!(usages.len() >= 2 && usages[1].1 > 0, "{spec}: the second request read nothing from the cache: {usages:?}");
        }
    }
}

#[test]
fn a_conversation_moves_between_real_providers() {
    if !enabled() {
        return;
    }
    let specs = usable();
    let [first, second, ..] = specs.as_slice() else {
        eprintln!("skipped: needs two providers");
        return;
    };
    let project = Project::new("switch");
    let (ok, said) = project.rein(&["-m", first, "Use the read tool to read note.txt, then reply with only the word in it."], &[]);
    assert!(ok && said.contains(WORD), "{first}:\n{said}");
    let (ok, said) = project.rein(&["-c", "-m", second, "Without reading anything, what was the word in note.txt? Only the word."], &[]);
    assert!(ok && said.contains(WORD), "{second} didn't get the conversation from {first}:\n{said}");
}

#[test]
fn claude_takes_the_thinking_rein_sends_back_through_a_compaction() {
    if !enabled() {
        return;
    }
    let Some(spec) = usable().into_iter().find(|s| s.starts_with("anthropic/")) else {
        eprintln!("skipped: no Anthropic key");
        return;
    };
    let project = Project::new("claude-compact");
    let (ok, said) = project.rein(&["-m", &spec, "Read big.txt with the read tool, then reply with only its first three characters."], &[]);
    assert!(ok, "{said}");
    // Small enough that the next message compacts first: the old read is
    // retired, or the model summarizes, and Claude's thinking from after
    // that point has to go for Anthropic to take the request.
    let tight = [("REIN_COMPACT_AT", "6000")];
    let (ok, said) = project.rein(&["-c", "Read big.txt again, then reply with only its last three characters before the newline."], &tight);
    assert!(ok && said.contains("compacted"), "{said}");
    let (ok, said) = project.rein(&["-c", "Reply with only the word: done"], &tight);
    assert!(ok && said.to_lowercase().contains("done"), "{said}");
}
