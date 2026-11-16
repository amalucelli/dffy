#!/bin/sh
# Installs the dffy binary, and optionally renderer plugins, from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/amalucelli/dffy/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/amalucelli/dffy/main/install.sh | sh -s -- --plugin markdown
#
# Options:
#   --plugin NAME     also install the renderer plugin dffy-render-NAME next to dffy;
#                     repeatable. Plugins: markdown, mermaid
#
# Environment:
#   DFFY_PLUGINS      plugins to install, separated by spaces or commas; adds to --plugin
#   DFFY_VERSION      release tag to install, e.g. v0.1.0 (default: the latest release)
#   DFFY_INSTALL_DIR  where to put the binaries (default: $HOME/.local/bin)
#   DFFY_BASE_URL     directory URL holding the archives and checksums.txt, overriding
#                     the GitHub release; for testing against a local build
#
# Archive names must match the archives section of .goreleaser.yaml.

set -eu

repo="amalucelli/dffy"

say() {
	printf 'dffy-install: %s\n' "$*" >&2
}

die() {
	say "error: $*"
	exit 1
}

has() {
	command -v "$1" >/dev/null 2>&1
}

detect_os() {
	case "$(uname -s)" in
	Linux)
		# The Linux builds link glibc.
		if has ldd && ldd --version 2>&1 | grep -qi musl; then
			die "no build for musl libc; build from source with: cargo install --git https://github.com/$repo dffy"
		fi
		echo linux
		;;
	Darwin) echo darwin ;;
	*) die "unsupported OS: $(uname -s); releases cover Linux and macOS" ;;
	esac
}

detect_arch() {
	case "$(uname -m)" in
	x86_64 | amd64)
		# A shell under Rosetta reports x86_64 on Apple silicon; prefer the native build.
		if [ "$1" = darwin ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
			echo arm64
		else
			echo amd64
		fi
		;;
	aarch64 | arm64) echo arm64 ;;
	*) die "unsupported architecture: $(uname -m); releases cover x86_64 and aarch64" ;;
	esac
}

# download URL DEST
download() {
	if has curl; then
		curl -fsSL --retry 3 -o "$2" "$1" || die "download failed: $1"
	elif has wget; then
		wget -q -O "$2" "$1" || die "download failed: $1"
	else
		die "curl or wget is required"
	fi
}

sha256() {
	if has sha256sum; then
		sha256sum "$1" | cut -d ' ' -f 1
	elif has shasum; then
		shasum -a 256 "$1" | cut -d ' ' -f 1
	else
		die "sha256sum or shasum is required to verify the download"
	fi
}

# add_plugin NAME
add_plugin() {
	case "$1" in
	markdown | mermaid) ;;
	*) die "unknown plugin '$1'; available: markdown, mermaid" ;;
	esac
	case " $plugins " in
	*" $1 "*) ;;
	*) plugins="${plugins:+$plugins }$1" ;;
	esac
}

parse_args() {
	plugins=""
	while [ $# -gt 0 ]; do
		case "$1" in
		--plugin)
			[ $# -ge 2 ] || die "--plugin needs a name, e.g. --plugin markdown"
			add_plugin "$2"
			shift 2
			;;
		--plugin=*)
			add_plugin "${1#--plugin=}"
			shift
			;;
		*) die "unknown option '$1'; usage: sh -s -- [--plugin markdown]" ;;
		esac
	done

	old_ifs=$IFS
	IFS=', '
	set -f
	for name in ${DFFY_PLUGINS:-}; do
		if [ -n "$name" ]; then
			add_plugin "$name"
		fi
	done
	set +f
	IFS=$old_ifs
}

# fetch ARCHIVE BINARY: download ARCHIVE into $tmp, verify it against checksums.txt
# and extract BINARY from it.
fetch() {
	download "$base/$1" "$tmp/$1"
	expected=$(awk -v name="$1" '$2 == name { print $1 }' "$tmp/checksums.txt")
	[ -n "$expected" ] || die "$1 is not listed in checksums.txt"
	actual=$(sha256 "$tmp/$1")
	[ "$actual" = "$expected" ] || die "checksum mismatch for $1: expected $expected, got $actual"
	tar -xzf "$tmp/$1" -C "$tmp" "$2" || die "cannot extract $2 from $1"
}

# place BINARY: move BINARY from $tmp into $install_dir.
place() {
	# Copy next to the target and rename, so a running binary is never overwritten in place.
	cp "$tmp/$1" "$install_dir/.$1.tmp" || die "cannot write to $install_dir"
	chmod 755 "$install_dir/.$1.tmp"
	mv -f "$install_dir/.$1.tmp" "$install_dir/$1"

	if [ "$os" = darwin ] && xattr -p com.apple.quarantine "$install_dir/$1" >/dev/null 2>&1; then
		xattr -d com.apple.quarantine "$install_dir/$1"
	fi

	# stdin is closed so a binary that reads it can't eat the rest of a piped script.
	say "installed $("$install_dir/$1" --version </dev/null 2>/dev/null || echo "$1") to $install_dir/$1"
}

main() {
	parse_args "$@"

	os=$(detect_os)
	arch=$(detect_arch "$os")

	version=${DFFY_VERSION:-}
	case "$version" in
	"" | v*) ;;
	*) version="v$version" ;;
	esac
	if [ -n "${DFFY_BASE_URL:-}" ]; then
		base=${DFFY_BASE_URL%/}
	elif [ -n "$version" ]; then
		base="https://github.com/$repo/releases/download/$version"
	else
		base="https://github.com/$repo/releases/latest/download"
	fi

	install_dir=${DFFY_INSTALL_DIR:-${HOME:?HOME is not set; set DFFY_INSTALL_DIR}/.local/bin}

	tmp=$(mktemp -d) || die "cannot create a temporary directory"
	trap 'rm -rf "$tmp"' EXIT
	trap 'exit 130' INT
	trap 'exit 143' TERM

	binaries=dffy
	for name in $plugins; do
		binaries="$binaries dffy-render-$name"
	done

	say "downloading $binaries${version:+ ($version)}"
	download "$base/checksums.txt" "$tmp/checksums.txt"
	# Everything is verified before anything is installed, so a bad plugin archive
	# leaves no half-finished install behind.
	for bin in $binaries; do
		fetch "${bin}_${os}_${arch}.tar.gz" "$bin"
	done

	mkdir -p "$install_dir" || die "cannot create $install_dir"
	for bin in $binaries; do
		place "$bin"
	done

	case ":${PATH:-}:" in
	*":$install_dir:"*) ;;
	*) say "$install_dir is not on your PATH; add it in your shell profile, e.g. export PATH=\"$install_dir:\$PATH\"" ;;
	esac
}

# Called last, so a download cut short by the pipe runs nothing.
main "$@"
