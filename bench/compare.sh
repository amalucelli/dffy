#!/bin/sh
# Times dffy against delta, diff-so-fancy, difftastic, hunk and lumen on the
# Linux kernel's v6.0..v6.1 and prints the tables in docs/benchmarks.md.
#
#   DFFY_BENCH_REPO=~/src/linux bench/compare.sh > results.md
#
# The clone needs tags v6.0 and v6.1; the script never modifies it. The
# working-tree viewer table needs uncommitted changes in it and is skipped
# without them. Each tool is taken from its variable or else from PATH, and a
# missing one is skipped:
#   DFFY DELTA DIFFT DIFF_SO_FANCY HUNK LUMEN HYPERFINE
# HUNK should name hunk's native binary (with npm, node_modules/hunkdiff-<os>-<arch>/bin/hunk)
# rather than the Node launcher, so the launcher's startup isn't counted.
# PYTHON names a python3 with pyte installed, for the interactive viewers;
# without pyte those tables are skipped.
# RUNS (default 10) sets the runs per measurement, SLOW_RUNS (default 3) the
# runs for commands that take more than about ten seconds, MEM_RUNS (default
# 3) the runs that sample memory.
# SCENARIOS (default "range renames file syntax tui") picks the sections;
# "tui" is every viewer table.
set -eu

: "${DFFY_BENCH_REPO:?point DFFY_BENCH_REPO at a Linux clone with tags v6.0 and v6.1}"
RUNS=${RUNS:-10}
SLOW_RUNS=${SLOW_RUNS:-3}
MEM_RUNS=${MEM_RUNS:-3}
SCENARIOS=${SCENARIOS:-range renames file syntax tui}
PYTHON=${PYTHON:-python3}
HARNESS=$(cd "$(dirname "$0")" && pwd)/harness.py
REPO=$(cd "$DFFY_BENCH_REPO" && pwd)

tool() {
	command -v "$1" 2>/dev/null || true
}
DFFY=$(tool "${DFFY:-dffy}")
DELTA=$(tool "${DELTA:-delta}")
DIFFT=$(tool "${DIFFT:-difft}")
DSF=$(tool "${DIFF_SO_FANCY:-diff-so-fancy}")
HUNK=$(tool "${HUNK:-hunk}")
LUMEN=$(tool "${LUMEN:-lumen}")
HYPERFINE=$(tool "${HYPERFINE:-hyperfine}")
[ -n "$HYPERFINE" ] || { echo "hyperfine not found" >&2; exit 1; }

# Every tool runs with an empty home, config, state and cache, and git with
# no global or system config, so nobody's settings (a diff.external, a delta
# theme, remembered viewer state) change what's measured.
WORK=$(mktemp -d "${TMPDIR:-/tmp}/dffy-compare.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
trap 'exit 130' INT TERM
mkdir -p "$WORK/home" "$WORK/config" "$WORK/state" "$WORK/cache" "$WORK/data"
: >"$WORK/gitconfig"
export HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/config" XDG_STATE_HOME="$WORK/state" \
	XDG_CACHE_HOME="$WORK/cache" XDG_DATA_HOME="$WORK/data" \
	GIT_CONFIG_GLOBAL="$WORK/gitconfig" GIT_CONFIG_NOSYSTEM=1 TERM=xterm-256color
unset GIT_EXTERNAL_DIFF GIT_PAGER PAGER BAT_THEME DELTA_FEATURES DELTA_PAGER NO_COLOR \
	DFT_DISPLAY DFT_BACKGROUND DFT_GRAPH_LIMIT DFT_BYTE_LIMIT DFT_PARSE_ERROR_LIMIT
# dffy's disk cache is emptied before every run, so no run reuses another's
# rename or syntax results.
FRESH="rm -rf '$WORK/cache/dffy'"
cd "$REPO"

ver() {
	"$@" 2>/dev/null | head -n 1
}

# hyperfine ARGS...: one command, timed, exported to $WORK/h.json.
bench() {
	"$HYPERFINE" --shell=sh --ignore-failure --export-json "$WORK/h.json" --style none "$@" >/dev/null
}

# The last hyperfine run's wall time as median ± stdev (min–max).
stats() {
	"$PYTHON" -I -c '
import json, sys
t = json.load(open(sys.argv[1]))["results"][0]["summary"]["time_wall_clock"]
med, sd, lo, hi = t["median"], t["stddev"] or 0, t["min"], t["max"]
f = (lambda v: f"{v * 1000:.1f}ms") if hi < 1 else (lambda v: f"{v:.2f}s")
print(f"{f(med)} ± {f(sd)} ({f(lo)}–{f(hi)})")
' "$WORK/h.json"
}

# Turns the harness's rss_anon_kb:<program> and rss_file_kb:<program> lines
# (medians in KB) into three table cells: the tool's own memory, the files it
# has mapped, and git's own / mapped when git runs beside it. The tool is the
# program other than git with the most memory of its own.
memory_cells() {
	awk -F '\t' '
	function mb(k) { return k < 1048576 ? sprintf("%.1fMB", k / 1024) : sprintf("%.2fGB", k / 1048576) }
	/^rss_/ {
		split($1, key, ":")
		if (key[1] == "rss_anon_kb") anon[key[2]] = $2; else mapped[key[2]] = $2
	}
	END {
		tool = ""
		for (p in anon) if (p != "git" && (tool == "" || anon[p] > anon[tool])) tool = p
		if (tool == "") tool = "git"
		other = (tool != "git" && ("git" in anon)) ? mb(anon["git"]) " / " mb(mapped["git"]) : "—"
		printf "%s | %s | %s", mb(anon[tool]), mb(mapped[tool]), other
	}' "$@"
}

# memory CMD [RUNS]: the memory cells for CMD, sampled over RUNS runs
# (default MEM_RUNS).
memory() {
	"$PYTHON" -I "$HARNESS" mem --runs "${2:-$MEM_RUNS}" --prepare "$FRESH" "$1" | memory_cells
}

# Waits until the one-minute load average is below 1, so a measurement
# doesn't start while the previous one's processes, or anything else, still
# load the machine. Gives up after five minutes on a machine that stays busy.
idle() {
	waited=0
	while awk '{ exit !($1 >= 1) }' /proc/loadavg; do
		if [ "$waited" -ge 300 ]; then
			echo "load average still at 1 or more after 5 minutes; measuring anyway" >&2
			return
		fi
		sleep 10
		waited=$((waited + 10))
	done
}

# row NAME RUNS INPUT TOOL: one table row for TOOL reading INPUT's output,
# or TOOL alone when INPUT is empty. Total time comes from hyperfine, first
# byte and memory from the harness.
row() {
	name=$1 runs=$2 input=$3 cmd=$4
	full=${input:+$input | }$cmd
	warmup=0
	[ "$runs" = "$RUNS" ] && warmup=1
	mem_runs=$MEM_RUNS
	[ "$runs" -lt "$mem_runs" ] && mem_runs=$runs
	idle
	bench --warmup "$warmup" --runs "$runs" --prepare "$FRESH" "$full"
	time=$(stats)
	idle
	first=$("$PYTHON" -I "$HARNESS" pipe --runs "$runs" --prepare "$FRESH" "$full" |
		awk -F '\t' '{ printf "%sms (%s–%s)", $2, $3, $4 }')
	idle
	echo "| $name | \`$(show "$full")\` | $first | $time | $(memory "$full" "$mem_runs") |"
}

# A command as the tables show it: tools by name, files relative to the
# temporary directory, and pipes escaped for markdown.
show() {
	s=$1
	for t in "$DFFY" "$DELTA" "$DIFFT" "$DSF" "$HUNK" "$LUMEN"; do
		[ -n "$t" ] && s=$(printf '%s\n' "$s" | sed "s|$t|${t##*/}|g")
	done
	printf '%s\n' "$s" | sed -e "s|$WORK/||g" -e 's/|/\\|/g'
}

header() {
	echo
	echo "| Tool | Command | First byte, median (min–max) | Total, median ± σ (min–max) | Own memory | Mapped files | git beside it, own / mapped |"
	echo "|---|---|---|---|---|---|---|"
}

cat <<EOF
# dffy comparison benchmark

- Date: $(date -u '+%Y-%m-%d %H:%M UTC')
- Machine: $(grep -m 1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2 | sed 's/^ //'), $(nproc) cores, $(awk '/MemTotal/ { printf "%.0fGB", $2 / 1048576 }' /proc/meminfo) RAM, $(sed -n 's/^PRETTY_NAME="\(.*\)"/\1/p' /etc/os-release 2>/dev/null), Linux $(uname -r)
- Load average at start: $(cut -d ' ' -f 1-3 /proc/loadavg)
- Repository: $(git -C "$REPO" describe --always v6.1) ($(git diff --no-ext-diff --name-only v6.0 v6.1 | wc -l | tr -d ' ') files changed in v6.0..v6.1)
- Runs: $RUNS ($SLOW_RUNS for commands over ~10s, $MEM_RUNS sampling memory), warm page cache, dffy's disk cache emptied before each run
- Versions: dffy $(ver "$DFFY" --version | cut -d ' ' -f 2), $(ver git --version), $(ver diff --version), hyperfine $(ver "$HYPERFINE" --version | cut -d ' ' -f 2)
EOF
[ -n "$DELTA" ] && echo "  $(ver "$DELTA" --version)"
[ -n "$DIFFT" ] && echo "  $(ver "$DIFFT" --version)"
# diff-so-fancy prints its version only when stdin is a terminal.
[ -n "$DSF" ] && echo "  diff-so-fancy $(sed -n 's/^my [$]VERSION = "\(.*\)";/\1/p' "$(readlink -f "$DSF")")"
[ -n "$HUNK" ] && echo "  hunk $(ver "$HUNK" --version)"
[ -n "$LUMEN" ] && echo "  $(ver "$LUMEN" --version)"
true

case $SCENARIOS in *range*)
	echo
	echo "## The whole v6.0..v6.1 range, printed"
	header
	git_in="git diff --no-ext-diff --color=always v6.0 v6.1"
	row "git diff (baseline)" "$RUNS" "" "$git_in"
	[ -n "$DFFY" ] && row "dffy, as git's pager" "$RUNS" "$git_in" "$DFFY --paging=never --color=always"
	[ -n "$DFFY" ] && row "dffy, on the repository, no highlighting" "$RUNS" "" "$DFFY v6.0..v6.1 --paging=never --color=always --no-highlight"
	[ -n "$DFFY" ] && row "dffy, on the repository, highlighted" "$SLOW_RUNS" "" "$DFFY v6.0..v6.1 --paging=never --color=always"
	[ -n "$DELTA" ] && row "delta, no highlighting" "$SLOW_RUNS" "$git_in" "$DELTA --paging=never --syntax-theme=none"
	[ -n "$DELTA" ] && row "delta, highlighted" "$SLOW_RUNS" "$git_in" "$DELTA --paging=never"
	[ -n "$DSF" ] && row "diff-so-fancy" "$SLOW_RUNS" "$git_in" "$DSF"
	[ -n "$DIFFT" ] && row "difftastic, as git's external diff" "${DIFFT_RANGE_RUNS:-1}" "" "DFT_COLOR=always GIT_EXTERNAL_DIFF=$DIFFT git diff v6.0 v6.1"
;; esac

case $SCENARIOS in *renames*)
	echo
	echo "## The range's file list, with rename detection"
	header
	row "git diff --name-status" "$RUNS" "" "git diff --no-ext-diff --name-status v6.0 v6.1"
	if [ -n "$DFFY" ]; then
		row "dffy --name-status" "$RUNS" "" "$DFFY --name-status --color=never v6.0..v6.1"
		git diff --no-ext-diff --name-status v6.0 v6.1 >"$WORK/git.ns"
		"$DFFY" --name-status --color=never v6.0..v6.1 >"$WORK/dffy.ns"
		if cmp -s "$WORK/git.ns" "$WORK/dffy.ns"; then
			echo
			echo "Both lists are identical: $(wc -l <"$WORK/git.ns" | tr -d ' ') entries, $(grep -c '^R' "$WORK/git.ns") of them renames."
		else
			echo
			echo "The lists differ."
		fi
	fi
;; esac

FAIR=kernel/sched/fair.c
TABLE=drivers/net/wireless/realtek/rtw89/rtw8852c_table.c
FILES="$FAIR kernel/exit.c mm/mmap.c mm/vmscan.c $TABLE"
for f in $FILES; do
	mkdir -p "$WORK/old/$(dirname "$f")" "$WORK/new/$(dirname "$f")"
	git show "v6.0:$f" >"$WORK/old/$f"
	git show "v6.1:$f" >"$WORK/new/$f"
done

case $SCENARIOS in *file*)
	old=$WORK/old/$FAIR new=$WORK/new/$FAIR
	echo
	echo "## One large file: $FAIR, v6.0 to v6.1"
	header
	row "GNU diff" "$RUNS" "" "diff -u $old $new"
	row "git diff --no-index" "$RUNS" "" "git diff --no-index --color=always $old $new"
	[ -n "$DFFY" ] && row "dffy" "$RUNS" "" "$DFFY --paging=never --color=always $old $new"
	[ -n "$DELTA" ] && row "delta" "$RUNS" "" "$DELTA --paging=never $old $new"
	[ -n "$DSF" ] && row "diff-so-fancy" "$RUNS" "git diff --no-index --color=always $old $new" "$DSF"
	[ -n "$DFFY" ] && row "dffy --syntax" "$RUNS" "" "$DFFY --syntax --paging=never --color=always $old $new"
	[ -n "$DIFFT" ] && row "difftastic" "$RUNS" "" "DFT_COLOR=always $DIFFT $old $new"
;; esac

case $SCENARIOS in *syntax*)
	echo
	echo "## Syntax-aware diffs of five large files"
	echo
	echo "| File (old → new) | Tool | Time, median ± σ (min–max) | Own memory | Mapped files | What it showed |"
	echo "|---|---|---|---|---|---|"
	for f in $FILES; do
		old=$WORK/old/$f new=$WORK/new/$f
		size="$(( $(wc -c <"$old") / 1024 ))KB → $(( $(wc -c <"$new") / 1024 ))KB"
		if [ -n "$DFFY" ]; then
			cmd="$DFFY --syntax --paging=never --color=never $old $new"
			idle
			bench --warmup 1 --runs "$RUNS" --prepare "$FRESH" "$cmd"
			time=$(stats)
			mem=$(memory "$cmd" | sed 's/ | [^|]*$//')
			sh -c "$FRESH"
			# dffy writes a note into the diff when a region or the whole
			# file kept the line diff ("no syntax diff: ...", "syntax diff: ...").
			note=$(sh -c "$cmd" 2>&1 | grep -m 1 'syntax diff' || echo "syntax diff, nothing fell back")
			echo "| $f ($size) | dffy --syntax | $time | $mem | $note |"
		fi
		if [ -n "$DIFFT" ]; then
			cmd="DFT_COLOR=never $DIFFT $old $new"
			idle
			bench --warmup 1 --runs "$RUNS" "$cmd"
			time=$(stats)
			mem=$(memory "$cmd" | sed 's/ | [^|]*$//')
			# difftastic names a fallback in its file header, after the language.
			note=$(sh -c "$cmd" 2>&1 | head -n 1 | sed 's/.* --- //')
			echo "| $f ($size) | difftastic | $time | $mem | $note |"
		fi
	done
;; esac

# pty NAME RUNS MATCH REFUSAL ARGV...: one viewer row, run in a 200×60 pty
# until MATCH is on screen. A viewer that doesn't show it within 60s gets
# ">60s", and one that shows REFUSAL (if not empty) instead gets "n/a".
pty() {
	name=$1 runs=$2 match=$3 refusal=$4
	shift 4
	idle
	"$PYTHON" -I "$HARNESS" pty --runs "$runs" --prepare "$FRESH" --match "$match" \
		--refusal "$refusal" --timeout 60 -- "$@" >"$WORK/pty"
	# first byte, first frame, settled
	times=$(awk -F '\t' '/_ms\t/ {
		if ($2 == "timeout") printf ">%ss | ", $3
		else if ($2 == "refused" && $1 == "first_frame_ms") printf "n/a (shows \"%s\") | ", $3
		else if ($2 == "refused") printf "n/a | "
		else if ($2 == "busy") printf "still drawing %ss later | ", $3
		else printf "%sms (%s–%s) | ", $2, $3, $4
	}' "$WORK/pty")
	# A pipeline runs under sh -c; the table shows the pipeline.
	cmd="$*"
	echo "| $name | \`$(show "${cmd#sh -c }")\` | $times$(memory_cells "$WORK/pty") |"
	# hunk starts a session daemon on first use, outside the viewer's session.
	if [ -n "$HUNK" ]; then
		pkill -f "$HUNK daemon serve" || true
	fi
}

viewers() {
	echo
	echo "## Interactive viewers: $1"
	echo
	echo "$2"
	echo
	echo "| Viewer | Command | First byte | First frame | Settled | Own memory | Mapped files | git beside it, own / mapped |"
	echo "|---|---|---|---|---|---|---|---|"
}

case $SCENARIOS in *tui*)
	if ! "$PYTHON" -I -c 'import pyte' 2>/dev/null; then
		echo
		echo "Interactive viewers skipped: $PYTHON can't import pyte."
	else
		match=ping_portaddr_for_each_entry_rcu
		viewers "v6.0..v6.1, 200×60 pty" "First frame: the first changed line of the first file (in .clang-format) is on screen. Settled: the last byte before a second with no output."
		pty "harness floor" "$RUNS" "$match" "" sh -c "echo $match; sleep 1.5"
		[ -n "$DFFY" ] && pty "dffy" "$RUNS" "$match" "" "$DFFY" v6.0..v6.1
		[ -n "$LUMEN" ] && pty "lumen" "$SLOW_RUNS" "$match" "" "$LUMEN" diff v6.0..v6.1
		[ -n "$HUNK" ] && pty "hunk" "$SLOW_RUNS" "$match" "" "$HUNK" diff v6.0 v6.1

		# The first added line of the first changed file, as each viewer opens there.
		edit=$(git diff --no-ext-diff | sed -n 's/^+\([^+].*\)$/\1/p' | head -n 1 | cut -c 1-40)
		if [ -z "$edit" ]; then
			echo
			echo "Working tree skipped: the clone has no uncommitted changes."
		else
			viewers "the working tree, 200×60 pty" "Uncommitted changes to $(git diff --no-ext-diff --name-only | wc -l | tr -d ' ') files. First frame: the first added line of the first file is on screen."
			[ -n "$DFFY" ] && pty "dffy" "$RUNS" "$edit" "" "$DFFY"
			[ -n "$LUMEN" ] && pty "lumen" "$RUNS" "$edit" "" "$LUMEN" diff
			[ -n "$HUNK" ] && pty "hunk" "$RUNS" "$edit" "" "$HUNK" diff
		fi

		# hunk 0.25.0 declines rtw8852c_table.c (1.1MB) with this notice.
		large="File too large to render automatically."
		for f in "$FAIR:memory-tiers.h" "$TABLE:rtw89_8852c_phy_radioa_regs"; do
			path=${f%%:*} match=${f#*:}
			old=$WORK/old/$path new=$WORK/new/$path
			viewers "one large file, $path, 200×60 pty" "First frame: the file's first changed line is on screen. Each viewer reads it from the repository with a path filter, and from two extracted files where it can."
			[ -n "$DFFY" ] && pty "dffy" "$RUNS" "$match" "" "$DFFY" v6.0..v6.1 -- "$path"
			[ -n "$DFFY" ] && pty "dffy, two files" "$RUNS" "$match" "" "$DFFY" "$old" "$new"
			[ -n "$LUMEN" ] && pty "lumen" "$RUNS" "$match" "" "$LUMEN" diff v6.0..v6.1 --file "$path"
			[ -n "$LUMEN" ] && echo "| lumen, two files | | n/a | n/a (no two-file mode) | n/a | | | |"
			[ -n "$HUNK" ] && pty "hunk" "$RUNS" "$match" "$large" "$HUNK" diff v6.0 v6.1 -- "$path"
			[ -n "$HUNK" ] && pty "hunk, two files" "$RUNS" "$match" "$large" "$HUNK" diff --files "$old" "$new"
		done

		match=ping_portaddr_for_each_entry_rcu
		git_in="git diff --no-ext-diff v6.0 v6.1"
		viewers "as a pager, v6.0..v6.1, 200×60 pty" "The viewer reads the range's patch from \`$git_in\` on stdin, with the pty as its terminal. First frame: the first changed line of .clang-format is on screen."
		[ -n "$DFFY" ] && pty "dffy" "$RUNS" "$match" "" sh -c "$git_in | $DFFY"
		[ -n "$HUNK" ] && pty "hunk pager" "$SLOW_RUNS" "$match" "" sh -c "$git_in | $HUNK pager"
		[ -n "$LUMEN" ] && echo "| lumen | | n/a | n/a (not a pager: reads revisions, not a patch) | n/a | | | |"
	fi
;; esac
