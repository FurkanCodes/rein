#!/usr/bin/env python3
"""Records rein at work for the README: demo.webp, and logo.png and rein.png.

rein runs in tmux on a small Python project, typed into as a person would,
and every change to its screen is kept with the time it came. Each screen
is drawn by render.py and photographed by headless Chrome, then the lot is
put together as an animated WebP. Nothing captures your desktop. Waiting on
the model is cut short: while only the spinner moves, a moment stands for
the wait; typing and what rein shows keep their real pace.

What it shows, for the README's front page: rein as it starts, in trust
mode, talking and at work. A question and its answer, then "the tests
fail, fix them": the run that fails, the edit with its change, the run
that passes, and what the turn came to.

Needs rein, tmux, Google Chrome, Pillow, and the `claude` CLI signed in:
the demo runs on your Claude plan, a few requests. REIN_DEMO_MODEL names
another model, with its key in your environment. rein runs with folders of
its own, so the demo adds nothing to your conversations, history, hooks or
skills, and `claude` keeps your home only to find its sign-in. tmux runs
on a server of its own, so yours is left alone.

Usage: python3 docs/screenshot/demo.py [logo | encode]
  REIN names the rein to record (default: the one on PATH); record the
  build you are about to commit, so the README shows what it says.
  logo: only logo.png. encode: demo.webp and rein.png again from the last
  recording, kept in /tmp/rein-demo.json, without running rein again.
"""

import concurrent.futures
import hashlib
import json
import os
import pathlib
import random
import re
import shutil
import subprocess
import sys
import threading
import time

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import render  # noqa: E402

WORK = pathlib.Path("/tmp/rein-demo")
# The last recording: (screen, seconds) for each frame, and the still.
KEPT = pathlib.Path("/tmp/rein-demo.json")
MARK = ".made-by-demo.py"
SESSION = f"rein-demo-{os.getpid()}"
COLS, ROWS = 100, 34
MODEL = os.environ.get("REIN_DEMO_MODEL", "claude-code/sonnet")
REIN = os.environ.get("REIN", "rein")
CHROME = os.environ.get("CHROME", "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
ESCAPES = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
IDLE = "type a message"
QUESTION = re.compile(r"Allow this|Allow writing|Let [\w.-]+'s [\w.-]+ run\?")
# What moves while rein works and nothing else does: the spinner, and the time.
BUSY = re.compile(r"[⠀-⣿✳-✿]|\b\d+(\.\d+)?s\b")
TYPING = random.Random(7)

# Long enough that the fix is an edit, not the file written again.
SLUG = '''"""Helpers for the blog's URLs and previews."""

import re

MAX_WORDS = 12


def slugify(title):
    """A title as a piece of a URL: "Hello, World!" becomes "hello-world"."""
    return title.lower().replace(" ", "-")


def excerpt(text, words=MAX_WORDS):
    """The first `words` words of `text`, with … if there were more."""
    parts = text.split()
    return " ".join(parts[:words]) + (" …" if len(parts) > words else "")


def reading_time(text, per_minute=200):
    """Minutes it takes to read `text`, at least one."""
    return max(1, round(len(re.findall(r"\\w+", text)) / per_minute))


def tags(text):
    """The #tags in `text`, lowercased, each once, in order."""
    seen = []
    for tag in re.findall(r"#(\\w+)", text):
        if tag.lower() not in seen:
            seen.append(tag.lower())
    return seen
'''
TESTS = '''import unittest

from slug import slugify


class SlugifyTest(unittest.TestCase):
    def test_spaces_become_dashes(self):
        self.assertEqual(slugify("Hello World"), "hello-world")

    def test_punctuation_goes(self):
        self.assertEqual(slugify("Hello, World!"), "hello-world")

    def test_runs_of_spaces_make_one_dash(self):
        self.assertEqual(slugify("rein  is   here"), "rein-is-here")


if __name__ == "__main__":
    unittest.main()
'''
def tmux(*args):
    """tmux, on a server of its own: yours, and what's in it, is left alone."""
    return subprocess.run(["tmux", "-L", "rein-demo", *args], capture_output=True, text=True, check=True).stdout


def screen(colours=False):
    """The pane's text; with `colours`, its colour codes too, and the spaces
    at the ends of rows, which a band or a tint may colour to the edge."""
    return tmux("capture-pane", "-p", *(["-e", "-N"] if colours else []), "-t", SESSION)


def setup():
    if WORK.exists() and not (WORK / MARK).exists():
        sys.exit(f"{WORK} is in the way, and isn't from this script")
    shutil.rmtree(WORK, ignore_errors=True)
    for folder in ("slugs", "rein", "home", "bin", "frames"):
        (WORK / folder).mkdir(parents=True)
    (WORK / MARK).touch()
    (WORK / "slugs/slug.py").write_text(SLUG)
    (WORK / "slugs/test_slug.py").write_text(TESTS)
    # The `claude` CLI's own sign-in, as `rein login claude-code` keeps it.
    (WORK / "rein/claude-code.json").write_text('{"token":null}\n')
    claude = shutil.which("claude")
    if claude:
        wrapper = WORK / "bin/claude"
        wrapper.write_text(f"#!/bin/sh\nHOME='{os.environ['HOME']}' exec '{claude}' \"$@\"\n")
        wrapper.chmod(0o755)


def start():
    env = {
        "HOME": WORK / "home",
        "PATH": f"{WORK / 'bin'}:{os.environ['PATH']}",
        "REIN_HOME": WORK / "rein",
        "REIN_MODEL": MODEL,
        # What commands print in English, whatever this computer speaks.
        "LC_ALL": "en_US.UTF-8",
        # No __pycache__, which rein would list as made by each test run.
        "PYTHONDONTWRITEBYTECODE": "1",
        "TERM": "xterm-256color",
        "COLORTERM": "truecolor",
        "BROWSER": "false",
    }
    # CLAUDECODE unset: run from inside Claude Code, `claude` would see it.
    # A moment first, so the background below is set when rein asks for it.
    line = "sleep 0.3; exec env -u CLAUDECODE " + " ".join(f"{k}='{v}'" for k, v in env.items()) + f" '{REIN}'"
    # The pane's background is what render.py draws: tmux answers rein's
    # question with it, so your messages show on their band.
    tmux("new-session", "-d", "-s", SESSION, "-x", str(COLS), "-y", str(ROWS), "-c", str(WORK / "slugs"), line,
         ";", "set-option", "-t", SESSION, "window-style", f"bg={render.BACKGROUND}", ";", "set-option", "-t", SESSION, "status", "off")


def wait_for(pattern, timeout=600, stable=0.0):
    """The screen once it shows `pattern`, and has stayed the same for `stable` seconds."""
    deadline, seen, since = time.monotonic() + timeout, None, 0.0
    while True:
        now = screen()
        if re.search(pattern, now):
            if now != seen:
                seen, since = now, time.monotonic()
            if time.monotonic() - since >= stable:
                return now
        else:
            seen = None
        if time.monotonic() > deadline:
            sys.exit(f"timed out waiting for {pattern!r}. The screen:\n{now}")
        time.sleep(0.1)


class Recorder(threading.Thread):
    """Every change to the screen, with when it came, and the spans that
    keep their real length (`holds`)."""

    def __init__(self):
        super().__init__(daemon=True)
        self.frames, self.holds, self.still, self.end, self.going = [], [], None, None, True

    def run(self):
        last = None
        while self.going:
            try:
                now = screen(colours=True)
            except subprocess.CalledProcessError:
                break
            if now != last:
                self.frames.append((time.monotonic(), now))
                last = now
            time.sleep(0.03)
        self.end = time.monotonic()

    def hold(self, seconds):
        start = time.monotonic()
        time.sleep(seconds)
        self.holds.append((start, time.monotonic()))


def type_(text, per_second=15):
    for ch in text:
        tmux("send-keys", "-t", SESSION, "-l", ch)
        time.sleep(TYPING.uniform(0.6, 1.4) / per_second)


def key(*names):
    tmux("send-keys", "-t", SESSION, *names)


def work(rec):
    """Lets a turn run, saying yes to each question once it's been seen,
    until rein waits for a message again."""
    time.sleep(1.5)
    while True:
        now = wait_for(f"{IDLE}|{QUESTION.pattern}")
        if QUESTION.search(now):
            rec.hold(2.2)
            type_("y")
            time.sleep(1.0)
            continue
        now = wait_for(f"{IDLE}|{QUESTION.pattern}", stable=2.5)
        if not QUESTION.search(now):
            return


def scenes(rec):
    wait_for(IDLE, stable=1.0)
    rec.hold(1.4)
    # Talking: a question, and its answer.
    type_("hi! what's a URL slug?")
    rec.hold(0.5)
    key("Enter")
    work(rec)
    rec.hold(4.0)
    # At work: the tests fail, and rein fixes them.
    type_("the tests fail, fix them")
    rec.hold(0.5)
    key("Enter")
    work(rec)
    rec.still = time.monotonic()
    rec.hold(5.0)


def changed_lines(before, after):
    a, b = ESCAPES.sub("", before).split("\n"), ESCAPES.sub("", after).split("\n")
    if len(a) != len(b):
        return None
    return [(x, y) for x, y in zip(a, b) if x != y]


def timeline(rec):
    """(screen, seconds) for each frame shown, waits cut short."""
    frames = rec.frames
    held = lambda t: any(start - 0.05 <= t <= end for start, end in rec.holds)  # noqa: E731
    out = []
    for i, (t, shown) in enumerate(frames):
        last = i + 1 == len(frames)
        seconds = (rec.end if last else frames[i + 1][0]) - t
        changed = changed_lines(frames[i - 1][1], shown) if i else None
        spinning = changed is not None and 0 < len(changed) <= 2 and all(BUSY.search(x) and BUSY.search(y) for x, y in changed)
        if spinning and not held(t) and out:
            # Only the spinner moved: the wait so far stands as one moment.
            out[-1] = (shown, min(out[-1][1] + seconds, 0.45))
            continue
        if not held(t):
            seconds = min(seconds, 0.9)
        out.append((shown, max(seconds, 0.03)))
    if out:
        out[-1] = (out[-1][0], 3.5)
    return out


def photograph(shown, png, scale, **how):
    html_page, width, height = render.page(shown, **how)
    page = png.with_suffix(".html")
    page.write_text(html_page)
    profile = png.parent / f"chrome-{png.stem}"
    subprocess.run(
        [CHROME, "--headless=new", "--disable-gpu", "--hide-scrollbars", f"--user-data-dir={profile}", "--default-background-color=00000000",
         f"--force-device-scale-factor={scale}", f"--window-size={width},{height}", f"--screenshot={png}", page.as_uri()],
        capture_output=True, check=True,
    )
    shutil.rmtree(profile, ignore_errors=True)
    page.unlink()
    return png


def encode(items, out):
    from PIL import Image

    frames = WORK / "frames"
    unique = {hashlib.sha256(shown.encode()).hexdigest()[:16]: shown for shown, _ in items}
    with concurrent.futures.ThreadPoolExecutor(6) as pool:
        list(pool.map(lambda kv: photograph(kv[1], frames / f"{kv[0]}.png", 2, window=True, size=(COLS, ROWS)), unique.items()))
    images = [Image.open(frames / f"{hashlib.sha256(shown.encode()).hexdigest()[:16]}.png").convert("RGBA") for shown, _ in items]
    durations = [round(seconds * 1000) for _, seconds in items]
    images[0].save(out, save_all=True, append_images=images[1:], duration=durations, loop=0, lossless=False, quality=88, method=6)
    return len(unique), sum(durations) / 1000


def finish(items, still):
    """rein.png and demo.webp from a recording."""
    photograph(still, HERE / "rein.png", 2, window=True, size=(COLS, ROWS))
    print(f"wrote {HERE / 'rein.png'}")
    count, seconds = encode(items, HERE / "demo.webp")
    size = (HERE / "demo.webp").stat().st_size
    print(f"wrote {HERE / 'demo.webp'}: {count} screens, {seconds:.0f} s, {size / 1e6:.1f} MB")


def main():
    if sys.argv[1:] == ["encode"]:
        kept = json.loads(KEPT.read_text())
        if WORK.exists() and not (WORK / MARK).exists():
            sys.exit(f"{WORK} is in the way, and isn't from this script")
        (WORK / "frames").mkdir(parents=True, exist_ok=True)
        (WORK / MARK).touch()
        try:
            finish([tuple(item) for item in kept["items"]], kept["still"])
        finally:
            shutil.rmtree(WORK, ignore_errors=True)
        return
    only_logo = sys.argv[1:] == ["logo"]
    setup()
    try:
        # The first start says what your rules ask about: once, before the
        # one shown. Its top rows are the logo.
        start()
        wait_for(IDLE, stable=1.0)
        photograph(screen(colours=True), HERE / "logo.png", 4, rows=(0, 3), cols=(1, 18), clear=True)
        print(f"wrote {HERE / 'logo.png'}")
        if only_logo:
            return
        key("C-d")
        while subprocess.run(["tmux", "-L", "rein-demo", "has-session", "-t", SESSION], capture_output=True).returncode == 0:
            time.sleep(0.2)
        start()
        rec = Recorder()
        rec.start()
        scenes(rec)
        rec.going = False
        rec.join()
        items = timeline(rec)
        still = [shown for t, shown in rec.frames if t <= rec.still][-1]
        KEPT.write_text(json.dumps({"items": items, "still": still}))
        finish(items, still)
    finally:
        subprocess.run(["tmux", "-L", "rein-demo", "kill-session", "-t", SESSION], capture_output=True)
        shutil.rmtree(WORK, ignore_errors=True)


if __name__ == "__main__":
    main()
