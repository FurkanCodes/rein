# 02: The agent loop

**Code:** `git checkout ep02`

rein reads a file, asks before running a command, and keeps going until it
has an answer:

````
$ rein "What does calc.py print? Read it, then run it to check."
Using ChatGPT plan (manage usage: https://chatgpt.com/settings/usage)
[123 in (0 cached), 18 out (0 reasoning)]
> read calc.py
  (4 lines back to the model)
[171 in (0 cached), 20 out (0 reasoning)]
run `python calc.py`? [y/N] y
> `python calc.py`
  (2 lines back to the model)
[216 in (0 cached), 21 out (0 reasoning)]
run `python3 calc.py`? [y/N] y
> `python3 calc.py`
  (2 lines back to the model)
It prints:

```text
-1
```

Despite its name, `add(a, b)` returns `a - b`, so `add(2, 3)` evaluates to `-1`. Running it with `python3 calc.py` confirmed this.
[257 in (0 cached), 58 out (0 reasoning)]
````

Four requests for one question. There's no `python` on this machine, so the
first command failed; its error went back to the model as text, and the
model tried `python3`. Nobody wrote that rule. That's the loop.

## The idea: the model asks, the harness acts

A model still can't touch anything. What changes is that rein tells it which
**tools** exist, with a name, a description and a JSON schema for the
arguments, and the model can answer with a request to call one instead of
with text. rein runs it, adds the result to the conversation, and sends the
whole conversation again. That repeats until the model answers with text
alone.

Anthropic's [Building effective agents](https://www.anthropic.com/engineering/building-effective-agents)
says the same thing in one line: agents "are typically just LLMs using tools
based on environmental feedback in a loop." rein's loop is about 40 lines
(`Agent::send` in `crates/rein-agent/src/agent.rs`):

```
history += your message
repeat up to 25 times:
    send history + tool descriptions        → text, tool calls
    history += everything the model produced
    no tool calls?  done
    for each call:  ask you if it's a command, run it,
                    history += its result
```

The same post recommends "stopping conditions (such as a maximum number of
iterations) to maintain control". rein's is 25 turns; after that it stops and
hands the keyboard back.

## What we built

```
┌──────────── rein ────────────┐
│ UI        rein-cli            │  ← prints steps, asks y/N
│ Loop      ask → act → repeat  │  ← this episode
│ Tools     read, bash          │  ← this episode
│ Context   what to resend      │
│ Provider  rein-llm            │  ← items, tools, prompt cache
└──────────────┬───────────────┘
               │ HTTPS + server-sent events
               ▼
          the model
```

| Piece | File | Why a harness needs it |
|---|---|---|
| Items | `Item` in `crates/rein-llm/src/lib.rs` | A conversation isn't just text any more. It's a list of items: what you typed, what the model wrote, its tool calls, their results, and provider data rein sends back untouched (OpenAI's encrypted reasoning, so the model keeps its train of thought between turns without rein ever reading it). |
| The loop | `crates/rein-agent/src/agent.rs` | Ask, run, send back, repeat; stop when the model answers or after 25 turns. A refused command never runs: the model is told "The user declined to run this command." and carries on from there. |
| `read` | `crates/rein-agent/src/tools.rs` | Returns a text file from the project. The path is resolved, symlinks included, and anything outside the project folder is refused, so the model can't read your credentials in `~/.config`. |
| `bash` | same | Runs a command with `sh -c` in the project folder, with no input, stops it after 60 seconds, and returns its output and exit code. You approve every one. |
| Errors as text | same | A tool that fails returns what went wrong (`error: can't read x.rs: No such file or directory (os error 2)`) instead of stopping rein. An error the model can read is one it can work around, like `python` → `python3` above. |
| Output caps | same | Everything the model sees is resent with every later request, so a tool's output is cut at 30 KB, on a character boundary, with a note saying so. |
| Tools in a namespace | `crates/rein-llm/src/openai.rs` | Sign in with ChatGPT only accepts function tools grouped in a namespace, so rein's live in one called `rein`. Their schemas are strict: every property required, nothing extra allowed. |
| Prompt cache | `session-id` in `openai.rs` | See below. |
| Loop tests | tests in `agent.rs` | The loop talks to a `Model` and a `Ui`, not to OpenAI and a terminal. Tests drive it with a scripted model and a fake person who approves or refuses, and check every request it sends. A real recorded tool call is replayed too. |

`rein -s "..."` also arrived: standing instructions, sent as the system
prompt with every request.

## The cache we weren't getting

The loop resends the whole conversation every turn, so most of each request
is the same as the last. Providers keep a **prompt cache** for that: when a
request starts like an earlier one, they can skip the work for the part they
have already seen. OpenAI documents it in its
[prompt caching guide](https://developers.openai.com/api/docs/guides/prompt-caching),
for prompts of 1,024 tokens or more, which is why the short demo above shows
`0 cached` every time.

Through Sign in with ChatGPT, though, nothing was ever cached, even for long
prompts. The request field meant to steer it, `prompt_cache_key`, is
replaced by the server with a random key on every request. What the cache
does follow is a `session-id` header. So rein now makes one random ID per
conversation and sends it with every request.

Measured with `gpt-6.1-sol`: an identical 5,248-token request went from **0**
cached tokens to **5,120** from the second request on, and the second turn of
an agent loop now reuses **95%** of its input. rein prints the hit rate after
every request, so a regression would show at once.

## Decisions

- **Ask before every command; don't ask before reads.** A read can't change
  anything, and it can't leave the project. A command can do anything you
  can, so for now every one needs a yes. No answer, like input that isn't a
  terminal, means no.
- **Traits between the loop and the world.** `Model` and `Ui` keep the loop
  free of OpenAI and of the terminal. That's what makes it testable without
  the network, and it's where a second provider will plug in.
- **Still small.** The loop, the items and two tools cost about 50 KB: the
  release binary went from 2,302,688 bytes in episode 1 to 2,352,720.

## Try it

```sh
cargo build --release
./target/release/rein "What does calc.py print? Read it, then run it to check."
./target/release/rein -s "Answer in one sentence." "What's in this folder?"
cargo test
```

## Known gaps

- **One task per run.** rein does what you asked and exits; there's no
  conversation to follow up in yet.
- **Read and run, nothing else.** The model can't change a file except
  through `bash`, which you have to read as a shell command.
- **All or nothing.** Every command asks, every time. There's no way to say
  "yes to `cargo test` from now on".
- **Ctrl-C ends rein**, not just the turn.
- **No sandbox.** `read` stays in the project, but `bash` can run anything
  you approve, `cat ~/.ssh/id_ed25519` included. Your yes is the only guard.

## Next

Changing files safely: `write` and `edit` tools whose changes you see before
they happen, and a permission engine with modes and rules, so `cargo test`
can run without asking and `git push` never can.
