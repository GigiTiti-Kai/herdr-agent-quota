#!/usr/bin/env bash
# Sync the fork with upstream and rebuild.
#   fork/sync.sh                 -> fast-forward main to the newest v* release tag
#   fork/sync.sh upstream/main   -> track upstream HEAD instead
# main mirrors upstream (never commit there); dev carries our changes.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

if [ -n "$(git status --porcelain)" ]; then
    echo "working tree not clean; commit first" >&2
    exit 1
fi

git fetch upstream --tags --prune
target="${1:-$(git tag --list 'v*' --sort=-v:refname | head -1)}"
echo "syncing main -> $target"

# Upstream release tags are reachable from upstream/main, so --ff-only holds.
# If that ever stops being true, this exits non-zero rather than inventing a merge.
git checkout main
git merge --ff-only "$target"
git checkout dev
# On conflict this exits non-zero: resolve, `git commit`, then run ./install.sh
# and `git push origin dev` by hand.
git merge --no-edit main
git push origin dev
"$repo/install.sh"

cat <<'EOF'

synced. pick the new binary up with:
  herdr plugin action invoke refresh --plugin herdr-agent-quota
EOF
