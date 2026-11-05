# rein: notes for anyone changing it

rein is a terminal coding agent in Rust (edition 2024, tokio `current_thread`).
`crates/rein-llm` speaks to providers (Responses, Messages, Gemini and Chat
Completions APIs), `crates/rein-agent` is the agent loop and its tools,
`crates/rein-cli` is the `rein` binary and its full-screen terminal. The README
says what rein does; this file says how its code is kept.

Every rule below is here because breaking it caused a real bug or a review
finding. Each says why in a line.

## Done means

- `cargo test`, `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`
  all pass. rustfmt is configured (`rustfmt.toml`): long lines on purpose.
- A bug fix comes with a test that fails without the fix. Break the fix on
  purpose and watch the test fail before calling it done; a test that can't fail
  proves nothing.
- Behaviour you can see gets a screen test (`crates/rein-cli/tests/screen.rs`):
  the real binary in a pseudo-terminal, against a fake model server.
- Real providers only through the opt-in suite:
  `cargo test --test e2e -- --ignored --nocapture --test-threads 1`.
- Now and then run `cargo clippy -- -W clippy::pedantic -W clippy::nursery` and
  read what's not style: unused `&mut`, values passed by value but not consumed,
  lock guards held too long, too many bools.

## Types say what's allowed

- A closed set of values is an `enum`, not a `String`. A typo then doesn't
  compile, and a `match` must handle every case. rein had tool names, effort
  levels, error codes and modes as strings; misspellings compiled, and a typo
  quietly became "max effort". See `ToolName`, `Effort`, `ErrorKind`, `Mode`.
- Parse text into the type once, at the edge (flags, settings, the model's tool
  calls, JSON from disk), with `FromStr`/serde. Deep inside, pass the type.
- Wire data stays text where the wire says text (`ToolCall::name` is the model's
  string); read it once with a constructor like `ToolName::of(call)`.
- One variant, one meaning. `Read::Line(String)` once carried typed text, a menu
  index and an answer key; a menu choice is `Read::Picked(usize)`.
- No `Option<Option<T>>`: name the cases (`EffortSetting { Default, Level }`).
- Several bools that can't all be true are one enum
  (`enum Last { Nothing, You, Step, Text }`, not three `after_*` flags).
- A rule about a set lives on the type (`ToolName::only_looks()`), not in two
  lists that must agree by hand.
- A serde enum for a closed set in tool arguments (`GrepOutput`,
  `StepStatus`): a value the model misspells comes back as an error it can fix.

- What a model can do comes from its provider's model list where the list
  says, not from one list for every model: rein once offered the same six
  effort levels to every model, and `minimal` to a model that doesn't take
  it failed the turn. Read it into the type (`Efforts`), and where a list
  says nothing, learn from what the server turns down, per model.
- Try a provider's feature the way people pay for it, not only as its docs
  and its list say: ChatGPT's list says gpt-6.1-sol has OpenAI's tool
  search, and a ChatGPT plan turns it down ("subscription sharing … only
  function and custom tools"). rein's search is a function of its own, and
  what it finds comes as `additional_tools`, which the plan takes.

- How much of your limits is used is the provider's word, never rein's
  guess, and a provider that says nothing shows nothing: pi showed
  "$3.92 (sub)" worked out from a price list, and its users couldn't tell
  whether they'd been billed. A figure informs; only the provider's refusal
  stops anything, as a week at 100% still served Opus in oh-my-pi. Name a
  window by its length, never from a list: a list of the ones it knew made
  a monthly plan vanish from oh-my-pi's bar.

## Errors are values until the edge

- Keep errors typed until `main`, and turn them into text once there
  (`describe`). Never decide anything by searching an error's text: name the
  outcome (`ErrorKind`), or a `const` both sides use (`DECLINED_COMMAND`,
  `INTERRUPTED`, `tools::ERROR`). Compaction once parsed "The user declined" out
  of tool results; rewording the message would have silently broken it.
- Keep the provider's own error code next to rein's kind; don't overwrite it.
- Implement `source()` on error types, and show the chain (`with_causes`):
  "error sending request" alone hides DNS or TLS.

## Panics and arithmetic

- No `unwrap()` outside tests. `expect("…")` only for a real invariant, saying
  which.
- Arithmetic on anything a user or a model sent is checked or saturating, and
  clamping happens before comparing: `@a.rs:0-0` once became lines 1 to 0 and
  `last - first + 1` underflowed.
- Never `expect` on the shape of a `serde_json::Value` built from data you read
  back from disk: a tampered session file must not panic.

## Ownership: borrow what you only read

- A request borrows the conversation (`Request<'a> { input: &'a [Item], .. }`).
  It used to clone the whole history on every request.
- Build once what doesn't change (the tool specs are built when the agent is).
- A "builder" method (`fn with_x(self) -> Self`) changes only its own value,
  never state shared by every clone through an `Arc`.

## Async: cancelling is dropping

- In async Rust a stopped turn is a dropped future. Cleanup belongs in `Drop`,
  not after an `.await` that may never come back.
- `tokio::spawn` detaches: dropping its `JoinHandle` does not stop the task.
  Tasks scoped to an operation go in a `JoinSet`, which stops them all when
  dropped. A cancelled sign-in once kept its callback server listening.
- A stop signal uses `Notify::notify_waiters()`. `notify_one()` stores a permit
  when nobody waits, and the next turn consumed it: Esc during a lookup stopped
  your next message.
- Nothing blocks the runtime's only thread: no `std::process::Command::status()`,
  `std::thread::sleep` or blocking reads in async code. Use `tokio::process`, a
  timeout, or `spawn_blocking`. A browser named in `$BROWSER` once could freeze
  rein until it closed.
- Every network read has a timeout: a stream idle for 5 minutes is given up on,
  a callback connection that sends nothing for 10 seconds is closed.
- Hold a std `Mutex` guard for as short as possible, and never across `.await`.

## Processes

- A command runs in its own process group, held by a guard whose `Drop` kills
  the whole group. Killing `sh` alone left `cargo test` and dev servers running
  after a timeout or Esc. A command that finishes normally disarms the guard, so
  what it put in the background on purpose keeps running.
- Read a child's output with a cap; never `wait_with_output` on something that
  can print gigabytes.

## Security

- Permission checks fail closed: what rein can't parse it doesn't allow on a
  guess. A `deny` rule was dodged by gluing the command to a `;`.
- A rule that asks or refuses looks hard for what it names; one that allows
  matches only exactly. `deny rm` once missed `/bin/rm`, `sudo rm` and
  `sh -c 'rm …'`, while a looser `allow ls` would have let `sudo ls` run.
- Secrets never reach `Debug`: types holding keys or tokens (`Auth`,
  `Credentials`) have a hand-written `Debug` that leaves them out. A derived
  `Debug` prints them into any log or panic message.
- A secret typed in the terminal is never echoed, never kept in history, and the
  terminal is put back even on Ctrl-C.
- Never sign in as another app: no borrowed OAuth client IDs, no spoofed
  headers or system prompts. Official sign-ins and API keys only.
- A token that runs out is asked for before each request, never kept from
  the start: rein kept the ChatGPT token it started with, and every
  conversation stopped after an hour. Renew under a lock every rein window
  shares, and read the file again under it: a refresh token works once, and
  two windows spending the same one sign you out.

## The sandbox

- The walls are yours to choose, with the mode: in trust nothing rein
  starts is walled in (commands, MCP servers), and the other modes wall
  them all, unless one has walls of its own. Every harness whose user
  approves each command ships its sandbox off, none walls in servers
  while commands run free, and walls broke what servers need. What comes
  with a project waits for your yes in every mode, which doesn't depend
  on walls.
- Your own hooks run outside the sandbox in every mode, as Claude Code's
  do: they're your setup, not what the model runs. Walled in, a hook
  couldn't open its own script in `~/.claude/hooks`, Python's exit code
  for that (2) read as a no, and every message was turned away in
  read-only mode. That's safe because what the model runs can't plant
  one: `~/.claude`, rein's folder and a project's `.claude` are out of its
  reach. A project's hooks are someone else's, and wait for your yes.
- A command meant for the sandbox never runs without one: no sandbox means
  ask, not run (`Run::Sandboxed` without one is an error). Claude Code's
  default runs commands unsandboxed when its sandbox can't start.
- Rules allow a list and deny the rest (`(deny default)`), never the other way
  round: a sandbox that denied a list of bad operations was escaped through
  what the list forgot. System services are a short list for the same
  reason; allowing them all let a command reach Launch Services, which opens
  apps outside the sandbox.
- A path rule locks the folders above it too: `mv .git x`, write
  `x/hooks/pre-commit`, `mv x .git` got past a rule on `.git/hooks`.
- Protect what runs later, not only what runs now: hooks, git's config, rule
  files, editors' and agents' settings. A command that plants one escapes on
  your next commit.
- An MCP server at a URL gets a token for its URL alone (`resource`), from
  a sign-in that takes PKCE or none at all, and the token goes only there:
  over https, or plain http to this computer, never across a redirect to
  another server. A header you gave it wins over rein's sign-in.
- An MCP server is a command, someone else's: it runs in the sandbox like
  one, and a project's starts only once you've said yes to all of what it
  runs (command, arguments, environment), never to its name. Cursor went by
  the name, and a later commit swapped the command behind it
  (CVE-2025-54136). What a server says of its tools is its own word: none
  runs without asking for it.
- What a command may write whatever its walls (a tool's records: rtk's
  stats) is a file, never a link or a folder, and never the settings
  beside it: rtk outside the walls writes where those say, and through a
  link planted in place of its stats. Not rtk's `tee` folder either: a
  folder of links moved there could take its next file, whose names can
  be guessed.
- Network access is the internet, never other programs' sockets: with all
  network allowed, a command reached Docker's socket, and Docker can mount
  your home.
- Compare paths as the file system does: macOS ignores case, so `.REIN` is
  `.rein` (the sandbox resolves this itself; rein's own checks lower-case).
- Keys stay out of rein's environment (`secrets::keep`, first in `main`):
  macOS hands any process of yours another's environment (`KERN_PROCARGS2`),
  and a sandboxed command once read rein's key from it. Seatbelt stops that
  only with `process-info` denied in so many words: `(deny default)` doesn't
  count for it, and no `sysctl-read` rule alone blocks the call.
- Seatbelt rules never name syscalls by number, not even in a quick
  experiment: a profile allowing numbers 0 to 599 crashed macOS (its kernel
  takes 560), and the Mac restarted. An older macOS has fewer syscalls, so a
  number that's fine here can crash one there.
- Between fork and exec, only system calls on what's ready: no allocating,
  no locks. The Landlock ruleset is built in rein's process; the command's
  only takes it on.
- An exception to what's hidden is a list of its own, allowed after the
  hidden rule (`SKILLS_IN_HIDDEN`), never a readable place inside a hidden
  one: those are dropped, and `~/.claude/skills` was readable on paper for
  a release while no command could read it. Test it with a command that
  reads it, and one that reads a key beside it.
- Landlock allows whole folders and can't wall off part of one, so on Linux
  what's protected inside the project is guarded (noted, then put back), and
  a rule on `/tmp` reaches a project in it: grant around what's excepted.

## The agent loop

- Stop what's stuck, not what's long. A fixed 25 steps a turn cut off real
  work once the sandbox stopped asking; a loop is the same call giving the
  same result again and again, and that's what's caught. Only a turn nobody
  watches has a step limit, and its last step asks for a report.
- A guard against rein looping on its own work goes by progress, not a
  count: compacting "once per message" overflowed long turns. Compact again
  after a step since the last time, and not after one that made no room.
- What the model is shown changes at the end of a request, never at its
  start: the tool list comes before everything, so a tool added to it
  loses the whole cache. A tool found mid-conversation joins after the
  conversation so far (`additional_tools`, `tool_reference`), and a format
  that can't do that shows every tool from the start.
- A hook's rewrite is decided on as the model's own call, and rein's ask
  and deny rules on the call as the model made it too: rtk turns `git push
  --force` into `rtk git push --force`, and a rewrite deciding alone could
  hide a command from the rules. Hooks on one call run in turn, each given
  the call as the last left it: Claude Code runs them at once, and the
  last to finish wins.
- What the model should do before a task goes in its instructions, not
  only in a tool's description: told of skills only in the `skill` tool,
  a model left one unloaded for a small task that fit it.
- A note rein adds to the conversation isn't something you said: mark it
  with a `const` (`skills::is_note`). Compaction's record lists every
  message as yours, and kept a note of skills changed as your words.
- Check a convention against the files people have before following it:
  Claude Code fills `$1` in any skill called with arguments, and in the
  skills on this Mac every `$1` was money (`$2.4M ARR`) or SQL
  (`VALUES ($1, $2)`). rein fills it only where a skill says it takes some.
- What a tool brings into the conversation is sent again with every
  request after it. A lookup that pulls in pages runs in a request of its
  own and keeps only its answer: OpenAI's web search, run in the
  conversation, added 12,000 tokens to it; in its own request, 780.
- A CLI used only as the model (`claude -p`) is told it has one tool, the
  one it answers with, and that the tools listed are rein's: told only
  "don't run tools yourself", the model called `mcp__linear__list_teams` as
  its own, was told there's no such tool, and often gave up on the call.
  A server's schema isn't held to there either: the CLI turned a whole
  answer away for a null it didn't allow. `--output-format stream-json
  --verbose` shows what the model did, turn by turn.

## Encapsulation

- Fields that must change together are private, behind methods. `Agent`'s
  conversation (history, everything said, the session file, the window) changes
  only through `Agent::new`, `resume`, `send` and `compact`; when it had public
  fields, `/resume` set eight of them by hand, missed one, and `/undo` reverted
  the previous conversation's files.
- A value derived from others is a method, not a field that can drift.

## Files on disk

- Never put an image's bytes in a conversation's file: keep it once, as a
  file named by what's in it, and name the file. Codex kept them inline,
  copied again at each compaction, and session folders reached 165 GB.
- An image that can't be shown becomes a note saying so, never bytes sent
  as they are: one a provider turned away failed every request after it,
  in Claude Code.
- Write files whole: a temp file with a name of its own (process and counter),
  then rename. A fixed `.tmp` name let two rein windows rename each other's
  half-written file into place.
- A copy kept to put back later is checked to be the file that was looked
  at (inode and times), opened never through a symlink: a link swapped in
  would have copied your keys, and `/undo` written them into the project.
  And it's a copy or a clone of its own, never a hard link to the file,
  which an edit in place would change too.
- Measure what a file system costs before building on it: hard links take
  about 200 µs each on APFS, and a pool linked from each conversation made
  the first look at 20,000 files 7 s; clones alone made it 2 s.

## unsafe and FFI

- Every `unsafe` block has a `// SAFETY:` comment saying why it's sound, and is
  as small as it can be.
- A signal handler only calls async-signal-safe functions (`tcsetattr`, `write`,
  `_exit`), and whoever installs one puts back what was there before.
- Check FFI return values (`localtime_r`, `tcgetattr`).

## Structure

- A file over ~800 lines, or a function over ~80, is a sign to split. `main.rs`
  was 2,000 lines; it's now `args`, `conversation`, `terminal` and `format`.
- Code the provider formats share (grouping items by role, call IDs, parsing
  arguments) lives once, not in each format.
- `pub(crate)` unless something outside the crate needs it.
- The sandbox is its own crate, `rein-sandbox`, with no C in its
  dependencies: its tests build for Linux from a Mac and run in a container.
  So is `rein-mcp`, which runs servers in it: it once took `rein-llm` for a
  type, and with it reqwest's C, and stopped building for Linux.

## Tests

- Tests never touch the real environment: a temporary `HOME`/`REIN_HOME`,
  `BROWSER=false` so nothing opens your browser, `SSH_CONNECTION` set so nothing
  touches your clipboard, servers on port 0, never a fixed port. A test once
  opened real OpenRouter sign-in pages.
- Wait for the state you need (`wait_for`, `wait_until`), never `sleep` and then
  look: a menu's title can be drawn before its list.
- Check processes by their exact command, not `pgrep -f`, which also matches the
  shell that started the test.
- An opt-in test is `#[ignore = "why"]`, so it shows as ignored, not passed.
- A sandbox test skips where there's no sandbox, so it can pass by never
  running: break the wall it checks and watch it fail. Put the file back
  newer than the broken one (`touch` it): cargo goes by modification times,
  and a copy restored with its old time kept the broken build running. Its project lives
  outside the temp folders, which every command may write; a project in one
  hid that `look` could write it.
- Check that apps are out of reach with a probe that only reads
  (`lsappinfo front`), never an Apple event: one outside the sandbox asks you
  for permission on screen.
- Visual checks in tmux use a throwaway `REIN_HOME`, so they don't add to your
  history or saved conversations.

## Slash commands

- What you can set in a file, you can set with a slash command too, in the
  conversation: `/mcp add` rather than editing `mcp.json`, `/login` rather
  than `keys.json`. Asked how to connect an MCP server, rein's answer was
  "edit this JSON", which meant leaving the conversation and getting the
  JSON right by hand. The file stays the record, and you can still edit it:
  the command writes it whole, and keeps whatever else it holds.
- A command's line that may hold a key isn't shown again or kept for ↑
  (`commands::hides`): `/login <provider> <key>`, or `/mcp add` with a
  value written in as it is.
- An error you can fix offers the fix where it shows, as a question or a
  pick, never as a command to type: rein said "/mcp add it again with
  --access outside" of a server that failed in the sandbox, which meant
  typing its whole command again, and "it needs GITHUB_TOKEN" meant leaving
  rein to export it. So the failure is a type (`Failure`), not words, and
  its fix is a key away.

- Narrowing a list finds each word typed whole, every one, in a name or
  a description; letters in order only in a name, each starting a word or
  right after the one before. Letters anywhere, in order, across a
  description kept 171 of 211 skills for `react` ("pRactices").

## Docs

- Doc comments say what and why in plain words, and stay true: when the code
  changes, the comment changes with it.
