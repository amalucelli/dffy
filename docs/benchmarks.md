# Benchmarks and comparison

## What's measured

- **dffy next to other tools.** `delta`, `diff-so-fancy`, `difftastic`, `hunk`, `lumen`, git and GNU diff on the Linux kernel: how long each takes, how much memory it uses, and what each one does. The tables under [Setup](#setup) are the output of `cargo xtask compare` ([`crates/xtask`](../crates/xtask/src/compare.rs)), with tool and temporary paths shortened.
- **dffy against an earlier commit of itself.** `cargo xtask perf` ([`crates/xtask`](../crates/xtask/src/perf.rs)) builds the checkout and a baseline commit, runs both on the same machine, and fails when the checkout does more work, allocates more, is slower or uses more memory. It runs eight groups:

  | Group | Runs |
  |---|---|
  | `frame` | the divan benchmarks in `crates/dffy/benches/frame.rs`: drawing, scrolling, searching and preparing files in the viewer |
  | `engine` | the divan benchmarks in `crates/dffy-core/benches/engine.rs`: line diffs and enumerating changes |
  | `pty` | the `first_frame` example: dffy's first frame in a pseudo-terminal |
  | `count` | the `count_pass` example: the file tree's line counts over the range, checked against each file's diff |
  | `paint` | the `first_paint` example: the first and last change found in the range and the working tree |
  | `print` | dffy printing the range, highlighted and not, one file, and the file list |
  | `pager` | dffy reading the range's patch as a pager |
  | `comments` | the viewer on the range with 300 review threads, then `m` and page down through them ([below](#the-comments-group)) |

  dffy's own budgets are in [architecture.md](architecture.md#performance).

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

`perf:quick` measures every group. `task perf` and `task perf:full` do too, unless `--only` narrows them to a comma-separated list of groups from the table above, as in `task perf -- --only frame,print`.

**Baselines.** Every run builds its baseline fresh: a temporary git worktree of the commit under `target/perf/worktree/`, built from that commit's sources into `target/perf/build/`, and its executables copied to `target/perf/baseline/`. When the run ends, whether it passed, failed or was interrupted with Ctrl-C, it removes its builds, keeping only `target/perf/runs/` and `target/perf/layout.tsv`, and prints what it removed and about how much space that freed.

**Layout bands.** Timings are held to the layout bands in `target/perf/layout.tsv`: per metric, how far its median moved between builds of one commit with different code layouts. One file serves every baseline, and its first line names the commit and date it was measured on. `perf:full` measures it again when it's missing or more than 30 days old, and says so: the baseline's commit built four times with shuffled code layouts and each compared with the first, about three times as long as the comparison. When the file is current but has no band for a group being measured, such as a group added since, `perf:full` measures that group's bands alone, adds them and leaves the file's age as it was. Without the file the bands are 0. `perf` and `perf:quick` never calibrate.

**The rules.** Every round runs both builds, alternating which goes first, and a metric's value is its median over the rounds.

- A timing or peak-memory metric fails when the candidate was slower in enough rounds that a one-sided sign test gives p < 0.05 (9 of 10 at 10 rounds, 5 of 5 at 5) and its median is above the baseline's by more than 1% or the metric's layout band, whichever is larger, or when its instruction count fails. The 1% is criterion's default noise threshold.
- On Linux, the user-space instructions retired, threads and child processes included, are counted for `count_pass`, each `first_paint` run and one more run of dffy per print and pager case, and, after the rounds, per iteration for every divan benchmark whose timing failed. A count fails when the candidate's median is more than 0.5% above the baseline's. On macOS, or without a hardware counter, these rows have no values and pass.
- Allocation counts and bytes per iteration, from divan's allocation profiler, fail on any increase of the median.
- `count_pass` finding files whose count differs from their diff fails. A metric the baseline has and the candidate lacks fails; one only the candidate has is listed.

**The quick tier** keeps only the deterministic metrics, so 3 rounds are enough. Each round runs each divan benchmark alone twice, with the same number of samples, and divides the difference in instructions by the iterations the second run adds: the power of two nearest to 200ms of the benchmark's time per iteration, at most 8 more per sample. With equal sample counts, divan's per-sample work cancels in the difference, and a sample holds at most 10 iterations' inputs and outputs. Print and pager run on a fixed subset of the range: 421 changed files under ten directories, with C sources and headers, Makefiles and Kconfig, Rust, docs, YAML, JSON, renames and the largest file, in rows named `print/subset` and `pager/subset`. There are no timings, peak memory, `pty` or `comments` group. The summary ends with the largest spread of an instruction count over one side's rounds.

Every command runs in the kernel clone with only `PATH` from the environment, an empty home and XDG directories, git with no global or system config, and `TERM=xterm-256color`. Peak memory needs `/usr/bin/time` (GNU time on Linux). Layout bands hold only for the machine that measured them. Advanced: `cargo xtask bench-baseline`, `bench-ab`, `bench-noise` and `bench-layouts` run the steps above on a directory of executables, and `bench-noise` writes a `noise.tsv` beside them that lets an allocation metric pass within its band; `cargo xtask` prints their usage.

### The comments group

The `comments` group ([`crates/xtask`](../crates/xtask/src/comments.rs)) times the viewer with a large review open: `dffy --comments <file> v6.0..v6.1` in a 200×60 pseudo-terminal.

**The comments file.** At the start of a run, the checkout's own `dffy comments apply` writes it into the run's temporary directory, and both builds open that same file. It's made from the range's diff and the files' text at `v6.1`, so it's the same on every run:

- 300 threads: one on `.clang-format`'s changed line, so the first screen shows one; 250 over `mm/vmscan.c`'s added lines; 49 over `net/core/filter.c`'s.
- Every third thread covers up to three lines, every fifth is resolved, and every tenth is unplaced, because its anchor's lines aren't in the file.
- Thread `i` has `i % 4` replies, 450 in all.
- The texts range from one line to a paragraph that wraps, or a list with a code block.

**One run.** Each round runs it three times per build and keeps each metric's median:

| Metric | Measures |
|---|---|
| `comments/first frame` | Time from the start to the end of the first frame. |
| `comments/first thread` | Time to the `.clang-format` thread's text on screen. |
| `comments/settled` | Time to the last output before a second of silence. |
| `comments/next thread` | 16 presses of `m`: the first focuses the `.clang-format` thread, the second goes to `mm/vmscan.c`. The median time from a press to the end of the frame it draws. |
| `comments/page down` | Then 16 presses of page down in `mm/vmscan.c`: the median time from a press to the end of the frame it draws. |

- **Between presses.** The harness waits for 150ms of silence before the next press, so a late repaint isn't taken for the next key's frame.
- **Failures.** A run fails if a key draws no frame within 10 seconds, or if no thread's text is on screen after an `m`. It also fails if the last `m` doesn't land on the `mm/vmscan.c` thread it should. `m` walks the placed threads first and the unplaced ones after them, so the 16th press lands on thread 16.

These are timings only, held to the timing rule and to their layout bands, so `perf:quick` leaves the group out. The next `perf:full` adds their bands to `layout.tsv`.

**Baselines without comments.** A baseline from before `--comments`, such as any commit before the review-comments work, can't open the file, so the group would compare nothing: the run fails, saying so, rather than pass a gate it didn't check. Name a baseline that has comments to gate the comments path, leave the group out with `--only`, or pass `--allow-missing-baseline` (to `perf` or `bench-ab`) to run it on the checkout alone, its rows listed as "only in B". A checkout that can't open the file always fails.

```sh
task perf:full -- --rev <commit-with-comments> --only comments   # first time: also measures the group's layout bands
task perf -- --rev <commit-with-comments> --only comments
task perf -- --rev main --allow-missing-baseline                  # main from before comments: the group on B alone
```

The group needs the kernel clone. Its files and line numbers are the range's, and it fails when `DFFY_BENCH_REPO` has fewer added lines in them than threads.

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

dffy's budget table in [architecture.md](architecture.md#performance) is updated the same way, from a `task perf:full` run's report and this comparison.

## Setup

- **Date:** every table is from one run of `cargo xtask compare`, started on 2026-11-17 at 01:56 UTC, on a release build (`cargo build --release`).
- **Machine:** a VM with 4 vCPUs of an AMD EPYC 9554P, 15.6GiB of RAM, Ubuntu 24.04.5 LTS, Linux 6.12.115, with nothing else running. The load average was 0.11 when the run started, and each measurement started once the one-minute load average was under 1.
- **Versions:** dffy 0.2.0 (commit `126c8cf`, built with rustc 1.100.0), `delta` 0.20.1, `difftastic` 0.72.0, `diff-so-fancy` 1.4.14 on Perl 5.38.2, `hunk` 0.25.0, `lumen` 2.32.0, git 2.43.0, GNU diffutils 3.10, hyperfine 2.0.0.
- **Runs:** 10 per measurement after a warmup run, 3 without warmup for commands over about ten seconds, and one for `difftastic` on the whole range. Memory comes from 3 more runs of each command (one for `difftastic` on the range); the viewer tables sample memory during their timed runs.
- **Memory:** "own memory" is what the program allocated itself. "Mapped files" is the part of the repository's pack files (and the program's executable) it has mapped: OS file cache, shared with git and every other reader of the same packs. `git diff` maps the same packs.

## The whole v6.0..v6.1 range, printed

The pagers read what git writes for a terminal. Totals are measured with output to `/dev/null`, first bytes with output to a pipe.

| Tool | Command | First byte, median (min–max) | Total, median ± σ (min–max) | Own memory | Mapped files | git beside it, own / mapped |
|---|---|---|---|---|---|---|
| git diff (baseline) | `git diff --no-ext-diff --color=always v6.0 v6.1` | 207.5ms (184.5–218.5) | 4.20s ± 0.05s (4.09s–4.24s) | 125.8MB | 276.4MB | — |
| dffy, as git's pager | `git diff --no-ext-diff --color=always v6.0 v6.1 \| dffy --paging=never --color=always` | 209.1ms (187.3–217.5) | 4.40s ± 0.04s (4.34s–4.48s) | 3.1MB | 5.8MB | 125.8MB / 277.0MB |
| dffy, on the repository, no highlighting | `dffy v6.0..v6.1 --paging=never --color=always --no-highlight` | 6.4ms (4.0–7.7) | 777.7ms ± 16.0ms (755.5ms–805.6ms) | 46.5MB | 279.9MB | — |
| dffy, on the repository, highlighted | `dffy v6.0..v6.1 --paging=never --color=always` | 7.2ms (6.6–7.5) | 16.14s ± 0.30s (16.01s–16.58s) | 79.1MB | 285.4MB | — |
| `delta`, no highlighting | `git diff --no-ext-diff --color=always v6.0 v6.1 \| delta --paging=never --syntax-theme=none` | 201.8ms (189.5–211.0) | 8.58s ± 0.10s (8.42s–8.60s) | 21.6MB | 7.7MB | 125.8MB / 276.7MB |
| `delta`, highlighted | `git diff --no-ext-diff --color=always v6.0 v6.1 \| delta --paging=never` | 190.6ms (188.8–214.2) | 38.94s ± 0.16s (38.76s–39.08s) | 29.6MB | 7.9MB | 125.8MB / 276.5MB |
| `diff-so-fancy` | `git diff --no-ext-diff --color=always v6.0 v6.1 \| diff-so-fancy` | 211.0ms (209.0–221.7) | 14.81s ± 0.09s (14.75s–14.93s) | 33.4MB | 5.4MB | 125.8MB / 276.4MB |
| `difftastic`, as git's external diff | `DFT_COLOR=always GIT_EXTERNAL_DIFF=difft git diff v6.0 v6.1` | 245.4ms, one run | 935.15s, one run | 917.1MB | 11.5MB | 117.7MB / 276.5MB |

- A pager's first byte follows git's, which comes after git's rename detection: with `--no-renames`, git's first byte comes at 33ms (median of 10).
- dffy as a pager doesn't highlight code; dffy on the repository highlights every file it prints, which is most of its 16.1s.
- git and dffy on the repository each map about 280MB of the repository's 576MB of packs, the same pages of the same files. Of its own memory, dffy holds at most 4MB of file contents and output ahead of what it has written. A file larger than that is diffed on a thread of its own, one at a time, while the others go on, and its output streams once its turn comes. Highlighting adds the syntax trees of the files being highlighted.
- `difftastic` starts one process per file; its memory is the peak of the largest one.

## The range's file list, with rename detection

| Tool | Command | First byte, median (min–max) | Total, median ± σ (min–max) | Own memory | Mapped files | git beside it, own / mapped |
|---|---|---|---|---|---|---|
| git diff --name-status | `git diff --no-ext-diff --name-status v6.0 v6.1` | 197.3ms (187.5–213.0) | 211.0ms ± 9.1ms (192.2ms–221.9ms) | 19.0MB | 63.2MB | — |
| dffy --name-status | `dffy --name-status --color=never v6.0..v6.1` | 2.4ms (2.1–2.7) | 78.5ms ± 3.8ms (72.9ms–84.9ms) | 19.3MB | 66.9MB | — |

Both lists are identical: 12,914 entries, 254 of them renames, with the same similarity scores.

## One large file: kernel/sched/fair.c, v6.0 to v6.1

321KB, 283 lines added and 119 removed.

| Tool | Command | First byte, median (min–max) | Total, median ± σ (min–max) | Own memory | Mapped files | git beside it, own / mapped |
|---|---|---|---|---|---|---|
| GNU diff | `diff -u old/kernel/sched/fair.c new/kernel/sched/fair.c` | 5.7ms (3.7–8.2) | 4.2ms ± 1.1ms (3.4ms–7.0ms) | 1.7MB | 2.0MB | — |
| git diff --no-index | `git diff --no-index --color=always old/kernel/sched/fair.c new/kernel/sched/fair.c` | 9.5ms (5.8–11.9) | 7.9ms ± 2.6ms (6.0ms–13.2ms) | 2.6MB | 5.4MB | — |
| dffy | `dffy --paging=never --color=always old/kernel/sched/fair.c new/kernel/sched/fair.c` | 52.5ms (40.2–58.9) | 43.4ms ± 6.7ms (39.4ms–60.3ms) | 13.5MB | 9.2MB | — |
| `delta` | `delta --paging=never old/kernel/sched/fair.c new/kernel/sched/fair.c` | 16.2ms (11.5–23.8) | 55.1ms ± 11.2ms (43.8ms–71.4ms) | 3.7MB | 8.0MB | 2.6MB / 5.3MB |
| `diff-so-fancy` | `git diff --no-index --color=always old/kernel/sched/fair.c new/kernel/sched/fair.c \| diff-so-fancy` | 31.3ms (28.3–43.3) | 43.9ms ± 8.1ms (36.7ms–58.2ms) | 4.5MB | 5.4MB | 2.6MB / 5.3MB |
| dffy --syntax | `dffy --syntax --paging=never --color=always old/kernel/sched/fair.c new/kernel/sched/fair.c` | 66.9ms (58.5–77.3) | 76.4ms ± 8.8ms (58.7ms–85.5ms) | 34.4MB | 9.4MB | — |
| `difftastic` | `DFT_COLOR=always difft old/kernel/sched/fair.c new/kernel/sched/fair.c` | 258.3ms (239.5–281.4) | 264.4ms ± 11.9ms (248.3ms–290.9ms) | 45.0MB | 7.2MB | 17.1MB / 14.2MB |

`delta` runs `git diff --no-index` itself to compare two files, and `difftastic` runs git as well; their git is in the last column.

dffy parses both sides of the file for highlighting before it prints the first line, so its first byte comes after `delta`'s and `diff-so-fancy`'s; its total is level with `diff-so-fancy`'s, and its median is under `delta`'s. In the viewer, the plain diff paints first and highlighting follows.

## Syntax-aware diffs of five large files

`dffy --syntax --paging=never --color=never old new` and `DFT_COLOR=never difft old new`. "What it showed" is dffy's fallback note, if any, and `difftastic`'s file header.

| File (old → new) | Tool | Time, median ± σ (min–max) | Own memory | Mapped files | What it showed |
|---|---|---|---|---|---|
| kernel/sched/fair.c (316KB → 321KB) | dffy --syntax | 66.7ms ± 9.8ms (55.5ms–83.3ms) | 22.9MB | 8.5MB | syntax diff, nothing fell back |
| kernel/sched/fair.c (316KB → 321KB) | `difftastic` | 271.7ms ± 14.9ms (241.2ms–299.6ms) | 45.9MB | 7.2MB | Text (78 C parse errors, exceeded DFT_PARSE_ERROR_LIMIT, first at 127:21) |
| kernel/exit.c (44KB → 45KB) | dffy --syntax | 22.8ms ± 3.7ms (15.8ms–27.3ms) | 4.4MB | 8.6MB | syntax diff, nothing fell back |
| kernel/exit.c (44KB → 45KB) | `difftastic` | 66.3ms ± 9.7ms (55.9ms–82.4ms) | 9.1MB | 7.3MB | Text (49 C parse errors, exceeded DFT_PARSE_ERROR_LIMIT, first at 172:45) |
| mm/mmap.c (100KB → 100KB) | dffy --syntax | 32.8ms ± 5.3ms (27.4ms–40.8ms) | 8.9MB | 8.5MB | syntax diff, nothing fell back |
| mm/mmap.c (100KB → 100KB) | `difftastic` | 121.3ms ± 12.9ms (102.0ms–141.2ms) | 21.3MB | 7.4MB | Text (55 C parse errors, exceeded DFT_PARSE_ERROR_LIMIT, first at 170:30) |
| mm/vmscan.c (139KB → 212KB) | dffy --syntax | 42.3ms ± 2.1ms (40.0ms–46.3ms) | 12.8MB | 8.6MB | syntax diff, nothing fell back |
| mm/vmscan.c (139KB → 212KB) | `difftastic` | 150.4ms ± 8.0ms (136.2ms–164.3ms) | 26.9MB | 7.2MB | Text (11 C parse errors, exceeded DFT_PARSE_ERROR_LIMIT, first at 2145:23) |
| drivers/net/wireless/realtek/rtw89/rtw8852c_table.c (517KB → 1076KB) | dffy --syntax | 327.1ms ± 15.9ms (308.7ms–360.5ms) | 108.8MB | 8.5MB | syntax diff, nothing fell back |
| drivers/net/wireless/realtek/rtw89/rtw8852c_table.c (517KB → 1076KB) | `difftastic` | 233.4ms ± 10.5ms (221.2ms–254.9ms) | 141.5MB | 7.2MB | Text (1.1 MiB exceeded DFT_BYTE_LIMIT) |

`difftastic`'s times here are for its text fallback, not a structural diff; its header names the limit each file went over. dffy's syntax diff matches parse-error subtrees as opaque pieces, so the same files get a syntax diff.

## Interactive viewers

Every viewer runs in a 200×60 pseudo-terminal. First frame: the scenario's first changed line is on screen. Settled: the last byte before a second with no output. Medians with min–max, over 10 runs, or 3 for `lumen` and `hunk` on the whole range and for `hunk` as a pager.

### The v6.0..v6.1 range

The first changed line is in the first file, `.clang-format`.

| Viewer | Command | First byte | First frame | Settled | Own memory | Mapped files | git beside it, own / mapped |
|---|---|---|---|---|---|---|---|
| harness floor | `echo ping_portaddr_for_each_entry_rcu; sleep 1.5` | 2.5ms (1.5–2.9) | 2.5ms (1.5–2.9) | 2.5ms (1.5–2.9) | 0.1MB | 1.9MB | — |
| dffy | `dffy v6.0..v6.1` | 6.8ms (5.1–8.1) | 8.1ms (6.4–10.0) | 559.5ms (541.3–589.1) | 99.2MB | 285.1MB | — |
| `lumen` | `lumen diff v6.0..v6.1` | 5246.7ms (5192.6–5300.6) | 12474.4ms (12458.1–12545.4) | 12474.6ms (12458.4–12545.7) | 676.2MB | 314.1MB | — |
| `hunk` | `hunk diff v6.0 v6.1` | 10261.6ms (10171.8–10269.2) | 18111.1ms (17731.8–18298.9) | 18111.3ms (17731.9–18300.1) | 4.39GB | 95.7MB | 0.5MB / 5.4MB |

dffy keeps updating the screen after its first frame while it finishes the file list, pairs renames and counts every file's lines for the tree; that work ends at "settled". Its own pty benchmark, which stops at the end of the first complete frame, is in [architecture.md](architecture.md#performance).

### The working tree

The clone has a one-line comment appended to five files. Each viewer opens on everything uncommitted, with no arguments.

| Viewer | Command | First byte | First frame | Settled | Own memory | Mapped files | git beside it, own / mapped |
|---|---|---|---|---|---|---|---|
| dffy | `dffy` | 74.6ms (62.6–91.3) | 77.0ms (68.3–92.3) | 171.3ms (159.0–184.9) | 22.3MB | 19.0MB | — |
| `lumen` | `lumen diff` | 485.2ms (479.6–551.7) | 1113.6ms (1095.6–1166.1) | still drawing 5s later | 39.6MB | 170.6MB | — |
| `hunk` | `hunk diff` | 624.4ms (610.7–646.4) | 732.6ms (712.1–757.3) | 1267.9ms (1240.8–1287.7) | 172.3MB | 97.2MB | 17.7MB / 14.4MB |

The clone tracks 97,371 files; dffy's default diff covers untracked files as well.

### One large file: kernel/sched/fair.c

Each viewer reads the file from the repository with a path filter, and from the two extracted files where it can.

| Viewer | Command | First byte | First frame | Settled | Own memory | Mapped files | git beside it, own / mapped |
|---|---|---|---|---|---|---|---|
| dffy | `dffy v6.0..v6.1 -- kernel/sched/fair.c` | 7.3ms (5.7–11.1) | 21.0ms (17.4–24.0) | 97.1ms (85.0–119.9) | 23.0MB | 11.6MB | — |
| dffy, two files | `dffy old/kernel/sched/fair.c new/kernel/sched/fair.c` | 6.3ms (5.8–9.6) | 14.4ms (10.0–20.9) | 72.3ms (61.2–83.5) | 13.9MB | 9.5MB | — |
| `lumen` | `lumen diff v6.0..v6.1 --file kernel/sched/fair.c` | 98.1ms (84.5–109.4) | 851.3ms (835.0–898.1) | still drawing 5s later | 39.5MB | 56.0MB | — |
| `lumen`, two files | | n/a | n/a (no two-file mode) | n/a | | | |
| `hunk` | `hunk diff v6.0 v6.1 -- kernel/sched/fair.c` | 275.0ms (247.8–311.9) | 427.3ms (387.4–451.5) | 601.1ms (571.6–631.7) | 145.9MB | 96.8MB | 1.3MB / 5.9MB |
| `hunk`, two files | `hunk diff --files old/kernel/sched/fair.c new/kernel/sched/fair.c` | 310.1ms (299.5–336.7) | 470.9ms (443.8–487.9) | 554.9ms (527.5–571.7) | 140.3MB | 97.0MB | — |

### One large file: rtw8852c_table.c

`drivers/net/wireless/realtek/rtw89/rtw8852c_table.c`, 517KB at v6.0 and 1,076KB at v6.1, 27,113 lines added and 9,879 removed.

| Viewer | Command | First byte | First frame | Settled | Own memory | Mapped files | git beside it, own / mapped |
|---|---|---|---|---|---|---|---|
| dffy | `dffy v6.0..v6.1 -- drivers/net/wireless/realtek/rtw89/rtw8852c_table.c` | 7.1ms (6.6–8.0) | 49.2ms (41.6–56.4) | 49.8ms (42.0–57.1) | 13.2MB | 11.7MB | — |
| dffy, two files | `dffy old/…/rtw8852c_table.c new/…/rtw8852c_table.c` | 5.9ms (4.3–8.0) | 40.4ms (38.2–49.5) | 41.1ms (38.7–49.9) | 9.4MB | 9.1MB | — |
| `lumen` | `lumen diff v6.0..v6.1 --file drivers/net/wireless/realtek/rtw89/rtw8852c_table.c` | 108.7ms (90.9–129.4) | 7132.8ms (7079.2–7168.7) | still drawing 5s later | 151.6MB | 56.0MB | — |
| `lumen`, two files | | n/a | n/a (no two-file mode) | n/a | | | |
| `hunk` | `hunk diff v6.0 v6.1 -- drivers/net/wireless/realtek/rtw89/rtw8852c_table.c` | n/a | n/a (shows "File too large to render automatically.") | n/a | 69.0MB | 92.7MB | — |
| `hunk`, two files | `hunk diff --files old/…/rtw8852c_table.c new/…/rtw8852c_table.c` | >60s | >60s | >60s | 113.6MB | 44.1MB | — |

With two files, `hunk` wrote nothing to the terminal in 60 seconds, with one core busy.

### As a pager

The viewer reads the range's patch from `git diff --no-ext-diff v6.0 v6.1` on stdin, with the pty as its terminal. The first changed line is in `.clang-format`.

| Viewer | Command | First byte | First frame | Settled | Own memory | Mapped files | git beside it, own / mapped |
|---|---|---|---|---|---|---|---|
| dffy | `git diff --no-ext-diff v6.0 v6.1 \| dffy` | 208.6ms (203.4–234.6) | 209.7ms (204.4–234.8) | 226.6ms (222.2–256.5) | 2.1MB | 6.6MB | 21.5MB / 65.2MB |
| `hunk` pager | `git diff --no-ext-diff v6.0 v6.1 \| hunk pager` | 7504.7ms (7446.7–7633.5) | 15178.9ms (15175.8–15583.1) | 15179.0ms (15175.9–15583.2) | 4.19GB | 95.8MB | 125.8MB / 276.4MB |
| `lumen` | | n/a | n/a (not a pager: reads revisions, not a patch) | n/a | | | |

Both first frames wait for git's first byte, which comes after git's rename detection. dffy reads the patch only as far as the screen and its look-ahead need, so git waits on the pipe with 21.5MB of its own. Beside `hunk`, git runs to the end of the patch, with the same 126MB as on its own.

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
- Line comments and pull requests, checked on 2026-11-18 against the README on main: your own line notes are in its browser review ("mouse line-range notes"), not the terminal viewer. `hunk gh pr 123` opens a pull request, but the README describes neither its review threads nor posting a review.

**`lumen`** ([README](https://github.com/jnsahaj/lumen#readme))
- Viewer, sidebar (`tab`), watch (`--watch`, `w`), tree-sitter highlighting, mouse selection, "a single static Rust binary", "Works with Git and Jujutsu (jj)": the README, and [src/vcs/jj.rs](https://github.com/jnsahaj/lumen/blob/main/src/vcs/jj.rs).
- Side by side only: `lumen diff --help` says "Launch interactive side-by-side diff viewer", and there's no unified layout.
- Word emphasis: `compute_word_diff` in [diff_algo.rs](https://github.com/jnsahaj/lumen/blob/main/src/command/diff/diff_algo.rs). Tree-sitter is used for highlighting only.
- Not a pager: its arguments are revisions and pull requests, and `-` reads a commit id from stdin, not a diff ([main.rs](https://github.com/jnsahaj/lumen/blob/main/src/main.rs)). No two-file mode.
- AI and annotations: `draft`, `explain` and `operate` with ten providers, and annotations on selections, hunks and files that you write and export for an agent. Notes written by an agent aren't shown.
- Pull requests, checked on 2026-11-18 against the README on main: `lumen diff --pr 123` opens one, and marking a file viewed syncs with GitHub. The README describes neither the pull request's review threads nor posting a review.

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
- Line comments: [comments.md](comments.md) and [viewer.md](viewer.md#comments).
- Agent annotations are partial. An agent that can run commands can add comments with `dffy comments apply`, and the viewer shows them inline with their author. But no agent integration drives it: the MCP server only reads comments, and there's no skill or plugin for an agent.
- Pull request review: [viewer.md](viewer.md#code-host-reviews) and the dffy-review-github section of [plugins.md](plugins.md). It's partial because it needs the experimental plugin and the GitHub CLI, and doesn't support forks or GitHub Enterprise yet.
