# Coding agents

A coding agent with a shell works with dffy through its CLI: it reads what changed with `dffy --json`, reads and answers your review threads with `dffy comments`, and drives the viewer you have open with `dffy ctl`. A small skill, which `dffy agents setup` installs, tells agents to do so. Hosts with no shell, and sandboxes that block what the CLI needs, use [the MCP server](mcp.md) instead.

```sh
dffy agents setup          # install the skill for Claude Code, Codex, Pi and Amp
dffy agents status         # is it there, and current?
dffy agents show           # the guide the skill points agents to
```

## The skill

`dffy agents setup` writes `dffy/SKILL.md`, in the [Agent Skills](https://agentskills.io) format, in the two places the agents dffy knows read skills from:

| Place | Read by |
|---|---|
| `~/.claude/skills/dffy/SKILL.md` | Claude Code |
| `~/.agents/skills/dffy/SKILL.md` | Codex, Pi, Amp |

- **What's in it.** A description that sends the agent to dffy instead of `git diff`, `git show` or `git log -p`, and to your review threads; `allowed-tools: Bash(dffy *)`; the number of the stub (`metadata: {dffy-skill: "1"}`); a marker comment saying `dffy agents setup` installed it; and one instruction: run `dffy agents show` and follow it. The guide comes from the binary, so it always matches the dffy installed, and the skill changes only when the stub does.
- **`--agent <agent>`**, repeatable, names the agents to set up (`claude`, `codex`, `pi`, `amp`), and so the places: `--agent codex` writes the second only. Without it, both.
- **`--project`** writes the same two at the root of the repository dffy runs in (`.claude/skills/dffy/` and `.agents/skills/dffy/`), to commit with it, instead of in your home directory.
- **Linked directories** stay linked: a skills directory that's a link into your dotfiles is written through, and so is a `SKILL.md` that is one.
- **Someone else's.** A `dffy/SKILL.md` without the marker isn't dffy's: `setup` refuses it and writes nowhere, until `--force` replaces it. `remove` leaves it.
- **`--dry-run`** says what `setup` or `remove` would do, and does nothing. **`--json`** prints the result as one object.

`dffy agents status` says what each place holds:

| Status | Meaning |
|---|---|
| `current` | What this dffy writes. |
| `outdated` | dffy's skill, but another stub or edited since: `dffy agents setup` writes it again. |
| `foreign` | A `dffy/SKILL.md` that dffy didn't write. |
| `missing` | No `dffy/SKILL.md`. |

```
$ dffy agents status
current   ~/.claude/skills/dffy/SKILL.md  Claude Code
missing   ~/.agents/skills/dffy/SKILL.md  Codex, Pi, Amp
```

`dffy agents remove` deletes the `SKILL.md` that carry the marker, and their `dffy/` directories unless other files are in them. `dffy doctor` has a `skill` row for the places in your home directory, and for the repository's when it has one ([troubleshooting.md](troubleshooting.md#dffy-doctor)).

## What an agent does with dffy

`dffy agents show` prints the guide. In short:

- **Read the changes:** `dffy --json --stat` lists the changed files, then `dffy --json -- <path>` gives one file's hunks, so a large change never lands in the agent's context at once ([json.md](json.md)). Revisions go as in git. `--since-checkpoint` shows only what changed since your latest checkpoint, `--since-checkpoint=3` since checkpoint 3 ([git.md](git.md#checkpoints)): the agent's own turn after you handed it your review.
- **Review threads:** `dffy comments --open` lists the open threads with their ids, `dffy comments prompt <id>` gives one as a prompt and `dffy comments prompt --open` every open thread of yours in one ([comments.md](comments.md#as-a-prompt-dffy-comments-prompt)), and `dffy comments apply --as <name>` writes the agent's reply, with a suggestion when it proposes a change rather than making it ([comments.md](comments.md#writing-dffy-comments-apply)).
- **Your viewer:** `dffy ctl selection` reads the lines you selected, `dffy ctl open` and `dffy ctl focus` show you a line or a thread, and `dffy ctl apply-suggestion <id>` applies a thread's suggestion when you ask it to ([control.md](control.md)).

## The rules

The guide tells the agent, and dffy holds it to what it can:

- **Its comments are its own.** The guide always passes `--as` with the agent's name, so a reply is never yours: in a review session it stays [private](comments.md#drafts), and `P` doesn't post it until you make it yours. Without `--as`, a comment `dffy comments apply` adds is yours, as for any script.
- **It never resolves a thread.** You resolve it, once you've checked the change.
- **It never posts.** dffy doesn't touch the network, and the agent isn't to post review comments for you with another tool.
- **It never makes a comment yours or removes one.**
- **Applying a suggestion through the viewer** is refused while the viewer asks an agent, as for [MCP](mcp.md#apply_suggestion), since an agent asked to only read may reach the viewer.

## Skill or MCP

The CLI is the way for an agent with a shell: nothing to register, every command there for it, and the skill keeps it from reaching for `git diff`. [The MCP server](mcp.md) offers the same through tools, and is the way for:

- **Hosts with no shell,** such as Zed's and Cursor's agents when their terminal tool is off.
- **Sandboxes that block what the CLI needs.** The comments file is in the git directory and the viewer is reached through a Unix socket, so a sandbox that keeps `.git` read-only or blocks sockets, as Codex's `workspace-write` mode may, makes `dffy comments apply` and `dffy ctl` fail there. The MCP server runs outside the agent's sandbox, as the agent's own process starts it.
