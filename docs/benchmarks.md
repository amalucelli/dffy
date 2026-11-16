# Benchmarks and comparison

## What's measured

- **dffy next to other tools.** `delta`, `diff-so-fancy`, `difftastic`, `hunk`, `lumen`, git and GNU diff on the Linux kernel: how long each takes, how much memory it uses, and what each one does. The tables under [Setup](#setup) are the output of `cargo xtask compare` ([`crates/xtask`](../crates/xtask/src/compare.rs)), with tool and temporary paths shortened.
- **dffy against an earlier commit of itself.** `cargo xtask perf` ([`crates/xtask`](../crates/xtask/src/perf.rs)) builds the checkout and a baseline commit, runs both on the same machine, and fails when the checkout does more work, allocates more, is slower or uses more memory. It runs seven groups:

  | Group | Runs |
  |---|---|
  | `frame` | the divan benchmarks in `crates/dffy/benches/frame.rs`: drawing, scrolling, searching and preparing files in the viewer |
  | `engine` | the divan benchmarks in `crates/dffy-core/benches/engine.rs`: line diffs and enumerating changes |
  | `pty` | the `first_frame` example: dffy's first frame in a pseudo-terminal |
  | `count` | the `count_pass` example: the file tree's line counts over the range, checked against each file's diff |
  | `paint` | the `first_paint` example: the first and last change found in the range and the working tree |
  | `print` | dffy printing the range, highlighted and not, one file, and the file list |
  | `pager` | dffy reading the range's patch as a pager |

  dffy's own budgets, measured inside the process with the divan benchmarks and the kernel tests, are in [architecture.md](architecture.md#performance).

Both need `DFFY_BENCH_REPO` naming a clone of the Linux kernel with tags `v6.0` and `v6.1`. A shallow clone is enough (about 600MB of history):

```sh
git clone --depth 1 --branch v6.1 https://github.com/torvalds/linux <dir>
git -C <dir> fetch --depth 1 origin tag v6.0
```

## How to run

```sh
export DFFY_BENCH_REPO=<dir>
task perf                       # the checkout against HEAD~1, every group
task perf -- --rev main         # against another commit, branch or tag
task perf -- --only frame,print # only those groups
task perf:quick                 # instruction and allocation counts only (Linux)
task perf:full                  # every group, measuring layout bands when they're stale
task bench:compare       # dffy against the other tools
```

Every run prints its tables and saves them, with `results.json` and every program's raw output, in `target/perf/runs/<time>/`. The tasks never change a tracked file.

### Checking a change for regressions

The candidate is always the checkout, uncommitted changes included; the results mark it as such. The baseline is the commit given with `--rev`, `HEAD~1` unless given. Comparing two branches is checking out one and naming the other:

```sh
git checkout my-branch
task perf -- --rev main
```

| Task | Measures | Rounds | Use it |
|---|---|---|---|
| `task perf:quick` | instruction and allocation counts; Linux only | 3 | on every change |
| `task perf` | timing, peak memory, instructions and allocations | 10 | for changes to hot paths, or when `perf:quick` fails |
| `task perf:full` | the same, with fresh layout bands | 10 | before a release |

`perf:quick` and `perf:full` measure every group. `task perf` does too, unless `ONLY=` narrows it to a comma-separated list of groups from the table above, such as `ONLY=frame,print`.

**Baselines.** Every run builds its baseline fresh: a temporary git worktree of the commit under `target/perf/worktree/`, built from that commit's sources into `target/perf/build/`, and its executables copied to `target/perf/baseline/`. When the run ends, whether it passed, failed or was interrupted with Ctrl-C, it removes its builds, keeping only `target/perf/runs/` and `target/perf/layout.tsv`, and prints what it removed and about how much space that freed.

**Layout bands.** Timings are held to the layout bands in `target/perf/layout.tsv`: per metric, how far its median moved between builds of one commit with different code layouts. One file serves every baseline, and its first line names the commit and date it was measured on. `perf:full` measures it again when it's missing or more than 30 days old, and says so: the baseline's commit built four times with shuffled code layouts and each compared with the first, about three times as long as the comparison. Without the file the bands are 0. `perf` and `perf:quick` never calibrate.

**The rules.** Every round runs both builds, alternating which goes first, and a metric's value is its median over the rounds.

- A timing or peak-memory metric fails when the candidate was slower in enough rounds that a one-sided sign test gives p < 0.05 (9 of 10 at 10 rounds, 5 of 5 at 5) and its median is above the baseline's by more than 1% or the metric's layout band, whichever is larger, or when its instruction count fails. The 1% is criterion's default noise threshold.
- On Linux, the user-space instructions retired, threads and child processes included, are counted for `count_pass`, each `first_paint` run and one more run of dffy per print and pager case, and, after the rounds, per iteration for every divan benchmark whose timing failed. A count fails when the candidate's median is more than 0.5% above the baseline's. On macOS, or without a hardware counter, these rows have no values and pass.
- Allocation counts and bytes per iteration, from divan's allocation profiler, fail on any increase of the median.
- `count_pass` finding files whose count differs from their diff fails. A metric the baseline has and the candidate lacks fails; one only the candidate has is listed.

**The quick tier** keeps only the deterministic metrics, so 3 rounds are enough. Each round runs each divan benchmark alone twice, at 2 iterations and at 2 plus a power of two chosen so that the added iterations take at least a quarter of the time its fixture takes to load, and divides the difference in instructions by the added iterations. Print and pager run on a fixed subset of the range: 421 changed files under ten directories, with C sources and headers, Makefiles and Kconfig, Rust, docs, YAML, JSON, renames and the largest file, in rows named `print/subset` and `pager/subset`. There are no timings, peak memory or `pty` group. The summary ends with the largest spread of an instruction count over one side's rounds.

Every command runs in the kernel clone with only `PATH` from the environment, an empty home and XDG directories, git with no global or system config, and `TERM=xterm-256color`. Peak memory needs `/usr/bin/time` (GNU time on Linux). Layout bands hold only for the machine that measured them. Advanced: `cargo xtask bench-baseline`, `bench-ab`, `bench-noise` and `bench-layouts` run the steps above on a directory of executables, and `bench-noise` writes a `noise.tsv` beside them that lets an allocation metric pass within its band; `cargo xtask` prints their usage.

### Comparing with other tools

`task bench:compare` builds dffy from the checkout and runs `cargo xtask compare` with it. It needs git, GNU diff and hyperfine. Each other tool is found on `PATH` or through its variable (`DELTA`, `DIFFT`, `DIFF_SO_FANCY`, `HUNK`, `LUMEN`, `HYPERFINE`; `DFFY` overrides the build), and a tool it can't find is skipped. `RUNS`, `SLOW_RUNS` and `MEM_RUNS` set the number of runs, and `SCENARIOS` picks sections from `range,renames,file,syntax,tui`, where `tui` is every viewer table. The working-tree viewer table needs uncommitted changes in the clone and is skipped without them. On Linux every measurement waits until the one-minute load average is under 1, so the run stalls on a machine that never gets that quiet. A full run takes about an hour, half of it `difftastic` on the whole range.

## Reading the tables

**Comparison tables:**
- **Total:** wall time from start to exit, with output to `/dev/null`, from [hyperfine](https://github.com/sharkdp/hyperfine): the median, the standard deviation and the range.
- **First byte:** time from spawning the command to the first byte on its stdout (a pipe), from the harness in [`crates/xtask`](../crates/xtask/src/harness.rs). It's what decides when a pager shows its first screen.
- **Own memory** and **Mapped files:** the harness reads `/proc/<pid>/status` every 2ms for every process the command starts and keeps each program's peak, in separate runs from the timed ones. "Own memory" is `RssAnon`: the heap and stacks the program allocated. "Mapped files" is `RssFile`: pages of files the program has mapped, its own executable and, for git and dffy, the repository's pack files. Those pages are the OS file cache, shared with every other process that reads the same packs, so they aren't the program's to free. A daemon that leaves the command's process group, like `hunk`'s session daemon, isn't counted. On macOS, own memory is the whole command's peak RSS from `/usr/bin/time -l`, and the other memory columns are empty.
- **git beside it:** git's own and mapped memory when it runs beside the tool, in a pipeline or as a viewer's helper.
- **First frame** and **Settled:** each viewer runs in a 200×60 pseudo-terminal. The harness timestamps every chunk of output as it's read and feeds it to the [vt100](https://crates.io/crates/vt100) terminal emulator: "first frame" is the timestamp of the chunk after which the scenario's first changed line is on the screen, however the viewer draws it. "Settled" is the last chunk before a full second without output; a viewer that keeps redrawing gets 5 more seconds after its first frame and reads "still drawing". A viewer that doesn't show the line within 60 seconds is stopped and reads ">60s"; one that shows a notice in its place reads "n/a" with the notice. The harness answers the queries a terminal would (device attributes, cursor position, colors, mode reports, window size). The "harness floor" row is `echo` of the same text: the cost of starting a process from the harness. After the run, `q` is sent; memory is sampled from start to exit.

**`perf` tables:**
- **A** and **B:** the baseline's and the checkout's median over the rounds. Times are wall time, `B` bytes, `n` a count; instruction counts are in scientific notation.
- **Delta:** B's median relative to A's.
- **Slower rounds:** the rounds B was slower in, out of those both sides have.
- **Band:** in a `bench-noise` run only, the largest difference between the two sides of one round.
- **Result:** the rule's verdict, with the threshold when a layout band raised it above 1%, and `instructions flat` when a timing's count stayed within 0.5%. A run of the baseline against itself shows a failing metric as a false alarm.

`cargo xtask report <run-dir>` prints a run's tables from its `results.json`: a comparison's as they appear below, an A/B run's as Before, After, Change and Result.

## Updating the published numbers

The tables below are pasted from a run; no task writes them.

1. On a machine with nothing else running, run `task bench:compare`.
2. Run `cargo xtask report target/perf/runs/<time>` with the directory it printed.
3. Paste the tables under their headings below, and update [Setup](#setup) with the run's date, machine and versions from the top of its `summary.md`.

dffy's budget table in [architecture.md](architecture.md#performance) is updated the same way, from the divan benchmarks and the kernel tests.

## Setup

- **Date:** every table was measured on 2026-11-15 on a release build (`cargo build --release`).
  - The `delta` row without highlighting is from a second run of that section the same hour, because its total spread over 0.8s in the first.
  - The range table's first three rows (git, dffy as git's pager, dffy without highlighting) are from a later run of that section the same day, after a change that sped up dffy's printed output. So is dffy's row in the viewer table for the range: the faster line diff moved its settled time, and its first frame stayed the same.
  - dffy's other rows were measured again after that change. They stayed within their spread, or within 5% when timed run for run against the previous build, so they keep their first measurement.
- **Machine:** a VM with 4 vCPUs of an AMD EPYC 9554P, 15.6GiB of RAM, Ubuntu 24.04.5 LTS, Linux 6.12.115, with nothing else running. Each measurement started once the one-minute load average was under 1.
- **Versions:** dffy 0.1.0 (release build), `delta` 0.20.1, `difftastic` 0.72.0, `diff-so-fancy` 1.4.14 on Perl 5.38.2, `hunk` 0.25.0, `lumen` 2.32.0, git 2.43.0, GNU diffutils 3.10, hyperfine 2.0.0, pyte 0.8.2 on Python 3.12.3.
- **Runs:** 10 per measurement after a warmup run, 3 without warmup for commands over about ten seconds, and one for `difftastic` on the whole range. Memory comes from 3 more runs of each command (one for `difftastic` on the range); the viewer tables sample memory during their timed runs.
- **Memory:** "own memory" is what the program allocated itself. "Mapped files" is the part of the repository's pack files (and the program's executable) it has mapped: OS file cache, shared with git and every other reader of the same packs. `git diff` maps the same packs.

## The whole v6.0..v6.1 range, printed

The pagers read what git writes for a terminal. Totals are measured with output to `/dev/null`, first bytes with output to a pipe.

| Tool | Command | First byte, median (min–max) | Total, median ± σ (min–max) | Own memory | Mapped files | git beside it, own / mapped |
|---|---|---|---|---|---|---|
| git diff (baseline) | `git diff --no-ext-diff --color=always v6.0 v6.1` | 200.9ms (186.8–233.3) | 4.17s ± 0.05s (4.12s–4.26s) | 125.9MB | 277.3MB | — |
| dffy, as git's pager | `git diff --no-ext-diff --color=always v6.0 v6.1 \| dffy --paging=never --color=always` | 197.4ms (185.4–235.4) | 4.39s ± 0.05s (4.35s–4.50s) | 3.0MB | 5.5MB | 125.9MB / 276.7MB |
| dffy, on the repository, no highlighting | `dffy v6.0..v6.1 --paging=never --color=always --no-highlight` | 6.1ms (4.0–12.8) | 785.0ms ± 13.5ms (756.2ms–795.5ms) | 48.3MB | 279.2MB | — |
| dffy, on the repository, highlighted | `dffy v6.0..v6.1 --paging=never --color=always` | 7.1ms (7.0–7.3) | 17.07s ± 0.12s (17.01s–17.23s) | 74.7MB | 283.8MB | — |
| `delta`, no highlighting | `git diff --no-ext-diff --color=always v6.0 v6.1 \| delta --paging=never --syntax-theme=none` | 210.8ms (207.1–211.5) | 8.56s ± 0.06s (8.45s–8.57s) | 21.7MB | 7.4MB | 125.9MB / 276.4MB |
| `delta`, highlighted | `git diff --no-ext-diff --color=always v6.0 v6.1 \| delta --paging=never` | 218.0ms (214.0–235.1) | 38.99s ± 0.53s (38.56s–39.62s) | 29.7MB | 7.9MB | 125.9MB / 276.3MB |
| `diff-so-fancy` | `git diff --no-ext-diff --color=always v6.0 v6.1 \| diff-so-fancy` | 216.2ms (199.3–217.0) | 14.78s ± 0.06s (14.75s–14.87s) | 33.4MB | 5.6MB | 125.9MB / 277.2MB |
| `difftastic`, as git's external diff | `DFT_COLOR=always GIT_EXTERNAL_DIFF=difft git diff v6.0 v6.1` | 235.4ms, one run | 956.07s, one run | 916.5MB | 11.5MB | 117.7MB / 277.4MB |

- A pager's first byte follows git's, which comes after git's rename detection: with `--no-renames`, git's first byte comes at 33ms (median of 10).
- dffy as a pager doesn't highlight code; dffy on the repository highlights every file it prints, which is most of its 17.1s.
- git and dffy on the repository each map about 280MB of the repository's 576MB of packs, the same pages of the same files. Of its own memory, dffy holds at most 4MB of file contents and output ahead of what it has written. A file larger than that is diffed on a thread of its own, one at a time, while the others go on, and its output streams once its turn comes. Highlighting adds the syntax trees of the files being highlighted.
- `difftastic` starts one process per file; its memory is the peak of the largest one.

## The range's file list, with rename detection

| Tool | Command | First byte, median (min–max) | Total, median ± σ (min–max) | Own memory | Mapped files | git beside it, own / mapped |
|---|---|---|---|---|---|---|
| git diff --name-status | `git diff --no-ext-diff --name-status v6.0 v6.1` | 193.3ms (188.5–241.2) | 212.0ms ± 19.0ms (190.6ms–249.5ms) | 19.1MB | 63.6MB | — |
| dffy --name-status | `dffy --name-status --color=never v6.0..v6.1` | 3.4ms (2.5–6.2) | 82.5ms ± 3.8ms (75.2ms–87.0ms) | 19.1MB | 65.7MB | — |

Both lists are identical: 12,914 entries, 254 of them renames, with the same similarity scores.

## One large file: kernel/sched/fair.c, v6.0 to v6.1

321KB, 283 lines added and 119 removed.

| Tool | Command | First byte, median (min–max) | Total, median ± σ (min–max) | Own memory | Mapped files | git beside it, own / mapped |
|---|---|---|---|---|---|---|
| GNU diff | `diff -u old/kernel/sched/fair.c new/kernel/sched/fair.c` | 7.9ms (5.0–8.5) | 7.0ms ± 0.6ms (6.8ms–8.4ms) | 1.7MB | 2.1MB | — |
| git diff --no-index | `git diff --no-index --color=always old/kernel/sched/fair.c new/kernel/sched/fair.c` | 13.1ms (6.6–16.2) | 16.6ms ± 2.5ms (10.2ms–18.8ms) | 2.5MB | 5.4MB | — |
| dffy | `dffy --paging=never --color=always old/kernel/sched/fair.c new/kernel/sched/fair.c` | 56.0ms (42.8–64.9) | 59.7ms ± 11.6ms (38.2ms–72.5ms) | 13.5MB | 8.4MB | — |
| `delta` | `delta --paging=never old/kernel/sched/fair.c new/kernel/sched/fair.c` | 27.0ms (23.2–29.0) | 60.7ms ± 12.2ms (48.2ms–84.6ms) | 3.7MB | 8.0MB | 2.4MB / 5.3MB |
| `diff-so-fancy` | `git diff --no-index --color=always old/kernel/sched/fair.c new/kernel/sched/fair.c \| diff-so-fancy` | 32.6ms (26.0–53.6) | 44.1ms ± 16.5ms (37.8ms–77.6ms) | 4.5MB | 5.6MB | 2.5MB / 5.4MB |
| dffy --syntax | `dffy --syntax --paging=never --color=always old/kernel/sched/fair.c new/kernel/sched/fair.c` | 55.5ms (53.6–60.4) | 67.5ms ± 10.4ms (54.3ms–82.1ms) | 34.4MB | 8.5MB | — |
| `difftastic` | `DFT_COLOR=always difft old/kernel/sched/fair.c new/kernel/sched/fair.c` | 254.7ms (230.8–273.6) | 260.6ms ± 10.0ms (244.3ms–271.2ms) | 45.1MB | 7.3MB | 17.1MB / 14.2MB |

`delta` runs `git diff --no-index` itself to compare two files, and `difftastic` runs git as well; their git is in the last column.

dffy parses both sides of the file for highlighting before it prints the first line, so its first byte comes after `delta`'s and `diff-so-fancy`'s; its total is level with `delta`'s. In the viewer, the plain diff paints first and highlighting follows.

## Syntax-aware diffs of five large files

`dffy --syntax --paging=never --color=never old new` and `DFT_COLOR=never difft old new`. "What it showed" is dffy's fallback note, if any, and `difftastic`'s file header.

| File (old → new) | Tool | Time, median ± σ (min–max) | Own memory | Mapped files | What it showed |
|---|---|---|---|---|---|
| kernel/sched/fair.c (316KB → 321KB) | dffy --syntax | 71.1ms ± 9.8ms (57.2ms–93.4ms) | 22.8MB | 7.8MB | syntax diff, nothing fell back |
| kernel/sched/fair.c (316KB → 321KB) | `difftastic` | 253.0ms ± 13.3ms (241.6ms–286.8ms) | 45.8MB | 7.2MB | Text (78 C parse errors, exceeded DFT_PARSE_ERROR_LIMIT, first at 127:21) |
| kernel/exit.c (44KB → 45KB) | dffy --syntax | 20.8ms ± 4.3ms (17.5ms–29.0ms) | 4.4MB | 7.9MB | syntax diff, nothing fell back |
| kernel/exit.c (44KB → 45KB) | `difftastic` | 67.5ms ± 8.2ms (53.6ms–80.1ms) | 9.0MB | 7.3MB | Text (49 C parse errors, exceeded DFT_PARSE_ERROR_LIMIT, first at 172:45) |
| mm/mmap.c (100KB → 100KB) | dffy --syntax | 41.1ms ± 10.3ms (26.7ms–55.3ms) | 8.8MB | 7.7MB | syntax diff, nothing fell back |
| mm/mmap.c (100KB → 100KB) | `difftastic` | 122.1ms ± 10.6ms (114.1ms–145.4ms) | 21.3MB | 7.4MB | Text (55 C parse errors, exceeded DFT_PARSE_ERROR_LIMIT, first at 170:30) |
| mm/vmscan.c (139KB → 212KB) | dffy --syntax | 43.8ms ± 8.9ms (39.7ms–60.7ms) | 12.6MB | 8.0MB | syntax diff, nothing fell back |
| mm/vmscan.c (139KB → 212KB) | `difftastic` | 149.5ms ± 8.6ms (145.6ms–173.1ms) | 27.1MB | 7.2MB | Text (11 C parse errors, exceeded DFT_PARSE_ERROR_LIMIT, first at 2145:23) |
| drivers/net/wireless/realtek/rtw89/rtw8852c_table.c (517KB → 1076KB) | dffy --syntax | 334.8ms ± 18.5ms (314.6ms–364.2ms) | 108.4MB | 7.9MB | syntax diff, nothing fell back |
| drivers/net/wireless/realtek/rtw89/rtw8852c_table.c (517KB → 1076KB) | `difftastic` | 242.3ms ± 9.5ms (234.3ms–268.6ms) | 141.2MB | 7.2MB | Text (1.1 MiB exceeded DFT_BYTE_LIMIT) |

`difftastic`'s times here are for its text fallback, not a structural diff; its header names the limit each file went over. dffy's syntax diff matches parse-error subtrees as opaque pieces, so the same files get a syntax diff.

## Interactive viewers

Every viewer runs in a 200×60 pseudo-terminal. First frame: the scenario's first changed line is on screen. Settled: the last byte before a second with no output. Medians with min–max, over 10 runs, or 3 for `lumen` and `hunk` on the whole range and for `hunk` as a pager.

### The v6.0..v6.1 range

The first changed line is in the first file, `.clang-format`.

| Viewer | Command | First byte | First frame | Settled | Own memory | Mapped files | git beside it, own / mapped |
|---|---|---|---|---|---|---|---|
| harness floor | `echo ping_portaddr_for_each_entry_rcu; sleep 1.5` | 4.0ms (3.6–4.6) | 4.0ms (3.6–4.6) | 4.0ms (3.6–4.6) | 0.1MB | 2.0MB | — |
| dffy | `dffy v6.0..v6.1` | 9.0ms (5.4–14.5) | 9.5ms (7.5–14.5) | 725.2ms (711.7–750.1) | 90.5MB | 279.6MB | — |
| `lumen` | `lumen diff v6.0..v6.1` | 5196.2ms (5158.2–5220.8) | 12434.8ms (12428.3–12450.7) | 12435.0ms (12428.5–12451.0) | 676.2MB | 314.4MB | — |
| `hunk` | `hunk diff v6.0 v6.1` | 10252.5ms (10191.4–10260.7) | 17742.4ms (17692.6–17884.6) | 17746.4ms (17693.4–17887.5) | 4.26GB | 95.8MB | 0.4MB / 5.4MB |

dffy keeps updating the screen after its first frame while it finishes the file list, pairs renames and counts every file's lines for the tree; that work ends at "settled". Its own pty benchmark, which stops at the end of the first complete frame, is in [architecture.md](architecture.md#performance).

### The working tree

The clone has a one-line comment appended to five files. Each viewer opens on everything uncommitted, with no arguments.

| Viewer | Command | First byte | First frame | Settled | Own memory | Mapped files | git beside it, own / mapped |
|---|---|---|---|---|---|---|---|
| dffy | `dffy` | 58.6ms (53.8–72.6) | 63.1ms (54.6–72.8) | 174.2ms (150.6–179.4) | 22.8MB | 18.3MB | — |
| `lumen` | `lumen diff` | 495.8ms (486.1–515.7) | 1122.4ms (1106.8–1220.1) | 7097.1ms (7086.5–7208.7) | 39.3MB | 170.4MB | — |
| `hunk` | `hunk diff` | 660.4ms (618.7–684.5) | 790.0ms (726.4–814.9) | 1361.9ms (1277.8–1485.6) | 166.1MB | 97.1MB | 17.7MB / 14.4MB |

The clone tracks 97,371 files; dffy's default diff covers untracked files as well.

### One large file: kernel/sched/fair.c

Each viewer reads the file from the repository with a path filter, and from the two extracted files where it can.

| Viewer | Command | First byte | First frame | Settled | Own memory | Mapped files | git beside it, own / mapped |
|---|---|---|---|---|---|---|---|
| dffy | `dffy v6.0..v6.1 -- kernel/sched/fair.c` | 8.1ms (7.2–9.8) | 14.1ms (12.6–20.6) | 88.6ms (87.0–103.1) | 22.2MB | 10.9MB | — |
| dffy, two files | `dffy old/kernel/sched/fair.c new/kernel/sched/fair.c` | 6.8ms (4.3–8.3) | 10.9ms (6.2–13.6) | 60.2ms (39.9–68.1) | 13.9MB | 8.9MB | — |
| `lumen` | `lumen diff v6.0..v6.1 --file kernel/sched/fair.c` | 102.9ms (89.9–124.8) | 864.5ms (828.8–890.6) | 5993.7ms (5948.2–6023.4) | 39.5MB | 56.0MB | — |
| `lumen`, two files | | n/a | n/a (no two-file mode) | n/a | | | |
| `hunk` | `hunk diff v6.0 v6.1 -- kernel/sched/fair.c` | 278.0ms (259.6–300.7) | 438.9ms (405.6–461.7) | 609.4ms (575.1–649.5) | 144.0MB | 96.8MB | 1.3MB / 5.9MB |
| `hunk`, two files | `hunk diff --files old/kernel/sched/fair.c new/kernel/sched/fair.c` | 304.5ms (295.1–322.7) | 470.7ms (459.6–494.0) | 552.1ms (541.4–577.8) | 140.6MB | 97.1MB | — |

### One large file: rtw8852c_table.c

`drivers/net/wireless/realtek/rtw89/rtw8852c_table.c`, 517KB at v6.0 and 1,076KB at v6.1, 27,113 lines added and 9,879 removed.

| Viewer | Command | First byte | First frame | Settled | Own memory | Mapped files | git beside it, own / mapped |
|---|---|---|---|---|---|---|---|
| dffy | `dffy v6.0..v6.1 -- drivers/net/wireless/realtek/rtw89/rtw8852c_table.c` | 8.7ms (7.5–11.8) | 35.5ms (27.2–50.5) | 36.8ms (28.5–51.3) | 12.7MB | 9.9MB | — |
| dffy, two files | `dffy old/…/rtw8852c_table.c new/…/rtw8852c_table.c` | 7.0ms (3.6–8.5) | 28.4ms (21.3–40.2) | 28.9ms (21.9–40.5) | 8.0MB | 7.5MB | — |
| `lumen` | `lumen diff v6.0..v6.1 --file drivers/net/wireless/realtek/rtw89/rtw8852c_table.c` | 107.5ms (92.9–115.6) | 7211.5ms (7124.4–7518.4) | 12731.9ms (12650.3–12979.6) | 151.6MB | 56.0MB | — |
| `lumen`, two files | | n/a | n/a (no two-file mode) | n/a | | | |
| `hunk` | `hunk diff v6.0 v6.1 -- drivers/net/wireless/realtek/rtw89/rtw8852c_table.c` | n/a | n/a (shows "File too large to render automatically.") | n/a | 69.6MB | 92.9MB | — |
| `hunk`, two files | `hunk diff --files old/…/rtw8852c_table.c new/…/rtw8852c_table.c` | >60s | >60s | >60s | 113.7MB | 44.0MB | — |

With two files, `hunk` wrote nothing to the terminal in 60 seconds, with one core busy.

### As a pager

The viewer reads the range's patch from `git diff --no-ext-diff v6.0 v6.1` on stdin, with the pty as its terminal. The first changed line is in `.clang-format`.

| Viewer | Command | First byte | First frame | Settled | Own memory | Mapped files | git beside it, own / mapped |
|---|---|---|---|---|---|---|---|
| dffy | `git diff --no-ext-diff v6.0 v6.1 \| dffy` | 214.9ms (193.5–230.4) | 216.7ms (198.1–231.0) | 234.8ms (214.0–251.1) | 2.0MB | 6.5MB | 21.3MB / 65.2MB |
| `hunk` pager | `git diff --no-ext-diff v6.0 v6.1 \| hunk pager` | 7476.5ms (7468.2–7805.2) | 15267.4ms (15044.2–15397.0) | 15268.1ms (15044.7–15397.7) | 4.18GB | 95.6MB | 125.8MB / 276.4MB |
| `lumen` | | n/a | n/a (not a pager: reads revisions, not a patch) | n/a | | | |

Both first frames wait for git's first byte, which comes after git's rename detection. dffy reads the patch only as far as the screen and its look-ahead need, so git waits on the pipe with 21MB of its own. Beside `hunk`, git runs to the end of the patch, with the same 126MB as on its own.

## Method

**Workload.** The Linux kernel repository with tags `v6.0` and `v6.1`. The range changes 13,168 files (12,914 entries once git pairs 254 renames), and `git diff` writes 48MB of patch for it. The single-file cases use kernel files at both tags, extracted to two folders so every tool reads the same two files.

**Isolation.** Every command runs with only `PATH` from the environment, `TERM=xterm-256color`, `COLORTERM=truecolor`, `LANG=C.UTF-8`, an empty `HOME`, empty XDG config, state, cache and data directories, and git with no global or system config (`GIT_CONFIG_GLOBAL` pointing at an empty file, `GIT_CONFIG_NOSYSTEM=1`). That keeps a `diff.external`, a `delta` theme, `diff.renames = copies` or dffy's remembered viewer settings out of the numbers. Each tool runs with its defaults.

**Caches.** The page cache is warm: every repository object and file is read before timing (hyperfine's warmup run, or earlier rows). dffy keeps a disk cache of rename and syntax-diff results; it's emptied before every run, so no run reuses work from another. `hunk` starts a session daemon on first use, which stays up across its runs and is stopped after them.

**The tools don't all do the same work:**
- `delta` highlights syntax by default, and so does dffy when it reads the repository. dffy as a pager doesn't highlight, because a piped diff has only the lines git wrote. Both are shown with highlighting on and off where the tool allows it.
- `difftastic` isn't a pager: it reads two files, not a patch. For the range it runs as git's external diff, once per file, which is how its manual sets it up with git.
- `hunk` and `lumen` are interactive only, so they appear only in the viewer tables.

**Commands.** The pager rows read `git diff --no-ext-diff --color=always v6.0 v6.1`, which is what git hands its pager in a terminal. dffy's rows on the repository read the objects with dffy's own git code. The "Command" column has the exact command line for every row.

**Tools.** `difftastic`, `hyperfine` and `lumen` were installed with `cargo binstall`, `delta` from its GitHub release, and `hunk` and `diff-so-fancy` from npm. `hunk`'s npm package is a Node launcher around a compiled binary; `HUNK` points at that binary, so Node's startup isn't counted against it. The kernel clone is shallow, checked out at a later commit with a one-line comment appended to five files. The working-tree table diffs those five; no other table reads the working tree.

## Feature comparison sources

Checked on 2026-11-15 against the versions benchmarked: `hunk` 0.25.0, `lumen` 2.32.0, `difftastic` 0.72.0, `delta` 0.20.1, `diff-so-fancy` 1.4.14, GNU diffutils 3.10. dffy's column is checked against this repository's code and docs.

**`hunk`** ([README](https://github.com/modem-dev/hunk#readme), [`hunk`.dev](https://hunk.dev))
- Viewer, sidebar, watch (`hunk diff --watch`), mouse, split/unified/auto layout, agent annotations, Jujutsu and Sapling: the README.
- Pager: `core.pager "hunk pager"` in the README; non-diff input goes to `less -R` ([git pager and difftool docs](https://github.com/modem-dev/hunk/blob/main/website/src/content/docs/docs/workflows/git-pager-and-difftool.md)).
- No `git add -p` filter: `hunk`'s own comparison data says it "does not filter interactive staging" ([comparisons.ts](https://github.com/modem-dev/hunk/blob/main/website/src/data/comparisons.ts)); `hunk --help` and `hunk pager --help` have no such mode.
- Highlighting: Shiki themes ([config reference](https://github.com/modem-dev/hunk/blob/main/website/src/content/docs/docs/reference/config.md)). Word emphasis: `lineDiffType: "word-alt"` ([highlightRenderOptions.ts](https://github.com/modem-dev/hunk/blob/main/packages/hunk/src/ui/diff/highlightRenderOptions.ts)).
- Files outside a repository: `hunk diff --files <left> <right>`, two files; `hunk open` browses a folder but doesn't diff two ([files and patches docs](https://github.com/modem-dev/hunk/blob/main/website/src/content/docs/docs/workflows/files-and-patches.md)).
- Extensions: the README's "Extensions (experimental)" section, TypeScript extensions for commands, themes, VCS backends and sidebars.
- Binary: the README says the install script, Homebrew, mise and Nix "ship a standalone binary that does not require Node.js", and the npm install needs Node 22+. The standalone binary is built with `bun build --compile` ([build-bin.ts](https://github.com/modem-dev/hunk/blob/main/scripts/build/build-bin.ts)).

**`lumen`** ([README](https://github.com/jnsahaj/lumen#readme))
- Viewer, sidebar (`tab`), watch (`--watch`, `w`), tree-sitter highlighting, mouse selection, "a single static Rust binary", "Works with Git and Jujutsu (jj)": the README, and [src/vcs/jj.rs](https://github.com/jnsahaj/lumen/blob/main/src/vcs/jj.rs).
- Side by side only: `lumen diff --help` says "Launch interactive side-by-side diff viewer", and there's no unified layout.
- Word emphasis: `compute_word_diff` in [diff_algo.rs](https://github.com/jnsahaj/lumen/blob/main/src/command/diff/diff_algo.rs). Tree-sitter is used for highlighting only.
- Not a pager: its arguments are revisions and pull requests, and `-` reads a commit id from stdin, not a diff ([main.rs](https://github.com/jnsahaj/lumen/blob/main/src/main.rs)). No two-file mode.
- AI and annotations: `draft`, `explain` and `operate` with ten providers, and annotations on selections, hunks and files that you write and export for an agent. Notes written by an agent aren't shown.

**`difftastic`** ([README](https://github.com/Wilfred/difftastic#readme), [manual](https://difftastic.wilfred.me.uk/))
- Structural diff, and a line diff with word highlighting for unsupported files: the README.
- Not a pager or a patch filter: it runs as git's external diff or difftool ([git](https://difftastic.wilfred.me.uk/git.html)), its `-` reads one file, and the README's non-goals say it "does not generate patches".
- Files and folders: [usage](https://difftastic.wilfred.me.uk/usage.html). Layouts: `--display side-by-side | side-by-side-show-both | inline`, with no switch by width ([options.rs at 0.72.0](https://github.com/Wilfred/difftastic/blob/0.72.0/src/options.rs)).
- Other VCSs: [Jujutsu](https://difftastic.wilfred.me.uk/jj.html), [Mercurial](https://difftastic.wilfred.me.uk/mercurial.html).
- Falling back: the file headers in the tables above, from `difftastic`'s own output.

**`delta`** ([README](https://github.com/dandavison/delta#readme), [manual](https://dandavison.github.io/delta/))
- Pager (`core.pager = delta`), `git add -p` (`interactive.diffFilter = delta --color-only`), highlighting, word-level emphasis: the README.
- Not an interactive viewer: `navigate` adds `n`/`N` jumps inside `less` ([navigation](https://dandavison.github.io/delta/navigation-keybindings-for-large-diffs.html)); mouse scrolling is `less`'s ([mouse scrolling](https://dandavison.github.io/delta/tips-and-tricks/mouse-scrolling.html)).
- Layouts: `side-by-side = true` in 0.20.1 ([side-by-side view at 0.20.1](https://github.com/dandavison/delta/blob/0.20.1/manual/src/side-by-side-view.md)). The main branch adds `side-by-side = 140`, a minimum width below which `delta` falls back to unified ([on main](https://github.com/dandavison/delta/blob/main/manual/src/side-by-side-view.md)).
- Files outside a repository: "diffing two files, even if they are not in a git repo" ([usage](https://dandavison.github.io/delta/usage.html)); two folders also worked when tried with 0.20.1.
- Other VCSs: Jujutsu and Mercurial setup in [configuration](https://dandavison.github.io/delta/configuration.html) and [usage](https://dandavison.github.io/delta/usage.html).

**`diff-so-fancy`** ([README](https://github.com/so-fancy/diff-so-fancy#readme))
- Pager (`core.pager "diff-so-fancy | less --tabs=4 -RF"`), `git add -p` (`interactive.diffFilter "diff-so-fancy --patch"`), and `diff -u a b | diff-so-fancy`, recursive too: the README.
- Emphasis: the bundled [DiffHighlight.pm](https://github.com/so-fancy/diff-so-fancy/blob/master/lib/DiffHighlight.pm) highlights what lies between the common prefix and suffix of a changed pair of lines.
- A Perl script: [diff-so-fancy](https://github.com/so-fancy/diff-so-fancy/blob/master/diff-so-fancy) starts with `#!/usr/bin/env perl`. It recognizes Mercurial's diff output.

**GNU diff**: `diff --help` and `diff --version` of diffutils 3.10 (`-u`, `-y` for two columns, `-r`, `--color`; "License GPLv3+").

**dffy**: [viewer.md](viewer.md), [git.md](git.md), [watch.md](watch.md), [usage.md](usage.md#comparing-files-and-folders), [themes.md](themes.md), [plugins.md](plugins.md) and [config.md](config.md) (`view = auto` splits from `split-min-width`, 140 columns), each describing the current code. Highlighting is off in pager mode ([git.md](git.md#piped-input)).
