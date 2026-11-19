# Watch mode

```sh
dffy --watch
dffy --watch main        # main against the working tree, kept current
```

`--watch`, `w` in the viewer, or `watch = true` in the config makes the viewer refresh as you edit, stage and commit. The status bar shows `watch` while it's on.

Watching needs the viewer: printed output warns `--watch needs the viewer on a terminal; printing once` and prints once.

## What is watched

| Change | Effect |
|---|---|
| A file in the working tree | That file is diffed again. |
| The index, `HEAD`, a ref or `packed-refs` | The file list is re-evaluated, since what the diff compares may have moved. |
| A `.gitignore`, `.git/info/exclude` or the global excludes file (`core.excludesFile`) | The set of watched directories is re-evaluated, then the file list. |

- **Not watched:** changes in directories that git ignores and that hold no tracked files (build output, `node_modules`, `target/`) never cause a refresh. On Linux those directories aren't watched at all, so they cost no inotify watches. git's object store and logs aren't watched either.
- On Linux, an edit is on screen about 11ms after the write.
- Events are batched until 8ms pass without a new one. A long burst such as a checkout still refreshes at least every 100ms.
- A burst of more than 4,096 paths re-lists everything instead of checking each path.

### Limits

- A `.git/info` directory created after watching started isn't watched.
- Pointing `core.excludesFile` at another file needs a restart.

### Outside git

Two files or folders compared outside git (`dffy old/ new/`, see [usage.md](usage.md#comparing-files-and-folders)) are watched by the viewer itself, not through `dffyd`, with no git rules applied.

- Every directory of a compared folder is watched, or only the directory holding a compared file. Directories named `.git` aren't watched.
- A changed path is re-compared on both sides. `--exclude` patterns still leave their paths out of the list.

## dffyd

Every dffy viewer that watches the same repository shares one watcher, held by a per-user process called `dffyd` (the same binary, run as `dffy daemon run`). A repository then costs one set of [inotify watches](#linux-the-inotify-limit) however many views watch it. dffyd also serves the [control API](control.md), which every viewer registers with.

- The first viewer starts it, with nothing to set up. It exits a minute after its last viewer stops watching and exits.
- **Socket:** `$XDG_RUNTIME_DIR/dffy/broker-v<N>.sock`, or `$TMPDIR/dffy-<uid>/` when `XDG_RUNTIME_DIR` isn't set. The directory must belong to you and be closed to everyone else (mode `0700`). If it isn't, for instance because another user created `/tmp/dffy-<uid>` first, dffy doesn't use the broker: each viewer watches by itself and the status bar says why.
- **Versions:** `<N>` is the protocol version, so dffy only talks to a broker that speaks its protocol. After an upgrade, old and new brokers run side by side until the old one goes idle.
- **Fallback:** if the broker can't be started or reached, or goes away, each viewer watches by itself, with a note in the status bar. A viewer the broker can't keep up with is dropped and watches by itself too.
- Only the watcher is shared. Each viewer still reads git and computes diffs on its own, and printed output and the pager never contact the broker.

```
$ dffy daemon status
dffyd running  pid 2048555, up 12s, protocol v1
build 0.1.0+c2500b15
socket /run/user/1000/dffy/broker-v1.sock
1 root
  /home/me/src/linux  1 subscriber  6341 watches  inotify
$ dffy daemon stop
stopped dffyd (pid 2048555)
```

`dffy daemon stop` is for debugging. Viewers that were watching through it carry on by themselves, and the next watching viewer starts a new broker. Viewers registered with the control API before can't be driven until they're started again.

`dffy doctor` also reports:

- whether the broker runs;
- whether it's from the same build as the `dffy` asking. A broker outlives rebuilds and reinstalls; `dffy daemon stop` replaces an old one;
- whether any repository is being polled.

## Linux: the inotify limit

inotify needs one watch per directory, and every process you run draws from the same per-user pool, editors, language servers and file finders included. Since Linux 5.11 the default limit scales with RAM (1% of memory at about 1.3KB per watch, at least 8,192), about 130,000 on a 16GB machine. A kernel-sized tree needs about 6,400.

`dffy doctor` counts the watches the repository it runs in needs, with the same rules the watcher uses, and shows them next to the limit and the watches your processes hold:

```
✓ watch       6,345 needed here · 159 of 129,194 in use
```

It warns when they won't fit in what's free. To raise the limit:

```sh
sudo sysctl fs.inotify.max_user_watches=524288
echo fs.inotify.max_user_watches=524288 | sudo tee /etc/sysctl.d/90-inotify.conf   # keep it after a reboot
```

- **Out of watches:** dffy polls that repository once a second instead and says so. The status bar notes it, and `dffy daemon status` and `dffy doctor` list the repository as polled.
- **Event queue overflow** (a huge checkout with a stalled reader): dffy rescans the whole diff.

## macOS

On macOS dffy watches with FSEvents, with one recursive stream per repository. FSEvents has no per-user limit. The broker shares one stream between views.

Starting to watch always rescans, so if watch mode misses a change, toggling `w` off and on picks it up.
