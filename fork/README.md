# fork/ — local fork tooling

Personal fork of [levi-qiao/herdr-agent-quota](https://github.com/levi-qiao/herdr-agent-quota),
used as the live Herdr plugin on this machine.

Everything under `fork/` is ours. Upstream never touches this directory, so it
cannot conflict during a sync. Fork-only design docs live in `fork/specs/`.

## Branches

- `main`: mirror of upstream, pinned to a release tag. Never commit here;
  `fork/sync.sh` fast-forwards it. It is **not pushed** — GitHub created
  `origin/main` from upstream's head when it made the fork, and that copy is
  irrelevant. Only the local branch matters.
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

A change to `herdr-plugin.toml` is the exception. Herdr copies the manifest into
`~/.config/herdr/plugins.json` when the plugin is linked and reads its own copy
afterwards, so a rebuild alone leaves the old pane sizes, actions and events in
place. Run `./install.sh` for those; it relinks and keeps existing preferences.

If you ever run an *older* build after a newer one, it meets a cached snapshot
containing window kinds it does not know. Loading it fails outright, which
aborts the whole refresh: no pane is updated at all, so the sidebar keeps
whatever it last showed rather than going blank. Recover by
deleting that provider's cached snapshot and refreshing — the state directory is
`~/.local/state/herdr/plugins/herdr-agent-quota/` and the file is named after the
source (`claude-statusline.json`, `codex-app-server.json`, ...):

    rm -f ~/.local/state/herdr/plugins/herdr-agent-quota/claude-statusline.json
    herdr plugin action invoke refresh --plugin herdr-agent-quota

The sidebar field preference lives beside it in `fields` and is not touched by
that, so a field you turned on stays on.
