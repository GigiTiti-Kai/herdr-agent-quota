#!/bin/sh
# Every Herdr hook and action runs the plugin through this launcher.
# bin/ holds the copy that install.sh and Herdr's build step make
# (scripts/install-binary.sh); `cargo clean` or a cleanup of target/ leaves it
# alone. target/release is the fallback for a checkout built before bin/
# existed. Only shell built-ins: Herdr may run this without a usable PATH.
root="${0%/scripts/*}"
bin="$root/bin/herdr-agent-usage"
[ -x "$bin" ] || bin="$root/target/release/herdr-agent-usage"
exec "$bin" "$@"
