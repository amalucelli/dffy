# Install

## Homebrew

```sh
brew install --cask amalucelli/tap/dffy
```

The cask is published with each release.

## Install script

```sh
curl -fsSL https://raw.githubusercontent.com/amalucelli/dffy/main/install.sh | sh
```

The script:

- downloads the release archive for your OS and architecture;
- checks it against the release's `checksums.txt`;
- puts `dffy` in `~/.local/bin`.

It needs `curl` or `wget`, and `sha256sum` or `shasum`. It never asks for root. `--plugin <name>` also installs a renderer plugin ([Plugins](#plugins)).

Environment variables:

| Variable | Default | Sets |
|---|---|---|
| `DFFY_VERSION` | the latest release | a release tag such as `v0.1.0` |
| `DFFY_INSTALL_DIR` | `~/.local/bin` | where the binaries go |
| `DFFY_PLUGINS` | none | plugins to install, as with `--plugin`, separated by spaces or commas |

```sh
curl -fsSL https://raw.githubusercontent.com/amalucelli/dffy/main/install.sh | DFFY_VERSION=v0.1.0 sh
```

The Linux binaries link glibc, so the script stops on musl systems such as Alpine; build from source there.

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

From a clone, `task install` runs `cargo install --path crates/dffy --root ~/.local`.

| Method | Binary lands in |
|---|---|
| `cargo install --git` | `~/.cargo/bin` |
| `task install` | `~/.local/bin` |

That directory must be on your `PATH`.

## Plugins

The experimental plugins are separate binaries: the renderers `dffy-render-markdown` and `dffy-render-mermaid`, and the review plugin `dffy-review-github`. Each dffy release ships them under the same version, outside dffy's archive and cask. Install them with one of these (drop the ones you don't want):

```sh
brew install --cask amalucelli/tap/dffy-render-markdown amalucelli/tap/dffy-render-mermaid amalucelli/tap/dffy-review-github
curl -fsSL https://raw.githubusercontent.com/amalucelli/dffy/main/install.sh | sh -s -- --plugin markdown --plugin mermaid --plugin review-github
cargo install --locked --git https://github.com/amalucelli/dffy dffy-render-markdown dffy-render-mermaid dffy-review-github
```

- The script installs dffy and the plugins side by side, all checked against the release's `checksums.txt`. `DFFY_PLUGINS=markdown,mermaid,review-github` does the same as the flags.
- dffy finds the plugins on your `PATH`.
- `dffy-review-github` also needs [`gh`](https://cli.github.com), logged in.

[plugins.md](plugins.md) covers turning them on.

## Shell completions

`dffy completions <shell>` prints a completion script for `bash`, `zsh`, `fish`, `elvish` or `powershell`.

| Shell | Command |
|---|---|
| fish | `dffy completions fish > ~/.config/fish/completions/dffy.fish` |
| zsh | `dffy completions zsh > ~/.zfunc/_dffy`, with `fpath=(~/.zfunc $fpath)` before `compinit` in `~/.zshrc` |
| bash | `dffy completions bash > ~/.local/share/bash-completion/completions/dffy` (needs the bash-completion package) |

Regenerate the script after upgrading to pick up new or changed flags.

## Man page

The man page is generated from the same definitions as `--help`. `dffy man` (hidden from `--help`) prints it in roff, and `task man` writes it to `target/dffy.1`:

```sh
task man
man target/dffy.1
```

To install it, copy it into a directory on your `MANPATH`, such as `~/.local/share/man/man1/`.

## Platforms

| OS | Architectures | File watching |
|---|---|---|
| Linux | x86_64, aarch64 | inotify |
| macOS | x86_64, aarch64 (Apple silicon) | FSEvents |

These are the release targets. Windows isn't supported. [watch.md](watch.md) covers file watching on each OS.

dffy reads repositories with its own code and doesn't need git to show diffs. `dffy doctor` checks for git, and the pager setup in [git.md](git.md) needs it.

Next: [usage.md](usage.md).
