#!/usr/bin/env bash
# Runs one command against the real Hermes checkout inside a bubblewrap sandbox.
#
#   real_hermes_sandbox.sh <scratch-dir> [--shadow-install-locks] <command> [args...]
#
# The Hermes checkout, tool store and install state are mounted read-only at
# their real paths; the Hermes home is a new empty directory under <scratch-dir>;
# there is no network, no real home, no Herdr socket. Hermes' bootstrap rewrote
# the real launchers the one time it ran outside such a boundary, so their
# hashes are compared before and after and a difference fails the run.
set -euo pipefail

scratch="${1:?usage: real_hermes_sandbox.sh <scratch-dir> <command> [args...]}"
shift
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
plugin_src="$(cd "$here/../../assets/hermes/herdr-agent-quota" && pwd)"
hermes="$HOME/.hermes"
launchers=("$hermes/hermes-agent/.hermes/bin/hermes" "$hermes/hermes-agent/.hermes/bin/hermes-acp")

case "$(realpath "$scratch")" in
"$hermes/cache/scratch/"*) ;;
*)
    echo "sandbox: scratch must be an existing directory under $hermes/cache/scratch" >&2
    exit 2
    ;;
esac
# A fresh, empty Hermes home and work directory for every run.
home="$(mktemp -d "$scratch/home.XXXXXX")"
work="$(mktemp -d "$scratch/work.XXXXXX")"
echo "sandbox home: $home"
echo "sandbox work: $work"

# Hermes' dependency activation opens its install lock read-write and creates a lease file
# beside the selected environment; on the read-only mounts both fail and nothing past the
# bootstrap imports. With --shadow-install-locks those two places (and only those) are covered
# by empty scratch files, so the real ones are neither written nor visible.
shadow=()
if [[ "${1:-}" == "--shadow-install-locks" ]]; then
    shift
    shadow_dir="$(mktemp -d "$scratch/shadow.XXXXXX")"
    n=0
    for lock in "$hermes"/installs/*/.install.lock; do
        : >"$shadow_dir/$n"
        shadow+=(--bind "$shadow_dir/$n" "$lock")
        n=$((n + 1))
    done
    for leases in "$hermes"/installs/*/environments/*/.leases; do
        mkdir "$shadow_dir/$n"
        shadow+=(--bind "$shadow_dir/$n" "$leases")
        n=$((n + 1))
    done
fi

before="$(sha256sum "${launchers[@]}")"
echo "launchers before:" && echo "$before"

status=0
bwrap \
    --unshare-user --unshare-pid --unshare-net --unshare-ipc --unshare-uts \
    --die-with-parent --new-session --clearenv \
    --ro-bind /usr /usr \
    --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib64 /lib64 --symlink usr/sbin /sbin \
    --proc /proc --dev /dev --tmpfs /tmp \
    --bind "$home" "$hermes" \
    --ro-bind "$hermes/hermes-agent" "$hermes/hermes-agent" \
    --ro-bind "$hermes/tools" "$hermes/tools" \
    --ro-bind "$hermes/installs" "$hermes/installs" \
    "${shadow[@]}" \
    --ro-bind "$plugin_src" /plugin-src \
    --ro-bind "$here" /tests \
    --bind "$work" /work \
    --setenv HOME "$HOME" --setenv PATH /usr/bin:/bin \
    --setenv HERMES_DISABLE_LAZY_INSTALLS 1 --setenv PYTHONDONTWRITEBYTECODE 1 \
    --chdir /work \
    -- "$@" || status=$?

after="$(sha256sum "${launchers[@]}")"
if [[ "$before" != "$after" ]]; then
    echo "sandbox: LAUNCHER CHANGED" >&2
    echo "$after" >&2
    exit 97
fi
echo "launchers after: unchanged"
exit "$status"
