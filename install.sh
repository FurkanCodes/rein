#!/bin/sh
# Installs rein on macOS or Linux, with nothing else needed:
#
#   curl -fsSL https://github.com/FurkanCodes/rein/releases/latest/download/install.sh | sh
#
# It picks the build for this computer from a GitHub release, checks it
# against the release's checksums, and puts `rein` in ~/.local/bin. Run it
# again to update. It never asks for sudo, and it changes your shell's
# startup file only if you say yes.
#
#   REIN_VERSION=0.1.0    that version, not the latest
#   REIN_INSTALL_DIR=...  another folder

set -eu

releases=${REIN_RELEASES:-https://github.com/FurkanCodes/rein/releases}
if [ -n "${REIN_RELEASES:-}" ]; then https=""; else https="--proto =https --tlsv1.2"; fi

# Colour only on a terminal that shows it: a log stays plain.
tinted() {
    [ -t "$1" ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-dumb}" != dumb ]
}

fail() {
    if tinted 2; then
        printf '\033[1;31mrein:\033[0m %s\n' "$1" >&2
    else
        printf 'rein: %s\n' "$1" >&2
    fi
    exit 1
}

# curl's options are one word each, on purpose.
# shellcheck disable=SC2086
fetch() {
    curl $https -fsSL "$@"
}

# The same with a bar, for the big file, where someone can see it.
# shellcheck disable=SC2086
download() {
    if [ -t 2 ]; then
        curl $https -fSL --progress-bar "$@"
    else
        fetch "$@"
    fi
}

target() {
    system=$(uname -s)
    chip=$(uname -m)
    case $system in
    Darwin)
        # A shell under Rosetta says x86_64 on an Apple silicon Mac.
        if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then chip=arm64; fi
        suffix=apple-darwin
        ;;
    Linux)
        if ldd --version 2>&1 | grep -qi musl; then
            fail "there's no build for musl (Alpine); build rein from source: https://github.com/FurkanCodes/rein#install"
        fi
        suffix=unknown-linux-gnu
        ;;
    *) fail "there's no build for $system; on Windows, use WSL" ;;
    esac
    case $chip in
    arm64 | aarch64) echo "aarch64-$suffix" ;;
    x86_64 | amd64) echo "x86_64-$suffix" ;;
    *) fail "there's no build for $chip" ;;
    esac
}

latest() {
    # GitHub sends /latest on to the release's tag, or to /releases if none.
    url=$(fetch -I -o /dev/null -w '%{url_effective}' "$releases/latest") || fail "can't reach $releases"
    case $url in
    */tag/v*) echo "${url##*/tag/v}" ;;
    *) fail "there's no release yet" ;;
    esac
}

checked() {
    grep "  $archive\$" "$folder/SHA256SUMS" > "$folder/wanted" || fail "SHA256SUMS doesn't list $archive"
    if command -v sha256sum > /dev/null; then
        check="sha256sum -c"
    elif command -v shasum > /dev/null; then
        check="shasum -a 256 -c"
    else
        fail "there's nothing here to check a download with (sha256sum or shasum)"
    fi
    # Word by word, on purpose.
    # shellcheck disable=SC2086
    (cd "$folder" && $check wanted > /dev/null 2>&1) || fail "$archive doesn't match its checksum, so nothing was installed"
}

# Where your shell looks as it starts: `file`, empty where that isn't known
# for sure, and the `line` that adds `$place` to PATH there.
startup() {
    shell=${SHELL:-}
    line="export PATH=\"$place:\$PATH\""
    file=""
    case ${shell##*/} in
    zsh) file="${ZDOTDIR:-$HOME}/.zshrc" ;;
    fish)
        file="${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish"
        line="set -gx PATH \"$place\" \$PATH"
        ;;
    bash)
        file="$HOME/.bashrc"
        if [ "$(uname -s)" = Darwin ]; then
            # A new terminal there is a login shell, which reads only the
            # first of these: a new .bash_profile would hide your .profile.
            file="$HOME/.bash_profile"
            for other in "$HOME/.bash_login" "$HOME/.profile"; do
                if [ ! -e "$file" ] && [ -e "$other" ]; then file=$other; fi
            done
        fi
        ;;
    esac
    # Anything else in a path could run each time a shell starts.
    case $place in
    *[!A-Za-z0-9_./\ +-]*) file="" ;;
    esac
}

add_line() {
    mkdir -p "${file%/*}" && printf '\n# Added by the rein installer\n%s\n' "$line" >> "$file"
}

# Says how to start rein. A folder PATH doesn't have is offered to your
# shell's startup file, and only you can say yes to that.
reach() {
    case ":$PATH:" in
    *":$place:"*)
        echo "Run \`rein\` in your project's folder to start."
        return
        ;;
    esac
    startup
    if [ -n "$file" ] && [ -f "$file" ] && grep -Fqx "$line" "$file"; then
        echo "$file adds $place to your PATH already. Open a new terminal to use it."
        return
    fi
    echo "$place isn't on your PATH yet."
    # This script came down a pipe, so an answer can only come from the terminal.
    if [ -n "$file" ] && [ -t 1 ] && (: < /dev/tty) 2> /dev/null; then
        printf 'Add it in %s? [Y/n] ' "$file"
        if ! read -r answer < /dev/tty; then
            echo
            answer=n
        fi
        case $answer in
        "" | [yY] | [yY][eE][sS])
            if add_line 2> /dev/null; then
                echo "Added. Open a new terminal, then run \`rein\` in your project's folder to start."
                return
            fi
            echo "Can't write to $file."
            ;;
        esac
    fi
    echo "To use it, add this to ${file:-your shell startup file}:"
    echo "  $line"
}

main() {
    command -v curl > /dev/null || fail "installing rein needs curl"
    command -v tar > /dev/null || fail "installing rein needs tar"
    built_for=$(target)
    version=${REIN_VERSION:-$(latest)}
    version=${version#v}
    name="rein-$version-$built_for"
    archive="$name.tar.gz"
    place=${REIN_INSTALL_DIR:-$HOME/.local/bin}

    mkdir -p "$place" || fail "can't make $place"
    place=$(CDPATH='' cd -- "$place" && pwd) || fail "can't open $place"
    # Beside where it goes, so it can run from there and be moved in whole.
    folder=$(mktemp -d "$place/.rein-install.XXXXXX") || fail "can't write to $place"
    trap 'rm -rf "$folder"' EXIT
    trap 'exit 1' HUP INT TERM

    echo "Downloading rein $version for $built_for"
    download -o "$folder/$archive" "$releases/download/v$version/$archive" || fail "can't download $archive"
    fetch -o "$folder/SHA256SUMS" "$releases/download/v$version/SHA256SUMS" || fail "can't download its checksums"
    checked
    tar -xzf "$folder/$archive" -C "$folder" || fail "can't unpack $archive"

    # Tried before it replaces a rein that works.
    said=$("$folder/$name/rein" --version 2>&1) || fail "this build can't run here: $said (the Linux builds need glibc 2.34 or newer)"
    mv -f "$folder/$name/rein" "$place/rein"

    if tinted 1; then
        printf '\033[1;32mInstalled\033[0m %s in %s\n' "$said" "$place"
    else
        printf 'Installed %s in %s\n' "$said" "$place"
    fi
    reach
}

# Last, so a download cut short runs nothing.
main
