#!/bin/sh
# Install a checksum-verified Linux release without changing shell configuration.
set -eu

main() (
    set -eu
    if [ "$#" -gt 0 ]; then
        case "$1" in
            -h|--help)
                printf '%s\n' 'Install gitt for Linux x86-64 or ARM64.' \
                    'Usage: sh install.sh' \
                    'GITT_INSTALL_DIR sets the destination (default: $HOME/.local/bin).' \
                    'GITT_VERSION selects a release tag (default: latest).'
                exit 0 ;;
            *) printf 'gitt: unknown argument: %s\n' "$1" >&2; exit 1 ;;
        esac
    fi

    fail() { printf 'gitt: %s\n' "$*" >&2; exit 1; }
    [ "$(uname -s)" = Linux ] || fail 'Linux is required.'
    case "$(uname -m)" in
        x86_64|amd64) arch=x86_64 ;;
        aarch64|arm64) arch=aarch64 ;;
        *) fail 'Supported architectures are x86-64 and ARM64.' ;;
    esac
    for command in tar sha256sum mktemp awk; do
        command -v "$command" >/dev/null 2>&1 || fail "Required command not found: $command"
    done
    if command -v curl >/dev/null 2>&1; then
        download() { curl -fLsS --retry 3 --connect-timeout 15 --max-time 300 "$1" -o "$2"; }
    elif command -v wget >/dev/null 2>&1; then
        download() { wget -q -T 30 -O "$2" "$1"; }
    else
        fail 'Install curl or wget first.'
    fi

    version=${GITT_VERSION:-latest}
    case "$version" in
        latest) release=https://github.com/TerryTsai/gitt/releases/latest/download ;;
        *)
            case "$version" in
                *[!A-Za-z0-9._-]*) fail 'GITT_VERSION must be a release tag, such as v0.2.0.' ;;
            esac
            case "$version" in
                v[0-9]*) ;;
                [0-9]*) version=v$version ;;
                *) fail 'GITT_VERSION must be a release tag, such as v0.2.0.' ;;
            esac
            release=https://github.com/TerryTsai/gitt/releases/download/$version ;;
    esac
    destination=${GITT_INSTALL_DIR:-${HOME:?Set HOME or GITT_INSTALL_DIR}/.local/bin}
    archive=gitt-linux-$arch.tar.gz
    temporary=$(mktemp -d "${TMPDIR:-/tmp}/gitt-install.XXXXXXXX")
    pending=
    trap 'rm -rf "$temporary"; if [ -n "$pending" ]; then rm -f "$pending"; fi' 0
    trap 'exit 1' HUP INT TERM

    printf 'Downloading gitt for Linux %s…\n' "$arch"
    download "$release/SHA256SUMS" "$temporary/SHA256SUMS" || fail 'Could not download release checksums.'
    download "$release/$archive" "$temporary/$archive" || fail 'Could not download the release archive.'
    awk -v archive="$archive" '$2 == archive && length($1) == 64 { print; found++ } END { if (found != 1) exit 1 }' \
        "$temporary/SHA256SUMS" > "$temporary/checksum" || fail 'No unique checksum found for this archive.'
    (cd "$temporary" && sha256sum -c checksum >/dev/null) || fail 'Checksum verification failed; nothing was installed.'
    tar -xzf "$temporary/$archive" -C "$temporary" gitt
    [ -f "$temporary/gitt" ] && [ ! -L "$temporary/gitt" ] || fail 'The archive does not contain a regular gitt executable.'

    mkdir -p "$destination"
    [ ! -d "$destination/gitt" ] || fail 'The destination gitt path is a directory.'
    pending=$(mktemp "$destination/.gitt.XXXXXXXX")
    cp "$temporary/gitt" "$pending"
    chmod 755 "$pending"
    mv -f "$pending" "$destination/gitt"
    pending=
    printf 'Installed %s\n' "$destination/gitt"
    case ":${PATH:-}:" in
        *:"$destination":*) printf 'Run: gitt /path/to/repositories\n' ;;
        *) printf 'Run: "%s/gitt" /path/to/repositories\n' "$destination" ;;
    esac
)

main "$@"
