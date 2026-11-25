# dffy for Claude Code

A Claude Code plugin. With it, `f` (one thread) and `h` (all your open threads) in dffy hand review threads to a Claude Code session already running in the same working tree, instead of starting a new agent.

```sh
claude plugin marketplace add amalucelli/dffy
claude plugin install dffy@dffy
```

It needs a `dffy` with `dffy comments listen` on `PATH`. Without one it does nothing. [docs/agents.md](../../docs/agents.md#claude-code) says how to use it and turn it off.

| File | What it does |
|---|---|
| `monitors/monitors.json` | Runs `dffy comments listen` for the session's life. What it prints wakes the session. |
| `hooks/hooks.json` | Gives each session `hooks/session-start.json`. Reports each prompt and file edit with `dffy comments listening`, so dffy can tell sessions apart. |
| `hooks/session-start.json` | What a session is told: how to take a hand-over, and what not to do. |

For whoever changes it:

- Any exit of the monitor's command, 0 included, starts a turn in the session. So without `dffy comments listen` it sleeps, and its stderr is dropped.
- The monitor is probed by its `--help`, not by a version. A dffy without `comments` reads the words as a diff's arguments and prints that help, so the probe looks for `--key` in it.
- `$PPID` is the Claude process for the monitor and the hooks alike, and stays the same through `/clear`. It's written without braces: Claude Code substitutes `${…}` in a monitor's command itself.
- The activity hooks are `async`, print nothing and always exit 0, whichever dffy runs.
- `plugin.json` has no `version`, so each commit is one to update to. A version there would hold everyone on their copy until it's changed.

Check it with `claude plugin validate integrations/claude-code` and `claude plugin validate .` at the repository root, which has the marketplace.
