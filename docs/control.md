# Control API

A running viewer can be driven from outside: shown a file at a line, sent to a comment thread, given a comment, asked to apply a thread's suggestion, and asked what's selected and what it shows. Editors, scripts and coding agents use it through `dffy ctl`, the [MCP server's](mcp.md#viewer-tools) viewer tools, or the protocol below.

```sh
dffy ctl open src/lib.rs:42        # show line 42 of src/lib.rs
dffy ctl selection                 # the lines selected with v, and their text
dffy ctl comment src/lib.rs:42 why does this clamp?
```

Unix only: the transport is a Unix socket. Windows isn't supported.

## How it works

- **dffyd serves it.** The [watch broker](watch.md#dffyd) process also listens on a control socket, `control-v1.sock`, in the same private directory: `$XDG_RUNTIME_DIR/dffy`, else `$TMPDIR/dffy-<uid>`.
- **Viewers register.** Every viewer of a working tree registers with dffyd once its first frame is out, starting dffyd if none runs. It stays registered until it exits; a viewer that's killed leaves the list too, as its connection closes. A diff piped on stdin, and files or folders compared outside git, don't register.
- **One per user.** Every viewer you run, in any repository, is in one list. A command picks the viewer in the working tree it runs in (`-C <path>` or the current directory), or the one `--view <id>` names.
- **The viewer decides.** dffyd hands each request to its viewer and the answer back; the viewer carries it out between frames, as it would a key. Nothing runs in a viewer that isn't asked, and nothing at all while nobody subscribes to its events.
- **Comments are written by the viewer.** `comments/add` adds the comment as `c` would, through the same write, to the comments file the viewer shows: the local review's, or a review session's. dffy stays the file's only writer. The comment is the client's, by the name its hello gives (`dffy ctl` when it gives none), not yours: in a review session it's [not posted](comments.md#drafts) until `A` on its thread makes it yours. One that starts with `@<name>` never asks an agent, as one you write in the viewer does ([viewer.md](viewer.md#asking-an-agent)). While a post runs, it's refused, as the viewer's own edits are.
- **Suggestions are applied by the viewer.** `comments/applySuggestion` does what `s` on the thread does ([viewer.md](viewer.md#applying-a-suggestion)), with the same refusals, against the diff the viewer shows: the working tree's file is written only when the anchored lines are still as the suggestion found them, and on someone else's review only once it's checked out. While the viewer asks an agent, it's refused (`an agent ask is running; apply with s in the viewer`): an agent that's only to read may reach the control API through dffy's MCP server, and applying writes files.
- **A request given up on is called off.** When a client's connection closes before its answer, as `dffy ctl` closes it after 5 seconds, dffyd tells the viewer, which drops the request instead of doing it late.
- **Clients never start dffyd.** When none runs, no viewer does, and `dffy ctl` and the MCP tools say so.

## `dffy ctl`

| Command | What it does |
|---|---|
| `dffy ctl views` | Lists the running viewers: id, pid, what each compares and its working tree. |
| `dffy ctl open <path>[:<line>[:old]]` | Shows a file of the diff, at a line of its new side, or of its old side with `:old`. Without a line, the file opens as it would from the tree. |
| `dffy ctl focus <id>` | Shows and focuses the comment thread holding comment `<id>` (`dffy comments --json` lists ids). |
| `dffy ctl selection` | Prints where the lines selected with `v` are (`src/lib.rs:12-14 new`), then their text. |
| `dffy ctl state` | Prints the file shown, the line at the top of the screen and what the viewer compares. |
| `dffy ctl comment <path>:<line>[:old] <text>…` | Adds a comment on that line, its words joined by spaces, and shows it. |
| `dffy ctl schema` | Prints the protocol's JSON Schema. It needs no viewer. |

- **Paths** are relative to the current directory, as git's are, and must be in the viewer's working tree.
- **`--view <id>`** picks a viewer; without it, the command goes to the one viewer in this working tree. With none there, or several, it fails and says which there are.
- **`--json`** prints the protocol's result object instead of text.
- **Waiting:** `open` and `comment` on a file whose diff isn't loaded yet show the file and answer once its diff is in. An answer that doesn't come within 5 seconds fails: the viewer's loop is held while the editor `e` opened runs.
- **Errors** exit with 128 and say what's wrong: no viewer running, a file not in the diff, a line the file doesn't have.

```
$ dffy ctl views
view 2  pid 48122  uncommitted  /home/me/src/dffy
view 3  pid 48310  main...HEAD  /home/me/src/linux
$ dffy ctl --view 3 state
view 3  kernel/sched/core.c  line 1204  main...HEAD
```

## Protocol

Each message is one line of JSON (newline-delimited). Lines over 1MB are refused.

**Handshake.** The client's first line names the version, and the client: comments it adds are by that name, up to 64 characters. dffyd answers with its capabilities:

```json
{"version":1,"client":"my-editor 0.3"}
{"version":1,"server":"dffyd","build":"0.2.1+c2500b15","capabilities":{"methods":["views/list","views/register","subscribe","view/state","view/selection","view/open","view/focusThread","comments/add","comments/applySuggestion"],"events":["selection/changed","view/changed"]}}
```

A hello of another version gets `{"error":{"code":"unsupported_version",…},"versions":[1]}` and dffyd hangs up. The socket's name carries the version too, so dffyds of other versions serve sockets of their own.

**Messages.** After the handshake:

- a request is `{"id":…,"method":…,"params":{…}}`, its id a number or a string;
- its answer is `{"id":…,"result":{…}}` or `{"id":…,"error":{"code":…,"message":…}}`;
- a notification is `{"method":…,"params":{…}}`, without an id.

Fields either side doesn't know are ignored, so new ones can come without a new version. A connection may send many requests, and answers can come in another order than the requests did.

**Requests.** Lines are 1-based. Every request on a view names it with `view`, the id `views/list` gives, and its result says `view` too.

| Method | Params | Result |
|---|---|---|
| `views/list` | | `{"views":[{"view","pid","worktree","repository","diff"}]}`; `repository` is the git directory the worktrees share, `diff` what the view compares. |
| `view/state` | `view` | `{"view","path","old_path","top":{"old","new"},"files","listing","layout","diff","revisions"}`: the file shown (null in an empty view), the lines of the first row of code on screen, how many files are listed and whether listing goes on, `split` or `unified`. |
| `view/selection` | `view` | `{"view","selection":{"path","side","start","end","text"}}`, or `selection: null` with nothing selected. Lines selected in code count; text selected in a comment doesn't. |
| `view/open` | `view`, `path`, `line`?, `side`? (`new`) | `{"view","path","line","side"}`, once the file shows. A line a gap hides is revealed with a few lines around it. |
| `view/focusThread` | `view`, `id` | `{"view","thread","path","focused"}`: the thread's first comment and its file. `focused` is false while the thread's place is on its way; it's focused once there. |
| `comments/add` | `view`, `path`, `line`, `side`? (`new`), `text` | `{"view","id","path","line","side"}`: the new comment's id. |
| `comments/applySuggestion` | `view`, `id` | `{"view","thread","comment","path","start","end","lines"}`, once it's written: the comment whose suggestion went in (`id`'s, or with none, the last in its thread with one), the lines it replaced as the file was, and how many went in their place (0 deleted them). Refused with `failed` and why, writing nothing. |
| `subscribe` | `view`, `events` | `{"view","events"}`. The events are pushed to this connection until it closes. |

**Events.** Pushed to subscribers once right after they subscribe, with what's there then, and from then on 50ms after the view stops changing, only when what they say changed:

| Notification | Params |
|---|---|
| `selection/changed` | `{"view","selection"}`, as `view/selection` answers. |
| `view/changed` | `view/state`'s result: the file or the top row moved. |
| `view/closed` | `{"view"}`: the viewer left; nothing more comes for it. |

**Errors.** `code` is one of:

| Code | When |
|---|---|
| `parse_error`, `invalid_request` | The line isn't JSON, or isn't a request, an answer or a notification. |
| `unknown_method` | No such method. |
| `invalid_params` | A param is missing or of the wrong kind; the message says which. |
| `no_view` | No view has that id. |
| `viewer_gone` | The viewer exited before it answered. |
| `not_found` | The file isn't in the diff, the line isn't in the file, or the comment isn't in the comments the view shows. |
| `failed` | It couldn't be done, as when the viewer has no comments file; the message says why. |

**The viewer's side.** A viewer sends `views/register` with `pid`, `worktree`, `repository` and `diff`, once per connection, and is answered `{"view":<id>}`. dffyd then sends it the requests on its view, with ids of dffyd's own and the asking client's name as `client` beside `params`, when its hello gave one; the notification `$/cancelRequest` (`{"id":…}`) for a request whose client left before the answer, which the viewer then doesn't do; and the notification `subscriptions` (`{"events":[…]}`) whenever what's subscribed to changes. The viewer answers the requests and pushes the events listed.

`dffy ctl schema` prints the JSON Schema of every message.

## Security

- **Only you.** The runtime directory must be yours and closed to everyone else (mode `0700`), as for the broker ([watch.md](watch.md#dffyd)); the socket is mode `0600`. dffyd also checks each connection's user (`SO_PEERCRED` on Linux, `getpeereid` on macOS) and hangs up on another's, or on one it can't tell. Elsewhere the directory's and the socket's modes are the check.
- **What a client can do** is what you can do in the viewer: move the view, read what it shows and what's selected, add comments to its comments file, as its own rather than yours, and apply a suggestion already in a thread to the working tree, as `s` does. It can't run programs, write other files or other text, or post a review, nor have its comments posted until you make them yours.
- **Not over the network.** It's a Unix socket on the local machine.

## Performance

- Registering happens after the first frame, so it doesn't slow opening.
- Nothing runs per frame. With nobody subscribed, the viewer does nothing for the API until a request comes; with a subscriber, it compares the selection and the top row after each event and pushes once they stay put.
- Idle, nothing wakes: there are no timers and no polling.
