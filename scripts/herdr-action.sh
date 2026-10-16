# Invoke a Herdr plugin action and wait for it to finish.
#
# `herdr plugin action invoke` starts the action and returns immediately with
# `"status":"running"`. Both installer scripts need the action to have actually
# completed before they continue: uninstall.sh unlinks the plugin next, and
# unlinking while the restore action is still running can leave a statusLine
# entry pointing at a plugin that is no longer there.
#
# Sourced by install.sh and uninstall.sh; not a standalone script.

PLUGIN_ID="herdr-agent-usage"
# Space-separated earlier ids. Append here when the plugin id changes again.
PLUGIN_ID_ALIASES="herdr-agent-quota"

HERDR_ACTION_PLUGIN_ID="$PLUGIN_ID"

# Preference form of an `--agent` selection.
#
# A saved enumeration that was complete when written is later read as every
# currently supported agent, so a new provider does not turn a once-complete
# install into a partial one. Prefix an explicit subset with `only` so it
# stays a subset after that upgrade. `all` is already the complete token.
agents_pref_value() {
  case "$1" in
    ""|all|only,*) printf '%s\n' "$1" ;;
    *) printf 'only,%s\n' "$1" ;;
  esac
}

xdg_state_home() {
  printf '%s\n' "${XDG_STATE_HOME:-${HOME}/.local/state}"
}

plugin_is_listed() {
  herdr plugin list 2>/dev/null | grep -F -q -- "$1"
}

plugin_state_dirs() {
  local id="$1"
  printf '%s\n' "$(xdg_state_home)/herdr/plugins/${id}"
  printf '%s\n' "$(xdg_state_home)/herdr/plugins/state/${id}"
}

# Move names from $1 into $2 when the destination does not already have them.
# Existing files in $2 win so a current install is never overwritten by an alias.
adopt_plugin_dir() {
  local from="$1" to="$2" item base
  [[ -d "$from" ]] || return 0
  [[ "$from" == "$to" ]] && return 0
  mkdir -p "$to"
  (
    shopt -s nullglob dotglob
    for item in "$from"/*; do
      [[ -e "$item" || -L "$item" ]] || continue
      base="$(basename "$item")"
      # Cursor's hooks.json names this script by absolute path. Moving it
      # before configure rewrites that command would drop cache reports.
      case "$base" in
        *-hooks.sh) continue ;;
      esac
      if [[ ! -e "$to/$base" && ! -L "$to/$base" ]]; then
        mv "$item" "$to/$base"
      fi
    done
  )
  rmdir "$from" 2>/dev/null || true
}

# Snapshot alias config/state paths while those ids are still linked.
collect_alias_plugin_dirs() {
  ALIAS_CONFIG_DIRS=""
  ALIAS_STATE_DIRS=""
  local id dir config_home
  # Herdr can switch a linked checkout to the new manifest id before install.sh
  # runs. The old id is then absent from `plugin list`, but its directories
  # are still on disk. Always probe those paths; a listed id may also live
  # somewhere `plugin config-dir` names.
  config_home="${XDG_CONFIG_HOME:-${HOME}/.config}"
  for id in $PLUGIN_ID_ALIASES; do
    if plugin_is_listed "$id" \
      && dir="$(herdr plugin config-dir "$id" 2>/dev/null)" \
      && [[ -n "$dir" ]]; then
      ALIAS_CONFIG_DIRS="${ALIAS_CONFIG_DIRS}${dir}"$'\n'
    fi
    ALIAS_CONFIG_DIRS="${ALIAS_CONFIG_DIRS}${config_home}/herdr/plugins/config/${id}"$'\n'
    ALIAS_STATE_DIRS="${ALIAS_STATE_DIRS}$(plugin_state_dirs "$id")"$'\n'
  done
}

adopt_alias_plugin_dirs() {
  local dest_config dest_state dest_state_alt old
  dest_config="$(herdr plugin config-dir "$PLUGIN_ID")" || return 1
  dest_state="$(xdg_state_home)/herdr/plugins/${PLUGIN_ID}"
  dest_state_alt="$(xdg_state_home)/herdr/plugins/state/${PLUGIN_ID}"
  while IFS= read -r old; do
    [[ -z "$old" ]] && continue
    adopt_plugin_dir "$old" "$dest_config"
  done <<< "$ALIAS_CONFIG_DIRS"
  while IFS= read -r old; do
    [[ -z "$old" ]] && continue
    case "$old" in
      */plugins/state/*) adopt_plugin_dir "$old" "$dest_state_alt" ;;
      *) adopt_plugin_dir "$old" "$dest_state" ;;
    esac
  done <<< "$ALIAS_STATE_DIRS"
}

unlink_alias_plugins() {
  local id
  for id in $PLUGIN_ID_ALIASES; do
    plugin_is_listed "$id" || continue
    herdr plugin disable "$id" >/dev/null 2>&1 || true
    herdr plugin unlink "$id" || true
  done
}

select_action_plugin_id() {
  if plugin_is_listed "$PLUGIN_ID"; then
    HERDR_ACTION_PLUGIN_ID="$PLUGIN_ID"
    return 0
  fi
  local id
  for id in $PLUGIN_ID_ALIASES; do
    if plugin_is_listed "$id"; then
      HERDR_ACTION_PLUGIN_ID="$id"
      return 0
    fi
  done
  return 1
}

unlink_all_plugin_ids() {
  local id
  for id in $PLUGIN_ID $PLUGIN_ID_ALIASES; do
    plugin_is_listed "$id" || continue
    herdr plugin disable "$id" >/dev/null 2>&1 || true
    herdr plugin unlink "$id" || true
  done
}

# Configuration writes touch a handful of small files. A minute is far beyond
# any legitimate run and still bounds a hung action.
HERDR_ACTION_TIMEOUT_SECONDS="${HERDR_ACTION_TIMEOUT_SECONDS:-60}"

# Extract the first "<key>":"<value>" string field from a JSON blob.
herdr_action_json_field() {
  sed -n "s/.*\"$2\":\"\([^\"]*\)\".*/\1/p" <<<"$1" | head -1
}

# Status of one log entry, or empty when Herdr no longer lists it.
herdr_action_status() {
  herdr plugin log list --plugin "$HERDR_ACTION_PLUGIN_ID" --limit 50 2>/dev/null \
    | tr '{' '\n' \
    | grep -F "\"log_id\":\"$1\"" \
    | sed -n 's/.*"status":"\([a-z_]*\)".*/\1/p' \
    | head -1
}

# invoke_action_and_wait <action-id>
#
# Returns non-zero when the action reports a failure. An action whose log entry
# cannot be found is treated as finished rather than hung: older Herdr builds
# may not list it, and blocking the installer on a missing log helps nobody.
invoke_action_and_wait() {
  # `status` is a read-only special parameter in zsh, so this stays `state`
  # even though the scripts themselves run under bash.
  local action="$1" output log_id state waited=0

  output="$(herdr plugin action invoke "$HERDR_ACTION_PLUGIN_ID.$action")" || return 1
  log_id="$(herdr_action_json_field "$output" log_id)"
  if [[ -z "$log_id" ]]; then
    return 0
  fi

  while ((waited < HERDR_ACTION_TIMEOUT_SECONDS)); do
    state="$(herdr_action_status "$log_id")"
    case "$state" in
      running|"") ;;
      succeeded) return 0 ;;
      *)
        printf 'error: plugin action %s %s\n' "$action" "$state" >&2
        printf 'inspect it with: herdr plugin log list --plugin %s\n' \
          "$HERDR_ACTION_PLUGIN_ID" >&2
        return 1
        ;;
    esac
    # An entry that never appears is not worth waiting a minute for.
    if [[ -z "$state" ]] && ((waited >= 3)); then
      return 0
    fi
    sleep 1
    waited=$((waited + 1))
  done

  printf 'error: plugin action %s did not finish within %ss\n' \
    "$action" "$HERDR_ACTION_TIMEOUT_SECONDS" >&2
  return 1
}
