# Control API

The control API drives a running viewer from outside. `dffy ctl` is its command-line client.

```sh
dffy ctl open src/lib.rs:42        # show line 42 of src/lib.rs
dffy ctl selection                 # the lines selected with v, and their text
dffy ctl comment src/lib.rs:42 why does this clamp?
```

A client can show a file at a line, focus a comment thread, add a comment, apply a thread's suggestion, and read what's selected and what the viewer shows. Editors, scripts and coding agents use it through `dffy ctl` ([agents.md](agents.md)), the [MCP server's viewer tools](mcp.md#viewer-tools), or the protocol below.

It's Unix only. The transport is a Unix socket, and Windows isn't supported.

## `dffy ctl`

```
$ dffy ctl views
view 2  pid 48122  uncommitted  /home/me/src/dffy
view 3  pid 48310  main...HEAD  /home/me/src/linux
$ dffy ctl --view 3 state
view 3  kernel/sched/core.c  line 1204  main...HEAD
```

| Command | What it does |
|---|---|
| `dffy ctl views` | Lists the running viewers: id, pid, what each compares, and its working tree. |
| `dffy ctl open <path>[:<line>[:old]]` | Shows a file of the diff at a line of its new side, or of its old side with `:old`. Without a line, the file opens as it would from the file tree. |
| `dffy ctl focus <id>` | Shows and focuses the thread holding comment `<id>`. `dffy comments --json` lists ids. |
| `dffy ctl selection` | Prints where the lines selected with `v` are (`src/lib.rs:12-14 new`), then their text. |
| `dffy ctl state` | Prints the file shown, the line at the top of the screen, and what the viewer compares. |
| `dffy ctl comment <path>:<line>[:old] <text>…` | Adds a comment on that line and shows it. The words are joined by spaces. |
| `dffy ctl apply-suggestion <id>` | Applies the suggestion of comment `<id>`'s thread to the working tree, as `s` on the thread does, and prints which lines it replaced ([Applying suggestions](#applying-suggestions)). |
| `dffy ctl schema` | Prints the protocol's JSON Schema. Needs no viewer. |

| Option | Effect |
|---|---|
| `-C <path>` | Use the viewer in this working tree instead of the current directory's. |
| `--view <id>` | Use this viewer. Without it, the command goes to the one viewer in this working tree, and fails listing the candidates when there are none or several. |
| `--json` | Print the protocol's result object instead of text. |

- Paths are relative to the current directory, as in git, and must be inside the viewer's working tree.
- `open` and `comment` on a file whose diff isn't loaded yet show the file and answer once the diff is in.
- A command fails after 5 seconds without an answer. The viewer can't answer while an editor opened with `e` is running.
- Errors exit with status 128 and say what's wrong: no viewer running, a file not in the diff, a line the file doesn't have.

## How it works

dffyd, the [watch broker](watch.md#dffyd), serves the API on a second socket, `control-v1.sock`. It lives in the same private directory: `$XDG_RUNTIME_DIR/dffy`, else `$TMPDIR/dffy-<uid>`.

- Every viewer of a working tree registers with dffyd after its first frame, starting dffyd if needed. It stays registered until it exits or its connection closes. A diff piped on stdin and files or folders compared outside git don't register.
- One dffyd serves all your viewers, in every repository. A command picks the viewer in its working tree, or the one `--view` names.
- dffyd forwards each request to its viewer and the answer back. The viewer runs it between frames, as it would a key press.
- When a client disconnects before its answer arrives, dffyd tells the viewer, which drops the request. `dffy ctl` disconnects after 5 seconds.
- Clients never start dffyd. With no dffyd, no viewer is running, and `dffy ctl` and the MCP tools say so.

### Comments a client adds

`comments/add` writes through the viewer, the same way `c` does, to the comments file the viewer shows: the local review's or a review session's. dffy stays the file's only writer.

- The comment's author is the client's name from its hello, or `dffy ctl` when the hello gives none. It's never yours. In a review session it's [private](comments.md#drafts) and isn't posted until `A` on its thread makes it yours.
- A comment that starts with `@<name>` never asks an agent, unlike one typed in the viewer ([viewer.md](viewer.md#asking-an-agent)).
- It's refused while a post runs, as the viewer's own edits are.

### Applying suggestions

`comments/applySuggestion` applies a suggestion as `s` on the thread does ([viewer.md](viewer.md#applying-a-suggestion)), against the diff the viewer shows. It uses the suggestion of comment `id`, or, when that comment has none, of the last comment in its thread that has one. It fails with `failed` and writes nothing wherever `s` refuses, for example:

- the anchored lines in the working tree changed since the suggestion was written;
- the thread is on old-side lines, or on a whole file;
- the file's diff isn't loaded yet (open it first);
- the review is someone else's and isn't checked out.

Two refusals are the API's own:

- The suggestion is already applied (`this suggestion is applied already`). `s` would take it back, but the API never does, so one client can't undo another's apply.
- The viewer is asking an agent (`an agent ask is running; apply with s in the viewer`). That agent may reach this API through dffy's MCP server, and it's meant to only read.

## Protocol

Each message is one line of JSON. Lines over 1MB are refused.

### Handshake

The client's first line names the protocol version and the client. Comments the client adds carry that name, up to 64 characters. dffyd answers with its capabilities:

```json
{"version":1,"client":"my-editor 0.3"}
{"version":1,"server":"dffyd","build":"0.2.1+c2500b15","capabilities":{"methods":["views/list","views/register","subscribe","view/state","view/selection","view/open","view/focusThread","comments/add","comments/applySuggestion"],"events":["selection/changed","view/changed"]}}
```

A hello with another version gets `{"error":{"code":"unsupported_version",…},"versions":[1]}`, and dffyd hangs up. The socket name carries the version, so dffyds of other versions serve their own sockets.

### Messages

| Message | Shape |
|---|---|
| Request | `{"id":…,"method":…,"params":{…}}`. The id is a number or a string. |
| Answer | `{"id":…,"result":{…}}` or `{"id":…,"error":{"code":…,"message":…}}` |
| Notification | `{"method":…,"params":{…}}`, without an id. |

Either side ignores fields it doesn't know, so new fields don't need a new version. A connection may send many requests, and answers can arrive in any order.

### Requests

Lines are 1-based. Every request on a view names it with `view`, the id from `views/list`, and its result echoes `view`.

| Method | Params | Result |
|---|---|---|
| `views/list` | | `{"views":[{"view","pid","worktree","repository","diff"}]}`. `repository` is the git directory the worktrees share. `diff` is what the view compares. |
| `view/state` | `view` | `{"view","path","old_path","top":{"old","new"},"files","listing","layout","diff","revisions"}`. `path` is the file shown, null in an empty view. `top` is the line numbers of the first code row on screen. `files` counts the listed files, and `listing` says whether listing is still going. `layout` is `split` or `unified`. |
| `view/selection` | `view` | `{"view","selection":{"path","side","start","end","text"}}`, or `selection: null` with nothing selected. Only lines selected in code count, not text selected in a comment. |
| `view/open` | `view`, `path`, `line`?, `side`? (`new`) | `{"view","path","line","side"}`, once the file shows. A line hidden in a gap is revealed with a few lines around it. |
| `view/focusThread` | `view`, `id` | `{"view","thread","path","focused"}`. `thread` is the thread's first comment. `focused` is false until the thread's place in the diff has loaded. |
| `comments/add` | `view`, `path`, `line`, `side`? (`new`), `text` | `{"view","id","path","line","side"}`. `id` is the new comment's. |
| `comments/applySuggestion` | `view`, `id` | `{"view","thread","comment","path","start","end","lines"}`, once written. `comment` is the comment whose suggestion went in. `start` and `end` are the replaced lines as the file was. `lines` is how many lines replaced them, 0 for a deletion. |
| `subscribe` | `view`, `events` | `{"view","events"}`. The events are pushed to this connection until it closes. |

### Events

A subscriber gets each event once right after subscribing, with the current state. After that, an event is pushed only when its content changed, 50ms after the view stops changing.

| Notification | Params |
|---|---|
| `selection/changed` | `{"view","selection"}`, as `view/selection` answers. |
| `view/changed` | `view/state`'s result, when the file or the top row moved. |
| `view/closed` | `{"view"}`. The viewer exited, and nothing more comes for it. |

### Errors

| Code | When |
|---|---|
| `parse_error`, `invalid_request` | The line isn't JSON, or isn't a request, an answer or a notification. |
| `unsupported_version` | The hello names a version dffyd doesn't serve. |
| `unknown_method` | No such method. |
| `invalid_params` | A param is missing or has the wrong type. The message says which. |
| `no_view` | No view has that id. |
| `viewer_gone` | The viewer exited before it answered. |
| `not_found` | The file isn't in the diff, the line isn't in the file, or the comment isn't among those the view shows. |
| `failed` | It couldn't be done, for example because the viewer has no comments file. The message says why. |

### The viewer's side

A viewer sends `views/register` with `pid`, `worktree`, `repository` and `diff`, once per connection, and gets `{"view":<id>}` back. dffyd then sends it:

- the requests on its view, with dffyd's own ids, and the client's name as `client` beside `params` when the hello gave one;
- `$/cancelRequest` (`{"id":…}`) for a request whose client left before the answer, which the viewer then skips;
- `subscriptions` (`{"events":[…]}`) whenever the set of subscribed events changes.

The viewer answers the requests and pushes the events listed.

`dffy ctl schema` prints the JSON Schema of every message.

## Security

- The runtime directory must be yours and closed to everyone else (mode `0700`), as for the [broker](watch.md#dffyd). The socket is mode `0600`.
- On Linux (`SO_PEERCRED`) and macOS (`getpeereid`), dffyd also checks each connection's user and hangs up on any other user, or one it can't identify. Elsewhere the file modes are the only check.
- A client can do what you can do in the viewer: move the view, read what it shows and what's selected, add comments under its own name, and apply a suggestion already in a thread. It can't run programs, write other files or text, post a review, or get its comments posted before you make them yours.
- Nothing is exposed over the network.
