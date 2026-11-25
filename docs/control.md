# Control API

The control API drives a running viewer from outside. `dffy ctl` is its command-line client.

```sh
dffy ctl open src/lib.rs:42        # show line 42 of src/lib.rs
dffy ctl selection                 # the lines selected with v, and their text
dffy ctl comment src/lib.rs:42 why does this clamp?
```

A client can show a file at a line, focus a comment thread, add a comment, apply a thread's suggestion, read what's selected and what the viewer shows, and wait for the threads the viewer hands over. Editors, scripts and coding agents use it through `dffy ctl` ([agents.md](agents.md)), `dffy comments listen` ([comments.md](comments.md#listening-dffy-comments-listen)), the [MCP server's viewer tools](mcp.md#viewer-tools), or the protocol below.

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
| `dffy ctl open <path>[:<line>[:old]]` | Shows a file at a line of its new side, or of its old side with `:old`. Without a line, the file opens as it would from the file tree. A file the diff doesn't change opens whole at that side, as from the [picker](viewer.md#opening-files), and the output says `(not in the diff)`. |
| `dffy ctl focus <id>` | Shows and focuses the thread holding comment `<id>`. `dffy comments --json` lists ids. |
| `dffy ctl selection` | Prints where the lines selected with `v` are (`src/lib.rs:12-14 new`), then their text. |
| `dffy ctl state` | Prints the file shown, the line at the top of the screen, and what the viewer compares. |
| `dffy ctl comment <path>:<line>[:old] <text>…` | Adds a comment on that line and shows it, opening a file the diff doesn't change as `open` does. The words are joined by spaces. |
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
- Errors exit with status 128 and say what's wrong: no viewer running, no file at that path and side, a line the file doesn't have.

## How it works

dffyd, the [watch broker](watch.md#dffyd), serves the API on a second socket, `control-v1.sock`. It lives in the same private directory: `$XDG_RUNTIME_DIR/dffy`, else `$TMPDIR/dffy-<uid>`.

- Every viewer of a working tree registers with dffyd after its first frame, starting dffyd if needed. It stays registered until it exits. When it can't register, or dffyd goes, it tries again, starting dffyd if needed and waiting longer each time, up to 30 seconds. The status bar says once that it can't. A diff piped on stdin and files or folders compared outside git don't register.
- One dffyd serves all your viewers, in every repository. A command picks the viewer in its working tree, or the one `--view` names.
- dffyd forwards each request to its viewer and the answer back. The viewer runs it between frames, as it would a key press.
- When a client disconnects before its answer arrives, dffyd tells the viewer, which drops the request. `dffy ctl` disconnects after 5 seconds.
- `dffy comments listen` starts dffyd if needed, since it waits for viewers that may open later. The other clients never do: with no dffyd, no viewer is running, and `dffy ctl` and the MCP tools say so.

### Files outside the diff

`view/open` and `comments/add` open any file of the repository, not only the diff's. A path the diff doesn't list is looked up at the diff's side `side` names, and opens whole as it is there, after the diff's files, as the [picker](viewer.md#opening-files) opens it. Asked for at the other side, an open one is replaced. The answer has `"listed":false`, a field older clients ignore, so the protocol version stays 1.

It's `not_found` when:

- no file is at that path at that side: nothing there, a folder, or a submodule;
- the diff was piped, or compares two files or folders outside git, which have no other files;
- the viewer is still listing the diff's files, as the path may be among them. Ask again once it's done.

At the working tree, the files are those the index tracks, by their exact path, as the picker lists them: an ignored or untracked file the diff doesn't list isn't found, nor a tracked one named in another case. A path leaving the working tree, or going into `.git`, is never a file there.

A binary or very large file opens, showing why it isn't shown. A line in it is `not_found` (`logo.png is binary; no lines`).

`comments/add` then adds its comment on the file opened, which the comments file keeps as it keeps any comment on a file outside the diff ([viewer.md](viewer.md#files-with-comments)). When the file isn't found, nothing is written.

A file found stays open even when the request then fails: a line it doesn't have, or a comment that couldn't be written. A request waiting for its file fails (`failed`) when the file is opened at the other version before it shows.

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

### Listening for threads

A coding agent's session asks `comments/listen` and gets an answer when the user hands it threads in a viewer ([viewer.md](viewer.md#sending-to-a-running-session)). `dffy comments listen` is the client to use ([comments.md](comments.md#listening-dffy-comments-listen)).

- dffyd keeps the request and hands it to every view of that working tree, those registered and those that register later. The first view to answer with a prompt wins, and dffyd calls off the others' copies.
- A view that closes, or that answers with an error or without a prompt, leaves the request waiting for another view.
- A connection waits with one request at a time, in the working tree it names. A second request before the answer is refused (`invalid_request`). After an answer, the client asks again on the same connection.
- The connection is the listener. dffyd stamps when it first asked, and when its session last edited and was last prompted, until the connection closes.
- `comments/listening` reports that activity by `key`, for every listener with that key. The key is the client's own, and dffyd doesn't interpret it. The views holding the listeners are told with `listener/changed`.
- The name a viewer shows for a listener is the client's name from its hello.

## Protocol

Each message is one line of JSON. Lines over 1MB are refused.

### Handshake

The client's first line names the protocol version and the client. Comments the client adds carry that name, up to 64 characters. dffyd answers with its capabilities:

```json
{"version":1,"client":"my-editor 0.3"}
{"version":1,"server":"dffyd","build":"0.2.1+c2500b15","capabilities":{"methods":["views/list","views/register","subscribe","view/state","view/selection","view/open","view/focusThread","comments/add","comments/applySuggestion","comments/listen","comments/listening"],"events":["selection/changed","view/changed"]}}
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

Lines are 1-based. Times are seconds since 1970. Every request on a view names it with `view`, the id from `views/list`, and its result echoes `view`.

| Method | Params | Result |
|---|---|---|
| `views/list` | | `{"views":[{"view","pid","worktree","repository","diff"}]}`. `repository` is the git directory the worktrees share. `diff` is what the view compares. |
| `view/state` | `view` | `{"view","path","old_path","top":{"old","new"},"files","listing","layout","diff","revisions"}`. `path` is the file shown, null in an empty view. `top` is the line numbers of the first code row on screen. `files` counts the listed files, and `listing` says whether listing is still going. `layout` is `split` or `unified`. |
| `view/selection` | `view` | `{"view","selection":{"path","side","start","end","text"}}`, or `selection: null` with nothing selected. Only lines selected in code count, not text selected in a comment. |
| `view/open` | `view`, `path`, `line`?, `side`? (`new`) | `{"view","path","line","side","listed"?}`, once the file shows. A line hidden in a gap is revealed with a few lines around it. A file the diff doesn't list opens whole at `side`, and the result then has `"listed":false` ([Files outside the diff](#files-outside-the-diff)). |
| `view/focusThread` | `view`, `id` | `{"view","thread","path","focused"}`. `thread` is the thread's first comment. `focused` is false until the thread's place in the diff has loaded. |
| `comments/add` | `view`, `path`, `line`, `side`? (`new`), `text` | `{"view","id","path","line","side","listed"?}`. `id` is the new comment's. A file the diff doesn't list opens as for `view/open`, and `listed` says so. |
| `comments/applySuggestion` | `view`, `id` | `{"view","thread","comment","path","start","end","lines"}`, once written. `comment` is the comment whose suggestion went in. `start` and `end` are the replaced lines as the file was. `lines` is how many lines replaced them, 0 for a deletion. |
| `comments/listen` | `worktree`, `key`? | `{"view","prompt"}`, once a viewer of `worktree` hands threads over, however long that takes. `worktree` is the working tree's canonical root, as `views/list` has it. `prompt` is what to do, as the user would say it to an agent. |
| `comments/listening` | `key`, `edited` | `{"listeners"}`, how many listeners have that key. `edited` is true when the session just edited a file, false when it was just prompted. |
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
| `unknown_method` | No such method, as a method newer than the running dffyd. |
| `invalid_params` | A param is missing or has the wrong type. The message says which. |
| `no_view` | No view has that id. |
| `viewer_gone` | The viewer exited before it answered. |
| `not_found` | No file is at that path and side, the line isn't in the file, or the comment isn't among those the view shows. |
| `failed` | It couldn't be done, for example because the viewer has no comments file. The message says why. |

### The viewer's side

A viewer sends `views/register` with `pid`, `worktree`, `repository` and `diff`, once per connection, and gets `{"view":<id>}` back. dffyd then sends it:

- the requests on its view, with dffyd's own ids, and the client's name as `client` beside `params` when the hello gave one;
- `comments/listen` for each listener in its working tree, after the answer to `views/register`. Beside the client's params it has `listener`, a number that's the same each time that connection asks, and `started`, `edited` and `prompted`, the last two null before the first. The viewer holds it and answers `{"prompt":"…"}` when the user hands that listener threads;
- `listener/changed` (`{"id","started","edited","prompted"}`) when the session of the listener it holds as request `id` edited or was prompted;
- `$/cancelRequest` (`{"id":…}`) for a request whose client left before the answer, or that another view answered, which the viewer then skips;
- `subscriptions` (`{"events":[…]}`) whenever the set of subscribed events changes.

The viewer answers the requests and pushes the events listed.

`dffy ctl schema` prints the JSON Schema of every message.

## Security

- The runtime directory must be yours and closed to everyone else (mode `0700`), as for the [broker](watch.md#dffyd). The socket is mode `0600`.
- On Linux (`SO_PEERCRED`) and macOS (`getpeereid`), dffyd also checks each connection's user and hangs up on any other user, or one it can't identify. Elsewhere the file modes are the only check.
- A client can do what you can do in the viewer: move the view, open any file of the repository in it, read what it shows and what's selected, add comments under its own name, and apply a suggestion already in a thread. It can also wait for the threads you hand over, which name thread ids and a comments file. It can't run programs, write other files or text, post a review, or get its comments posted before you make them yours.
- A session that listens takes a prompt from any view, and any program of yours can register one. A session should act on it only as far as its own permissions and rules for hand-overs allow ([agents.md](agents.md#claude-code)).
- Nothing is exposed over the network.
