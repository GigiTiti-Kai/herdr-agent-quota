# fork/ — local fork tooling

Personal fork of [levi-qiao/herdr-agent-quota](https://github.com/levi-qiao/herdr-agent-quota),
used as the live Herdr plugin on this machine.

Everything under `fork/` is ours. Upstream never touches this directory, so it
cannot conflict during a sync. Fork-only design docs live in `fork/specs/`.

## Branches

- `main`: mirror of upstream, pinned to a release tag. Never commit here;
  `fork/sync.sh` fast-forwards it.
- `dev`: everything of ours, merged on top of `main`. Build source. Pushed to `origin`.

Unlike the herdr fork, upstream's release tags are reachable from `upstream/main`,
so a plain `git merge --ff-only <tag>` works here.

## Remotes

| remote | repository |
| --- | --- |
| `origin` | `GigiTiti-Kai/herdr-agent-quota` (our fork) |
| `upstream` | `levi-qiao/herdr-agent-quota` |

## Commands

- `fork/sync.sh [<ref>]` — fetch upstream, fast-forward `main` to the newest `v*`
  tag (or `<ref>`), merge into `dev`, push `dev`, reinstall. Resolve conflicts by
  hand if the merge stops.
- `./install.sh` — build and relink the plugin (upstream's installer).

## Upstream policy

Do **not** auto-merge upstream. Releases change defaults (v1.6.0 moved the sidebar
to Space grouping and dropped `cache`/`ttl` from the default fields), so each sync
is reviewed. Ask Claude to run `fork/sync.sh` and report what changed.

Upstream accepts contributions, but our Claude-usage-API work depends on an
undocumented endpoint; keep it fork-local unless upstream asks for it.

## After a sync

The plugin runs from `target/release/herdr-agent-quota` in this checkout, so a
rebuild is enough — Herdr does not need reinstalling. Pick the new binary up with:

    herdr plugin action invoke refresh --plugin herdr-agent-quota

Settings and cached quota survive a rebuild.
