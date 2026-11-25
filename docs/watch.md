# Watch mode

Watch mode keeps the viewer current as you edit, stage and commit. Turn it on with `--watch`, `w` in the viewer, or `watch = true` in the config.

```sh
dffy --watch
dffy --watch main        # main against the working tree, kept current
```

The status bar shows `watch` while it's on. Watching needs the viewer. Printed output warns `--watch needs the viewer on a terminal; printing once` and prints once.

## What is watched

| Change | Effect |
|---|---|
| A file in the working tree | That file is diffed again. |
| The index, `HEAD`, a ref or `packed-refs` | The file list is rebuilt, since what the diff compares may have moved. |
| A `.gitignore`, `.git/info/exclude` or the global excludes file (`core.excludesFile`) | The set of watched directories is rebuilt, then the file list. |

- Directories git ignores that hold no tracked files (build output, `node_modules`, `target/`) never cause a refresh. On Linux they aren't watched at all, so they cost no inotify watches.
- git's object store and logs aren't watched.
- On Linux an edit is on screen about 11ms after the write. A long burst, such as a checkout, refreshes at least every 100ms.
- A `.git/info` directory created after watching started isn't watched.
- Pointing `core.excludesFile` at another file needs a restart.

If watch mode misses a change, press `w` twice. Starting to watch always rescans.

### Outside git

Files and folders compared outside git (`dffy old/ new/`, see [usage.md](usage.md#comparing-files-and-folders)) are watched by the viewer itself, not through `dffyd`, with no git rules.

- Every directory of a compared folder is watched, or only the directory holding a compared file. Directories named `.git` aren't watched.
- A changed path is compared again on both sides. `--exclude` patterns still apply.

## dffyd

Every viewer watching the same repository shares one watcher, held by a per-user process called `dffyd`. It's the same binary, run as `dffy daemon run`. A repository costs one set of [inotify watches](#linux-the-inotify-limit) however many viewers watch it. dffyd also serves the [control API](control.md), and every viewer registers with it.

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

The first viewer or `dffy comments listen` starts dffyd, with nothing to set up. It stays while a viewer watches or is registered with the control API, or a session listens there for threads. It exits a minute after the last one leaves.

It's detached from whoever starts it: it runs in a session of its own and isn't their descendant. Closing a terminal leaves it running, and so does an agent that ends by killing every process it started.

Only the watcher is shared. Each viewer reads git and computes diffs on its own, and printed output and the pager never contact the broker.

The socket is `$XDG_RUNTIME_DIR/dffy/broker-v<N>.sock`, or under `$TMPDIR/dffy-<uid>/` when `XDG_RUNTIME_DIR` isn't set. The directory must belong to you and be closed to everyone else (mode `0700`). If it isn't, for instance because another user created `/tmp/dffy-<uid>` first, each viewer watches by itself and the status bar says why.

`<N>` is the protocol version, so dffy only talks to a broker that speaks its protocol. After an upgrade, old and new brokers run side by side until the old one goes idle. If the broker can't be started or reached, or goes away, each viewer watches by itself and the status bar notes it. A viewer the broker can't keep up with is dropped and watches by itself too.

`dffy daemon stop` is for debugging, or for replacing a broker left over from an older build. Viewers watching through it carry on by themselves. While a viewer is open in a working tree or a session listens, dffyd starts again within moments, because each registers or listens again and starts the `dffy` it runs. That's the program at its own path, which an upgrade in place replaced.

So after an upgrade, stop dffyd and reopen viewers and sessions. A viewer from a dffy that doesn't register again can't be driven until it's reopened.

`dffy doctor` reports whether the broker runs, whether it's from the same build as the `dffy` asking, and whether any repository is being polled.

## Linux: the inotify limit

inotify needs one watch per directory. Every process you run draws from the same per-user pool, editors, language servers and file finders included. Since Linux 5.11 the default limit scales with RAM: 1% of memory at about 1.3KB per watch, at least 8,192. That's about 130,000 on a 16GB machine. A kernel-sized tree needs about 6,400.

`dffy doctor` counts the watches the current repository needs and shows them next to the limit and the watches your processes hold:

```
✓ watch       6,345 needed here · 159 of 129,194 in use
```

It warns when they won't fit in what's free. To raise the limit:

```sh
sudo sysctl fs.inotify.max_user_watches=524288
echo fs.inotify.max_user_watches=524288 | sudo tee /etc/sysctl.d/90-inotify.conf   # keep it after a reboot
```

When dffy runs out of watches, it polls that repository once a second instead. The status bar notes it, and `dffy daemon status` and `dffy doctor` list the repository as polled. If the kernel's event queue overflows, for example during a huge checkout, dffy rescans the whole diff.

## macOS

On macOS dffy watches with FSEvents, one recursive stream per repository, shared between viewers by the broker. FSEvents has no per-user limit.
