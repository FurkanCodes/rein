#!/usr/bin/env python3
"""Runs the gates: the window against each workload, on screen, while
this samples what it costs from outside. `run.py [gate ...]`, or every gate.

Each gate's pass mark is in the report it came from, and is a judgement, not a
published number. What it prints is measured. `REIN_WINDOW` picks another build
of the window, `REIN` another rein, and `REIN_GATES_FOCUSED=1` runs in front,
in a window that has your keyboard."""
import ctypes, json, os, pathlib, struct, subprocess, sys, tempfile, time

GUI = pathlib.Path(__file__).resolve().parent.parent
RELEASE = GUI / "target" / "release"
WINDOW, REPLAY = os.environ.get("REIN_WINDOW", str(RELEASE / "rein-window")), str(RELEASE / "rein-replay")
WORK = pathlib.Path(tempfile.gettempdir()) / "rein-gates"
# Beside your work, leaving your keyboard alone; set this and the run takes it.
ASIDE = [] if os.environ.get("REIN_GATES_FOCUSED") else ["--aside"]


def workload(name):
    path = WORK / f"{name}.jsonl"
    if not path.exists():
        subprocess.run([REPLAY, "make", name, str(path)], check=True)
    return str(path)


def agent(name, *flags):
    return ["--agent", " ".join([REPLAY, "serve", workload(name), "--auto", "300", *flags])]


LIBPROC = ctypes.CDLL("/usr/lib/libproc.dylib")
RUSAGE_INFO_V4 = 4
# `rusage_info_v4`: a 16-byte uuid, then 64-bit fields, the eighth of them this.
PHYS_FOOTPRINT = 16 + 7 * 8


def memory(pid):
    """What a process holds, in MB, as Activity Monitor counts it. `ps` leaves
    out what macOS has compressed, so its number falls with nothing freed:
    the same 10,000 messages read 76 MB, then 14."""
    info = ctypes.create_string_buffer(512)
    if LIBPROC.proc_pid_rusage(pid, RUSAGE_INFO_V4, info) != 0:
        return 0
    return struct.unpack_from("Q", info, PHYS_FOOTPRINT)[0] / 1024 / 1024


def sample(pid):
    out = subprocess.run(["ps", "-o", "pcpu=", "-p", str(pid)], capture_output=True, text=True).stdout.split()
    return (memory(pid), float(out[0].replace(",", "."))) if len(out) == 1 else None


def wakeups(pid):
    """Idle wake-ups so far, as top counts them."""
    out = subprocess.run(["top", "-l", "2", "-s", "1", "-pid", str(pid), "-stats", "pid,idlew"], capture_output=True, text=True).stdout
    rows = [line.split() for line in out.splitlines() if line.strip().startswith(str(pid))]
    return int(rows[-1][1].rstrip("+-")) if rows else None


def measured(window, out):
    """What a finished window wrote. One that crashed measured nothing, and
    zeros in a table read as a very good result."""
    if window.returncode != 0 or not out.exists():
        raise SystemExit(f"{out.stem}: the window ended with {window.returncode}, so nothing it measured counts")
    return json.loads(out.read_text())


def run(name, flags, seconds):
    out = WORK / f"{name}.json"
    out.unlink(missing_ok=True)
    command = [WINDOW, *ASIDE, "--seconds", str(seconds), "--out", str(out), *flags]
    window = subprocess.Popen(command, stderr=subprocess.DEVNULL)
    samples, first, started = [], None, time.time()
    while window.poll() is None:
        one = sample(window.pid)
        if one:
            samples.append((time.time() - started, *one))
        time.sleep(0.5)
    report = measured(window, out)
    late = [one for one in samples if one[0] > 2]
    report["memory_mb_peak"] = max((one[1] for one in samples), default=0)
    report["memory_mb_end"] = samples[-1][1] if samples else 0
    report["cpu_pct_mean"] = sum(one[2] for one in late) / max(len(late), 1)
    return report


def idle(*more):
    """Nothing moving for a minute, on screen."""
    out = WORK / "idle.json"
    out.unlink(missing_ok=True)
    flags = [*agent("table", "--speed", "50"), "--empty", "2", "--mark", "8", "--seconds", "68", "--out", str(out), *more]
    window = subprocess.Popen([WINDOW, *ASIDE, *flags], stderr=subprocess.DEVNULL)
    time.sleep(9)
    first = wakeups(window.pid)
    early = memory(window.pid)
    time.sleep(40)
    last = wakeups(window.pid)
    sampled = sample(window.pid)
    window.wait()
    report = measured(window, out)
    report["idle_wakeups_per_s"] = (last - first) / 41 if first is not None and last is not None else None
    report["memory_mb_peak"] = early
    report["memory_mb_end"], report["cpu_pct_mean"] = sampled if sampled else (0, 0)
    return report


class Serve:
    """A `rein serve` with a home of its own and a key that opens nothing:
    nothing here reaches a model, your sign-ins or your conversations."""

    def __init__(self, home, project):
        env = {"PATH": os.environ["PATH"], "HOME": str(home), "REIN_HOME": str(home), "OPENAI_API_KEY": "test", "OPENAI_BASE_URL": "http://127.0.0.1:9/v1",
               "REIN_MODE": "ask", "REIN_MEMORY": "off", "BROWSER": "false", "SSH_CONNECTION": "gate"}
        self.process = subprocess.Popen([os.environ.get("REIN", "rein"), "serve"], cwd=project, env=env, text=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.lines = 0

    def ask(self, id, method, params):
        """Sends a request, and reads to its answer."""
        self.process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": id, "method": method, "params": params}) + "\n")
        self.process.stdin.flush()
        for line in self.process.stdout:
            self.lines += 1
            message = json.loads(line)
            if message.get("id") == id and "method" not in message:
                if "error" in message:
                    raise SystemExit(f"{method}: {message['error']}")
                return message["result"]
        raise SystemExit(f"rein serve ended during {method}")


def serve_memory():
    """What `rein serve` itself holds with 10,000 messages open."""
    with tempfile.TemporaryDirectory(prefix="rein-gate-") as home:
        home = pathlib.Path(home).resolve()
        project = home / "project"
        project.mkdir()
        fresh = Serve(home, project)
        fresh.ask(1, "initialize", {"protocolVersion": 1, "clientCapabilities": {}})
        fresh.ask(2, "session/new", {"cwd": str(project), "mcpServers": []})
        empty = memory(fresh.process.pid)
        fresh.process.kill()

        said = [json.loads(line)["update"] for line in open(workload("history"))]
        folder = home / "sessions" / str(project).replace("/", "-")
        folder.mkdir(parents=True)
        id = "11111111-2222-3333-4444-555555555555"
        header = {"type": "session", "id": id, "cwd": str(project), "started": "2026-11-07 10:00", "model": "gpt-5", "instructions": None}
        lines = [header] + [{"type": "user" if one["sessionUpdate"] == "user_message_chunk" else "assistant", "text": one["content"]["text"]} for one in said]
        file = folder / "2026-11-07T10-00-00-11111111.jsonl"
        file.write_text("".join(json.dumps(line) + "\n" for line in lines))

        serve = Serve(home, project)
        serve.ask(1, "initialize", {"protocolVersion": 1, "clientCapabilities": {}})
        started = time.time()
        serve.ask(2, "session/load", {"sessionId": id, "cwd": str(project), "mcpServers": []})
        opened = time.time() - started
        held = memory(serve.process.pid)
        time.sleep(30)
        settled = memory(serve.process.pid)
        replayed = serve.lines - 2
        serve.process.kill()
        return {"notes": {"messages": len(said), "file_mb": round(file.stat().st_size / 1e6, 1), "replayed_updates": replayed, "empty_mb": round(empty, 1),
                          "loaded_mb": round(held, 1), "after_30s_mb": round(settled, 1), "open_ms": round(opened * 1000)}}


GATES = {
    # Three panes replaying, one of them 10,000 messages; a fourth typed in.
    "typing": lambda: run("typing", [*agent("long"), *agent("long"), *agent("history"), "--empty", "1", "--type", "--stalls"], 30),
    "idle": idle,
    # The caret in a box, as it mostly is: "idle" has nothing focused.
    "idle-caret": lambda: idle("--focus"),
    # A pointer crossing it for 6 seconds in the middle, then still again.
    "hover": lambda: idle("--sweep", "24,6"),
    "history": lambda: run("history", [*agent("history"), "--settle", "10000", "--scroll", "40", "--stalls"], 25),
    # At these speeds a run reaches a 200 KB answer, and 20,000 lines of code.
    "long-big": lambda: run("long-big", [*agent("long", "--speed", "20"), "--empty", "1", "--type", "--stalls"], 28),
    "code-big": lambda: run("code-big", [*agent("code", "--speed", "50"), "--empty", "1", "--type", "--stalls"], 28),
    "select": lambda: run("select", [*agent("history"), "--settle", "10000", "--select"], 6),
    # Scrolling back through 2,000 messages as an answer comes in below them.
    "reading": lambda: run("reading", [*agent("reading"), "--settle", "2000", "--scroll", "40", "--stalls"], 20),
    "resize": lambda: run("resize", [*agent("long", "--speed", "20"), "--empty", "1", "--type", "--stalls", "--resize"], 20),
    "serve-memory": serve_memory,
    "code": lambda: run("code", [*agent("code"), "--empty", "1", "--type", "--stalls"], 20),
    # Fast enough to end: a table is held as text until then, and its end
    # is the expensive moment, when all of it is laid out at once.
    "table": lambda: run("table", [*agent("table", "--speed", "3"), "--empty", "1", "--type", "--stalls"], 16),
    "burst": lambda: run("burst", [*agent("burst"), "--empty", "1", "--type", "--stalls"], 15),
    "tiny": lambda: run("tiny", [*agent("tiny"), "--empty", "1", "--type", "--stalls"], 20),
    "tool-output": lambda: run("tool-output", [*agent("tool-output"), "--empty", "1", "--type", "--stalls"], 20),
}


def line(name, report):
    keys, stalls, gaps = report.get("key_to_paint", {}), report.get("ui_thread_late", {}), report.get("frame_gap", {})
    # Hitches are time past each frame's due, so only a run drawing all along has any.
    hitch = f"{report.get('hitch_ms_per_s', 0):6.1f} ms/s" if name in ("history", "reading") else "     -     "
    return (f"{name:<12} key p50/p99/max {keys.get('p50_ms', 0):5.1f}/{keys.get('p99_ms', 0):5.1f}/{keys.get('max_ms', 0):6.1f} ms of {keys.get('count', 0):3}  "
            f"stall p99/max {stalls.get('p99_ms', 0):6.1f}/{stalls.get('max_ms', 0):7.1f} ms  "
            f"frames {report.get('frames', 0):5}  hitch {hitch}  "
            f"cpu {report.get('cpu_pct_mean', 0):5.1f}%  memory peak/end {report.get('memory_mb_peak', 0):4.0f}/{report.get('memory_mb_end', 0):4.0f} MB"
            + (f"  settled {report['settled_ms']:.0f} ms" if report.get("settled_ms") else "")
            + (f"  idle frames {report['frames_after_mark']}" if report.get("frames_after_mark") is not None and name.startswith("idle") else "")
            + (f"  wakeups/s {report['idle_wakeups_per_s']:.2f}" if report.get("idle_wakeups_per_s") is not None else "")
            + (f"\n{'':<12} {json.dumps(report['notes'])}" if report.get("notes") else "")
            + (f"\n{'':<12} frames each second {report['frames_each_second']}\n{'':<12} pointer each second {report['pointer_each_second']}" if name.startswith("idle") else ""))


def display_is_off():
    """Whether the display's last word was going off. GPUI doesn't draw then,
    so a gate reads as fast and light with nothing on screen."""
    log = subprocess.run(["pmset", "-g", "log"], capture_output=True, text=True).stdout
    said = [line for line in log.splitlines() if "Display is turned" in line]
    return bool(said) and "turned off" in said[-1]


def main():
    WORK.mkdir(exist_ok=True)
    chosen = sys.argv[1:] or list(GATES)
    results = {}
    for name in chosen:
        if display_is_off():
            raise SystemExit(f"the display is off, so {name} and what's left would measure nothing")
        results[name] = GATES[name]()
        print(line(name, results[name]), flush=True)
    (WORK / "results.json").write_text(json.dumps(results, indent=1))


if __name__ == "__main__":
    main()
