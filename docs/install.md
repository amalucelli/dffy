# Install

dffy is a single binary for Linux and macOS. Install it with Homebrew or the install script:

```sh
brew install --cask amalucelli/tap/dffy
# or
curl -fsSL https://raw.githubusercontent.com/amalucelli/dffy/main/install.sh | sh
```

## Install script

The script downloads the release archive for your OS and architecture, checks it against the release's `checksums.txt`, and puts `dffy` in `~/.local/bin`. It needs `curl` or `wget`, and `sha256sum` or `shasum`. It never asks for root.

| Option or variable | Default | Sets |
|---|---|---|
| `--plugin <name>` | none | A plugin to install next to dffy: `markdown`, `mermaid`, `review-github` or `agent`. Repeatable. See [Plugins](#plugins). |
| `DFFY_VERSION` | the latest release | A release tag such as `v0.1.0` |
| `DFFY_INSTALL_DIR` | `~/.local/bin` | Where the binaries go |
| `DFFY_PLUGINS` | none | Plugins to install, as with `--plugin`, separated by spaces or commas |

```sh
curl -fsSL https://raw.githubusercontent.com/amalucelli/dffy/main/install.sh | DFFY_VERSION=v0.1.0 sh
```

The Linux binaries link glibc, so the script stops on musl systems such as Alpine. Build from source there.

## mise

```sh
mise use -g github:amalucelli/dffy
```

mise installs from the same GitHub release archives.

## From source

dffy builds with stable Rust 1.90 or newer.

```sh
cargo install --locked --git https://github.com/amalucelli/dffy dffy
```

That puts `dffy` in `~/.cargo/bin`. From a clone, `task install` runs `cargo install --path crates/dffy --root ~/.local`, which puts it in `~/.local/bin`. Either directory must be on your `PATH`.

## Plugins

The experimental plugins are separate binaries: the renderers `dffy-render-markdown` and `dffy-render-mermaid`, the review plugin `dffy-review-github`, and the agent plugin `dffy-agent`. Each dffy release ships them at the same version, outside dffy's archive and cask. Install the ones you want:

```sh
brew install --cask amalucelli/tap/dffy-render-markdown amalucelli/tap/dffy-render-mermaid amalucelli/tap/dffy-review-github amalucelli/tap/dffy-agent
curl -fsSL https://raw.githubusercontent.com/amalucelli/dffy/main/install.sh | sh -s -- --plugin markdown --plugin mermaid --plugin review-github --plugin agent
cargo install --locked --git https://github.com/amalucelli/dffy dffy-render-markdown dffy-render-mermaid dffy-review-github dffy-agent
```

- The script installs the plugins next to dffy and checks them against the release's `checksums.txt`.
- dffy finds plugins on your `PATH`.
- `dffy-review-github` needs the GitHub CLI, [`gh`](https://cli.github.com), logged in with `gh auth login`. `dffy doctor` checks both once `review = github` is set.
- `dffy-agent` needs Claude Code (`claude`), Codex (`codex`) or Pi (`pi`), logged in.

[plugins.md](plugins.md) covers turning them on.

## Shell completions

`dffy completions <shell>` prints a completion script for `bash`, `zsh`, `fish`, `elvish` or `powershell`.

| Shell | Command |
|---|---|
| fish | `dffy completions fish > ~/.config/fish/completions/dffy.fish` |
| zsh | `dffy completions zsh > ~/.zfunc/_dffy`, with `fpath=(~/.zfunc $fpath)` before `compinit` in `~/.zshrc` |
| bash | `dffy completions bash > ~/.local/share/bash-completion/completions/dffy` (needs the bash-completion package) |

Regenerate the script after upgrading, and after setting a review plugin. The script includes the plugin's command, such as `dffy pr`, only when one is set.

## Man page

`task man` writes the man page to `target/dffy.1`. It's generated from the same definitions as `--help`, and leaves out review plugin commands.

```sh
task man
man target/dffy.1
```

To install it, copy it into a directory on your `MANPATH`, such as `~/.local/share/man/man1/`. `dffy man`, hidden from `--help`, prints the same page in roff.

## Platforms

| OS | Architectures | File watching |
|---|---|---|
| Linux | x86_64, aarch64 | inotify |
| macOS | x86_64, aarch64 (Apple silicon) | FSEvents |

Windows isn't supported. [watch.md](watch.md) covers file watching on each OS.

dffy reads repositories with its own code and doesn't need git to show diffs. Using it as git's pager needs git, and so do staging and resolving conflicts in the viewer ([git.md](git.md#when-dffy-runs-git)).

Next: [usage.md](usage.md).
