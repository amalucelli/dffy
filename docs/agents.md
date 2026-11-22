# Coding agents

A coding agent with a shell uses dffy through its CLI: `dffy --json` for changes, `dffy comments` for your review threads, `dffy ctl` for your open viewer. `dffy agents setup` installs a small skill that tells it how.

```sh
dffy agents setup          # install the skill for Claude Code, Codex, Pi and Amp
dffy agents status         # is it there, and current?
dffy agents show           # the guide the skill points agents to
```

Hosts with no shell, and sandboxes that block what the CLI needs, use [the MCP server](mcp.md) instead.

## What an agent does with dffy

`dffy agents show` prints the full guide. In short:

| Task | Commands |
|---|---|
| Read the changes | `dffy --json --stat` lists the changed files. `dffy --json -- <path>` gives one file's hunks, so a large change never lands in the agent's context at once ([json.md](json.md)). Revisions go before `--`, as in git. |
| Read its own turn | `--since-checkpoint` shows what changed since your latest [checkpoint](git.md#checkpoints), `--since-checkpoint=3` since checkpoint 3. |
| Read review threads | `dffy comments --open` lists open threads with their ids. `dffy comments prompt <id>` prints one thread as a prompt, `dffy comments prompt --open` every open thread of yours ([comments.md](comments.md#as-a-prompt-dffy-comments-prompt)). |
| Reply | `dffy comments apply --as <name>` writes a reply, optionally with a suggestion instead of an edit ([comments.md](comments.md#writing-dffy-comments-apply)). |
| Use your viewer | `dffy ctl selection` reads the lines you selected. `dffy ctl open` and `dffy ctl focus` show you a line or a thread. `dffy ctl apply-suggestion <id>` applies a suggestion when you ask ([control.md](control.md)). |

## What an agent may not do

The guide sets these rules. dffy enforces the ones it can.

- An agent's comments are its own. The guide always passes `--as` with the agent's name. In a review session its replies stay [private](comments.md#drafts), and `P` doesn't post them until you make them yours. Without `--as`, a comment `dffy comments apply` adds is yours, as for any script.
- It never resolves a thread. You resolve it after checking the change.
- It never posts. dffy doesn't touch the network, and the guide tells the agent not to post with another tool.
- It never makes a comment yours, and never removes one.
- It can't apply a suggestion through the viewer while the viewer is asking an agent. An agent that should only read may still reach the viewer ([MCP](mcp.md#apply_suggestion) has the same rule).

## The skill

`dffy agents setup` writes `dffy/SKILL.md` in the [Agent Skills](https://agentskills.io) format to the places agents read skills from:

| Place | Read by |
|---|---|
| `~/.claude/skills/dffy/SKILL.md` | Claude Code |
| `~/.agents/skills/dffy/SKILL.md` | Codex, Pi, Amp |

The file holds:

- a description that sends the agent to dffy instead of `git diff`, `git show` or `git log -p`, and to your review threads;
- `allowed-tools: Bash(dffy *)`;
- the stub's version, `metadata: {dffy-skill: "1"}`;
- a marker comment saying `dffy agents setup` installed it;
- one instruction: run `dffy agents show` and follow it.

The guide comes from the binary, so it always matches the installed dffy. The skill file changes only when the stub does.

`dffy agents status` reports what each place holds:

```
$ dffy agents status
current   ~/.claude/skills/dffy/SKILL.md  Claude Code
missing   ~/.agents/skills/dffy/SKILL.md  Codex, Pi, Amp
```

| Status | Meaning |
|---|---|
| `current` | What this dffy writes. |
| `outdated` | dffy's skill, but from another stub version or edited since. `dffy agents setup` rewrites it. |
| `foreign` | A `dffy/SKILL.md` that dffy didn't write. |
| `missing` | No `dffy/SKILL.md`. |

`dffy agents remove` deletes each `SKILL.md` that carries the marker. It also deletes its `dffy/` directory when nothing else is in it.

`dffy doctor` has a `skill` row for the places in your home directory, and for the repository's when it has one ([troubleshooting.md](troubleshooting.md#dffy-doctor)).

### Flags

| Flag | `setup` | `status` | `remove` | Effect |
|---|---|---|---|---|
| `--agent <agent>` | yes | yes | yes | Only the place this agent reads: `claude`, `codex`, `pi` or `amp`. Repeatable. `--agent codex` means the second place only. Without it, both. |
| `--project` | yes | yes | yes | The same two places at the repository's root (`.claude/skills/dffy/` and `.agents/skills/dffy/`), to commit with it. |
| `--force` | yes | | | Replace a `dffy/SKILL.md` without the marker. Without it, `setup` refuses and writes nothing. |
| `--dry-run` | yes | | yes | Print what it would do, and do nothing. |
| `--json` | yes | yes | yes | Print the result as one JSON object. |

A skills directory or `SKILL.md` that is a symlink, such as one into your dotfiles, is written through and stays a link. `remove` never touches a `dffy/SKILL.md` without the marker.

## Skill or MCP

Use the CLI when the agent has a shell. There's nothing to register, every command is available, and the skill keeps the agent from reaching for `git diff`. [The MCP server](mcp.md) offers the same through tools. Use it for:

- Hosts with no shell, such as Zed's and Cursor's agents with their terminal tool off.
- Sandboxes that block what the CLI needs. The comments file lives in the git directory, and the viewer is reached through a Unix socket. A sandbox that keeps `.git` read-only or blocks sockets makes `dffy comments apply` and `dffy ctl` fail. Codex's `workspace-write` mode may do this. The agent starts the MCP server as its own process, outside its sandbox.
