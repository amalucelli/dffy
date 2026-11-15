"""Timing harness for bench/compare.sh: what hyperfine can't measure.

pipe: time to the first byte a shell pipeline writes to its stdout. The
pipeline is killed once that byte arrives.

pty: time until an interactive viewer, started in a pseudo-terminal, shows a
given text on its screen. Output chunks are timestamped as they arrive and
replayed into the pyte terminal emulator afterwards, so pyte's speed never
delays a timestamp. The harness answers the terminal queries a real terminal
would (device attributes, cursor position, colors, mode reports, window
size), so a viewer that waits on an answer isn't charged for a terminal that
never replies. A run ends at the first second of silence once the text is on
screen ("settled"), or --busy seconds after it for a viewer that keeps
drawing, or at --timeout when the text never shows, or at the first second
of silence with the --refusal text on screen instead.

mem: peak memory of a command run to completion, output to /dev/null.

pty and mem sample memory from /proc/<pid>/status every 2ms, from a forked
process, for every process in the command's session, and keep the peak per
program name: RssAnon is the program's own memory (heap, stacks), RssFile the
file pages it has mapped (its executable, and for git and dffy the
repository's pack files, which are page cache shared with the OS). The shell
that runs the command isn't reported, and neither is a daemon that leaves
the session.

Each prints one tab-separated line per metric: name, median, min, max, in
milliseconds, or in KB for rss_anon_kb:<program> and rss_file_kb:<program>.
A pty timing that timed out reads "timeout" and the limit, one the viewer
declined reads "refused" and the --refusal text; a settled time that never
came reads "busy" and the --busy window.
"""

import argparse
import fcntl
import json
import os
import re
import select
import signal
import statistics
import struct
import subprocess
import sys
import termios
import time


def summary(name, values):
    return "\t".join(
        [name]
        + [f"{v:.1f}" for v in (statistics.median(values), min(values), max(values))]
    )


def prepare(cmd):
    if cmd:
        subprocess.run(["sh", "-c", cmd], check=True)


class Sampler:
    """Peak RssAnon and RssFile per program name, over every process in one
    session, sampled until stopped. Create it before starting the command
    and pass the session to `watch`: sampling runs in a process forked
    beforehand, so it never holds up the harness's own timestamps."""

    INTERVAL = 0.002

    def __init__(self):
        self.sid = None
        # Every process running now is outside the session to come.
        self.members = {int(e): False for e in os.listdir("/proc") if e.isdigit()}
        self.peaks = {}
        results, out = os.pipe()
        stop, self.stop_w = os.pipe()
        self.pid = os.fork()
        if self.pid == 0:
            # Only its two pipes: a pty end held open here would outlive
            # the viewer.
            for fd in map(int, os.listdir("/proc/self/fd")):
                if fd > 2 and fd not in (out, stop):
                    try:
                        os.close(fd)
                    except OSError:
                        pass
            sid = os.read(stop, 32)
            if sid:
                self.sid = int(sid)
                # Back to back at first, so a command that's over in a few
                # milliseconds is still seen.
                eager = time.perf_counter() + 0.1
                interval = lambda: 0 if time.perf_counter() < eager else self.INTERVAL
                while not select.select([stop], [], [], interval())[0]:
                    self.sample()
                self.sample()
            os.write(out, json.dumps(self.peaks).encode())
            os._exit(0)
        os.close(out)
        os.close(stop)
        self.results = results

    def watch(self, sid):
        os.write(self.stop_w, str(sid).encode())

    def stop(self):
        os.close(self.stop_w)
        data = b""
        while chunk := os.read(self.results, 65536):
            data += chunk
        os.close(self.results)
        os.waitpid(self.pid, 0)
        return json.loads(data)

    def member(self, pid):
        try:
            with open(f"/proc/{pid}/stat", "rb") as f:
                stat = f.read()
        except OSError:
            return False
        # Fields after the parenthesized name: state, ppid, pgrp, session.
        return int(stat[stat.rindex(b")") + 2:].split()[3]) == self.sid

    def sample(self):
        for entry in os.listdir("/proc"):
            if not entry.isdigit():
                continue
            pid = int(entry)
            if pid not in self.members:
                self.members[pid] = self.member(pid)
            if not self.members[pid]:
                continue
            try:
                with open(f"/proc/{pid}/status", "rb") as f:
                    status = f.read()
            except OSError:
                continue
            fields = dict(
                line.split(b":", 1) for line in status.splitlines() if b":" in line
            )
            name = fields[b"Name"].strip().decode()
            if name == "sh" or b"RssAnon" not in fields:
                continue
            anon = int(fields[b"RssAnon"].split()[0])
            mapped = int(fields[b"RssFile"].split()[0])
            peak = self.peaks.setdefault(name, [0, 0])
            peak[0], peak[1] = max(peak[0], anon), max(peak[1], mapped)


def memory_lines(runs):
    """Summary lines for a list of per-run peaks, per program, over the runs
    that saw it."""
    names = sorted({name for peaks in runs for name in peaks})
    lines = []
    for name in names:
        for i, kind in enumerate(["rss_anon_kb", "rss_file_kb"]):
            values = [peaks[name][i] for peaks in runs if name in peaks]
            lines.append(summary(f"{kind}:{name}", values))
    return lines


def mem(args):
    runs, times = [], []
    for _ in range(args.runs):
        prepare(args.prepare)
        # The command waits on fd 3 until the sampler is watching it.
        go_r, go_w = os.pipe()
        sampler = Sampler()
        proc = subprocess.Popen(
            ["sh", "-c", 'read _ <&3; exec 3<&-; eval "$1"', "sh", args.command],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            pass_fds=(go_r,),
            start_new_session=True,
        )
        os.close(go_r)
        sampler.watch(proc.pid)
        start = time.perf_counter()
        os.write(go_w, b"\n")
        os.close(go_w)
        proc.wait()
        times.append((time.perf_counter() - start) * 1000)
        runs.append(sampler.stop())
    print(summary("total_ms", times))
    print("\n".join(memory_lines(runs)))


def pipe(args):
    first = []
    for _ in range(args.runs):
        prepare(args.prepare)
        start = time.perf_counter()
        proc = subprocess.Popen(
            ["sh", "-c", args.command], stdout=subprocess.PIPE, start_new_session=True
        )
        if not os.read(proc.stdout.fileno(), 65536):
            sys.exit(f"no output: {args.command}")
        first.append((time.perf_counter() - start) * 1000)
        # Totals come from hyperfine, so the rest of the output isn't waited for.
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait()
        proc.stdout.close()
    print(summary("first_byte_ms", first))


# Terminal queries, with the replies a dark xterm-like terminal would give.
REPLIES = [
    (re.compile(rb"\x1b\[0?c"), lambda m, r, c: b"\x1b[?62;22c"),
    (re.compile(rb"\x1b\[6n"), lambda m, r, c: b"\x1b[1;1R"),
    (re.compile(rb"\x1b\](1[01]);\?(?:\x07|\x1b\\)"), lambda m, r, c: b"\x1b]"
     + m.group(1) + (b";rgb:ffff/ffff/ffff" if m.group(1) == b"10" else b";rgb:0000/0000/0000")
     + b"\x1b\\"),
    (re.compile(rb"\x1b\[\?(\d+)\$p"), lambda m, r, c: b"\x1b[?" + m.group(1) + b";2$y"),
    (re.compile(rb"\x1b\[\?u"), lambda m, r, c: b"\x1b[?0u"),
    (re.compile(rb"\x1b\[>0?q"), lambda m, r, c: b"\x1bP>|harness\x1b\\"),
    (re.compile(rb"\x1b\[14t"), lambda m, r, c: f"\x1b[4;{r * 20};{c * 10}t".encode()),
    (re.compile(rb"\x1b\[16t"), lambda m, r, c: b"\x1b[6;20;10t"),
    (re.compile(rb"\x1b\[18t"), lambda m, r, c: f"\x1b[8;{r};{c}t".encode()),
]


def on_screen(pyte, args, chunks, text=None):
    """The index of the chunk after which `text`, by default the match text,
    is on screen."""
    text = text or args.match
    screen = pyte.Screen(args.cols, args.rows)
    stream = pyte.ByteStream(screen)
    for i, (_, data) in enumerate(chunks):
        stream.feed(data)
        if any(text in line for line in screen.display):
            return i
    return None


def pty_run(args, pyte):
    master, slave = os.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", args.rows, args.cols, 0, 0))
    sampler = Sampler()
    start = time.perf_counter()
    proc = subprocess.Popen(
        args.argv,
        cwd=args.cwd,
        stdin=slave,
        stdout=slave,
        stderr=slave,
        start_new_session=True,
        preexec_fn=lambda: fcntl.ioctl(0, termios.TIOCSCTTY, 0),
    )
    os.close(slave)
    sampler.watch(proc.pid)
    chunks = []
    seen = busy = False
    deadline = start + args.timeout
    check = start + args.settle
    while True:
        now = time.perf_counter()
        left = deadline - now
        if not seen and now >= check:
            # A viewer that keeps drawing never goes silent; look anyway.
            check = now + args.settle
            if on_screen(pyte, args, chunks) is not None:
                seen = True
                deadline = min(deadline, now + args.busy)
                busy = True
        ready, _, _ = select.select([master], [], [], max(min(args.settle, left), 0))
        if left <= 0:
            break
        if not ready:
            # A second of silence: done once the text is on screen.
            seen = seen or on_screen(pyte, args, chunks) is not None
            if seen:
                busy = False
                break
            if args.refusal and on_screen(pyte, args, chunks, args.refusal) is not None:
                break
            continue
        try:
            data = os.read(master, 65536)
        except OSError:
            break
        if not data:
            break
        chunks.append((time.perf_counter(), data))
        for pattern, reply in REPLIES:
            for m in pattern.finditer(data):
                os.write(master, reply(m, args.rows, args.cols))
    i = on_screen(pyte, args, chunks)
    if i is None:
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait()
        os.close(master)
        refused = args.refusal and on_screen(pyte, args, chunks, args.refusal) is not None
        return "refused" if refused else None, sampler.stop()
    try:
        os.write(master, args.quit.encode())
    except OSError:
        pass
    end = time.perf_counter() + 5
    while proc.poll() is None and time.perf_counter() < end:
        if select.select([master], [], [], 0.05)[0]:
            try:
                os.read(master, 65536)
            except OSError:
                pass
    if proc.poll() is None:
        os.killpg(proc.pid, signal.SIGTERM)
        proc.wait()
    os.close(master)
    ms = lambda t: (t - start) * 1000
    settled = None if busy else ms(chunks[-1][0])
    return (ms(chunks[0][0]), ms(chunks[i][0]), settled), sampler.stop()


def pty(args):
    try:
        import pyte
    except ImportError:
        sys.exit("pty mode needs pyte: pip install pyte")
    times, peaks = [], []
    for _ in range(args.runs):
        prepare(args.prepare)
        run, peak = pty_run(args, pyte)
        peaks.append(peak)
        if run is None or run == "refused":
            # One timeout or refusal is the answer; more runs would only
            # repeat it.
            times = run
            break
        times.append(run)
    for i, name in enumerate(["first_byte_ms", "first_frame_ms", "settled_ms"]):
        if times == "refused":
            print(f"{name}\trefused\t{args.refusal}")
        elif times is None:
            print(f"{name}\ttimeout\t{args.timeout:.0f}")
        elif any(r[i] is None for r in times):
            print(f"{name}\tbusy\t{args.busy:.0f}")
        else:
            print(summary(name, [r[i] for r in times]))
    print("\n".join(memory_lines(peaks)))


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="mode", required=True)
    p = sub.add_parser("pipe")
    p.add_argument("--runs", type=int, default=10)
    p.add_argument("--prepare", default="")
    p.add_argument("command")
    m = sub.add_parser("mem")
    m.add_argument("--runs", type=int, default=3)
    m.add_argument("--prepare", default="")
    m.add_argument("command")
    t = sub.add_parser("pty")
    t.add_argument("--runs", type=int, default=10)
    t.add_argument("--prepare", default="")
    t.add_argument("--cwd", default=".")
    t.add_argument("--rows", type=int, default=60)
    t.add_argument("--cols", type=int, default=200)
    t.add_argument("--match", required=True, help="text whose appearance counts as the first frame")
    t.add_argument("--refusal", default="", help="text a viewer shows instead when it declines to render")
    t.add_argument("--settle", type=float, default=1.0, help="seconds of silence that end a run")
    t.add_argument("--timeout", type=float, default=60)
    t.add_argument(
        "--busy", type=float, default=5, help="seconds after the first frame a viewer that never goes silent is given"
    )
    t.add_argument("--quit", default="q")
    t.add_argument("argv", nargs="+")
    args = parser.parse_args()
    {"pipe": pipe, "mem": mem, "pty": pty}[args.mode](args)


if __name__ == "__main__":
    main()
