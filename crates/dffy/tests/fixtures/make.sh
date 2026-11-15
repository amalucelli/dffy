#!/usr/bin/env bash
# Builds a small repo with every change kind the pager parses and writes
# diff fixtures into $1. Dates, names and config are pinned so the output,
# commit ids included, is the same on every run:
#   crates/dffy/tests/fixtures/make.sh crates/dffy/tests/fixtures
set -euo pipefail
out=$1
work=$(mktemp -d)
cd "$work"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME="Ada Lovelace" GIT_AUTHOR_EMAIL="ada@example.com"
export GIT_COMMITTER_NAME="Ada Lovelace" GIT_COMMITTER_EMAIL="ada@example.com"
export GIT_AUTHOR_DATE="2024-03-01T12:00:00+0100" GIT_COMMITTER_DATE="2024-03-01T12:00:00+0100"
g() { git -c commit.gpgsign=false -c core.pager=cat -c diff.renames=true "$@"; }
g init -q -b main

cat > calc.c <<'EOF'
#include <stdio.h>

static int add(int a, int b)
{
	return a + b;
}

int main(void)
{
	int total = add(1, 2);
	printf("total: %d\n", total);
	return 0;
}
EOF
printf 'first line\nsecond line' > notes.txt
cat > old_name.py <<'EOF'
def greet(name):
    print("hello", name)


def farewell(name):
    print("goodbye", name)


if __name__ == "__main__":
    greet("world")
    farewell("world")
EOF
printf '#!/bin/sh\necho build\n' > build.sh
printf '\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR' > logo.png
printf 'obsolete\n' > gone.txt
g add -A && g commit -q -m "Initial commit"

cat > calc.c <<'EOF'
#include <stdio.h>

static long add(long a, long b)
{
	return a + b;
}

int main(void)
{
	long total = add(1, 2);
	printf("total: %ld\n", total);
	printf("done\n");
	return 0;
}
EOF
printf 'first line\nsecond line, edited\n' > notes.txt
g mv old_name.py greetings.py
sed -i 's/goodbye/bye/' greetings.py
chmod +x build.sh
printf '\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x01' > logo.png
g rm -q gone.txt
printf 'a brand new file\n' > added.md
g add -A && g commit -q -m "Widen calc to long and tidy files" -m "Body paragraph explaining why."

export GIT_AUTHOR_DATE="2024-03-02T09:30:00+0100" GIT_COMMITTER_DATE="2024-03-02T09:30:00+0100"
printf 'first line\nsecond line, edited\nthird line\n' > notes.txt
g commit -q -am "Add a third line to the notes"

g diff HEAD~2 HEAD~1 > "$out/git-diff.diff"
g -c color.ui=always diff HEAD~2 HEAD~1 > "$out/git-diff-color.diff"
g show HEAD~1 > "$out/git-show.diff"
g -c color.ui=always log -p -2 --decorate > "$out/git-log-color.diff"

# A merge with a conflict resolved by hand gives a combined diff.
g checkout -q -b side HEAD~1
printf 'first line\nsecond line, edited\nside line\n' > notes.txt
g commit -q -am "Side change"
g checkout -q main
g merge -q side 2>/dev/null || true
printf 'first line\nsecond line, edited\nthird line\nside line\n' > notes.txt
g add notes.txt && g commit -q -m "Merge branch side"
g show HEAD > "$out/git-show-merge.diff"

mkdir -p a b
printf 'one\ntwo\nthree\n' > a/f.txt
printf 'one\n2\nthree\nfour\n' > b/f.txt
(TZ=UTC touch -d '2024-03-01 12:00:00' a/f.txt b/f.txt; TZ=UTC diff -u a/f.txt b/f.txt) > "$out/diff-u.diff" || true
rm -rf "$work"
