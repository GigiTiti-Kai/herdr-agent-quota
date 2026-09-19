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

### v1.6.1 is rejected, and it cannot be skipped

2026-09-19. v1.6.1 was merged, installed, looked at, and reverted. `dev` carries
the merge and its revert; the resolved merge is kept on `fork/v1.6.1-merge`
together with `fork/specs/2026-09-19-upstream-v1.6.1-merge.md`. `main` stays at
v1.6.1.

Two of its presentation changes are unwanted here:

- A second tab of one login-scoped vendor in a Space loses cache, TTL and its own
  5h/7d/30d rows. `strip_vendor_child_extras` removes them and no setting turns
  them back on.
- A Space member's logo gains `GROUP_MEMBER_INDENT`, which puts it two columns
  right of every other row of that pane. Upstream's indent is correct for its own
  default rows, where the logo is the first row after `$quota_group`. This install
  keeps `state_text`, `terminal_title_stripped` and `$repo`/`$worktree` between
  them, so the logo is already a continuation row and the pad overshoots.

Nothing else in v1.6.1 reaches this machine: the backend work is Cursor Keychain
reading (macOS) and Grok login-scoped quota. Claude and Codex collection is
unchanged from v1.6.0.

**A later release cannot be taken without v1.6.1.** Upstream's history is linear,
so v1.6.2 and everything after it sit on top of those commits; `--ff-only` to a
newer tag brings them along. Do not try to cherry-pick around it.

Take the next release the normal way, then undo the two behaviours fork-locally.
Both are small and both are load-bearing on one function each:

- `src/herdr.rs::shares_login_quota` — return `false`. That empties
  `vendor_nesting`, so every pane stays `VendorRow::Flat`: no head/child roles, no
  `promote_shared_quota`, no `strip_vendor_child_extras`, and
  `mark_one_quota_row_per_vendor` returns early. The `$quota_share_*` rows
  `configure` writes stay empty and collapse.
- `src/herdr.rs::apply_group_and_icon` — drop the two `member` uses
  (`GROUP_MEMBER_INDENT` on the glyph, `indent_token` on a child's
  `quota_model`). Better, and upstreamable: apply the pad only when the plugin's
  first cell would land on row 0 once the empty `$quota_group` collapses — that is,
  only when no preserved user row sits between them. The plugin already reads
  Herdr's config on every publish (`configure::herdr::sidebar_width`), so the row
  order costs no extra read.

Keep the icon-colour change from v1.6.1 (`rules` on `$quota_icon` plus an
invisible working/done suffix). It is the fix for teal icons sitting one cell
right, and it is not part of either problem above.

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
