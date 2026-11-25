# The navigator helper

Go to definition keeps an index of every definition in a version of the diff, which is tens of megabytes on a kernel-sized repository. An allocator keeps memory once it's freed, so the only way to give it back is for the process holding it to exit. So the index lives in a helper process, `dffy __navigate`.

The viewer and `dffy mcp` each start a helper for their repository and speak to it over its stdin and stdout. This page calls them hosts. The helper finds where the definitions and uses are. The viewer still reads and highlights a definition's file itself.

`__navigate` is hidden: it's for dffy itself, not for scripts. This page is its contract. `crates/dffy/tests/fixtures/navigate-protocol/messages.json` holds a request and its answer for each method on a small repository, a push, and the errors. The helper's tests (`crates/dffy/tests/navigate.rs`) hold it to them. The host side's tests (`crates/dffy/src/navigator/tests.rs`) write the same params and read the same answers.

## Lifetime

**Starting.**

- A host starts the helper in the repository's working tree. The helper starts a session and process group of its own.
- It runs through `nice -n 10`, so its index build runs behind the user's own work, when `nice` is in one of `PATH`'s absolute folders and can be run (macOS and Linux). Otherwise it runs as is. The whole process is lowered, because lowering one thread alone would need unsafe code, and dffy uses none.
- It answers `initialize` within about 10ms of starting on the Linux kernel: it finds the repository and nothing else until asked.
- `dffy __navigate --build` prints which build it is and exits. dffy's tests use it to refuse a helper binary left from other sources.
- Each host has its own helper. A viewer and an MCP server on the same repository at once each hold an index, and each builds it. The second build reads most of what the first wrote to the disk cache. One helper per repository would need a socket server of its own for several hosts, which the stdio protocol leaves out.

**Exiting.**

- It exits when its stdin closes (its host went away), on `exit`, and when no request came for 10 minutes and no request or index build runs. The next ask then starts another, which reads back what it built from the disk cache.
- A host doesn't restart a helper by itself when it exits. It starts another on its next ask, at most 3 within a minute. After that it says `definitions unavailable: the helper keeps exiting: …`.
- When the helper exits on a lookup or a search step, the viewer asks another helper once, then says the request failed. That way a name that stops every helper leaves a start for the next one. `dffy mcp` answers the call with the error.
- A helper that can't start is reported the same way, where go to definition answers. Nothing runs in the host's own process instead.
- Each helper a host starts is a new generation. Ids start from 0 in every helper, so a host tags every navigator and search id it keeps with the generation it came from, and never asks a helper for another's id.

**The store file.** The definitions the helper reads go to a store file in the cache's `symbols` folder, unlinked as soon as it's made, and are read back from there by position. The list of which files define each name is there too, written in runs as the index is built. With an index built, the helper takes about 40–60 MB of memory on the Linux kernel.

- The file takes about one encoded copy of each version's definitions the helper opened. That's about 90 MB for the Linux kernel's v6.1 and 50 MB more for v6.0 beside it, since the files both have are shared. It's on top of the disk cache's budget, and goes when the helper exits.
- What dropped navigators held stays in the file. A navigator made when the file holds more of that than of what live navigators hold starts another file, and the old one goes once no navigator holds it. A watched working tree's navigators are made again on each change, and this keeps the files within about twice what the live ones hold.
- Where the file can't be made, written (a full disk) or read back, the definitions read from then on are kept in memory and `note` says so. An index whose names can't be read back is read again into memory, once.
- A process killed before it unlinked its file leaves it behind. A store made an hour later or more removes it.

**The search buffer.** A search for uses reads each file into a buffer it keeps for the next search, as large as the largest file read. On Linux the helper gives it back 30 seconds after the last request, and its memory goes with it. Elsewhere it keeps the buffer, since macOS's allocator would keep counting the freed memory and each search would grow another.

## Messages

JSON-RPC 2.0, one message a line.

- Requests are answered as they finish, not in order. Each runs on a thread of its own, so a lookup doesn't wait for a search's step.
- `$/cancelRequest` with `{"id"}` stops a lookup or a step. The lookup answers error -32800. The step answers at the next file with what it found so far, and leaves the rest for the next step.
- Fields a method doesn't know are left alone.
- Paths are UTF-8 strings. A byte that isn't UTF-8 is replaced.

| Method | Params | Result |
|---|---|---|
| `initialize` | `{"protocol":1,"client":{"name","version"},"threads","cache":{"dir","bytes"},"idle_ms"?}` | `{"protocol":1,"capabilities":{"methods":[…]}}` |
| `open` | `{"snapshots":[S,S],"side":"old"\|"new","whitespace":"exact"\|"ignore_all"\|"ignore_change"}` | `{"navigator","fit","fingerprint","languages","progress"}` |
| `lookup` | `{"navigator","name","origin":{"path","language"},"at"?}` | `{"candidates":[C…],"total","progress"}` |
| `progress` | `{"navigator"}` | `{"progress","building","error","skipped","note"}` |
| `search/start` | `{"navigator","kind":"references"\|"text","name","origin","cap"?,"calls_only"?,"in_diff"?}` | `{"search"}` |
| `search/step` | `{"search","budget_ms","until"?,"past_cap"?}` | `{"hits":[…],"files","searched","done","indexed"}` |
| `search/close` | `{"search"}`, a notification | none |
| `shutdown`, then `exit` | `{}` | `null`; `exit` is a notification |

The helper sends one notification, `progress`, with `{"navigator","progress","building","error","skipped","note"}`, the same as `progress` answers. It sends it as its build of that navigator's index starts, at most 4 times a second while it runs, and once when it ends, with `building` false, whether it finished or stopped for another's build. A host shows how far an index is from these alone, and asks nothing while no build runs.

### `initialize`

The repository is the one the helper runs in. Another protocol is refused with error -32000.

- `threads` is how many threads an index build uses: 1 for the viewer, 2 for the MCP server.
- `cache` is the disk cache the index is kept in. A host with the cache off doesn't start the helper.
- `idle_ms` replaces the 10 minutes.

### `open`

A snapshot `S` is `{"kind":"tree","id"}`, `{"kind":"index"}` or `{"kind":"worktree"}`, as `--json`'s header has them. The helper lists what differs between the two. It keeps the navigator it has for that side while the core says it still fits ([architecture.md](architecture.md), `Navigator::refit`). A host opens again whenever its diff's list was read again.

- `fit` is `same`, `changes` (it took the diff's changes again) or `new` (made again, and its `navigator` id is new).
- `fingerprint` is what it answers from, in hex. A host keys what it keeps of its answers on it.
- `progress` is `{"done","total"}` in files.

The helper keeps 2 navigators, the least recently used dropped first. It builds only the index of the one used last: opened, looked up in or searched. Another's build stops, and goes on once it's used again.

### `lookup`

- `origin` is the asking file and its language's name (`c`, `rust`).
- `at`, `{"line","column"}`, is where the name is used in it, 0-based. The column is a byte of the line, or null for the name's first use in code on the line. Given `at`, what the use itself names comes first: a local variable, a parameter or the import that brings the name in. An import under another name is followed to what it names.

A candidate `C` is `{"path","file","kind","start","end","container","rank","changed","in_hunk"}`:

- Lines are 0-based.
- `file` is where it's read: `{"kind":"blob","id","mode"}`, `{"kind":"worktree","mode","id"}` (`id` null when the scan didn't hash it) or `{"kind":"path","mode","root"}` (outside git, which no navigator lists).
- `kind` is a definition kind's name, or null for a line using the name. `local`, `parameter` and `import` come only from a lookup with `at`, in the asking file.
- `rank` is `same_file`, `imported`, `near` or `elsewhere`.
- `in_hunk` is null where it wasn't worked out.

### `progress`

- `progress`: how far the index is built.
- `building`: whether its build runs. An incomplete index with no build running waits while another's is built.
- `error`: why a language has no definitions, which is a tags query that doesn't compile, else null.
- `skipped`: `{"too_large","timed_out","unreadable"}`, the files built so far left without definitions.
- `note`: why the index is kept in memory rather than in the store file ([Lifetime](#lifetime)), else null.

### `search/start`

It starts a search for the uses of a name (`references`), or for its whole-word text matches (`text`, at most `cap` lines, 50 by default).

A search for uses can be narrowed. With `calls_only` it keeps only the lines calling the name (followed by its arguments, past a Rust macro's `!` or type arguments). With `in_diff` it keeps only those in files the diff changes. A file counts when it has a line kept.

The helper keeps 8 searches. One of a navigator it dropped goes too.

### `search/step`

It searches the search's next files, for about `budget_ms`, at least one file. With `until`, it stops once it has that many lines in all. A search for uses lists the lines of its first 200 files with one and counts the rest. With `past_cap` false it stops at those 200 files instead of counting.

- A hit of `references` is a candidate with `"text"` (the line, trimmed, at most 200 bytes) and `"more"` (on a file's last line listed, its lines past 1000). A hit of `text` is a candidate.
- `files` counts the files with a use so far (0 for `text`).
- `searched` counts the side's files searched so far.
- `done` says nothing more comes.
- `indexed` says the navigator's index was whole when the step started, so the definitions' own lines were left out.

### Errors

The message is the reason.

| Code | When |
|---|---|
| -32700 | A line that isn't a JSON object (with a null id, which couldn't be read) |
| -32600 | A line with an id but no method |
| -32601 | A method the helper doesn't have |
| -32602 | Params it can't read |
| -32002 | A request before `initialize` |
| -32001 | A navigator or search it no longer holds (open or start it again) |
| -32800 | Cancelled |
| -32000 | Anything else, a request that failed inside the helper included |
