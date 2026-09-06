#!/usr/bin/env bash
set -Eeuo pipefail

app_id=io.github.resolve_ingest.ResolveIngest
delete_data=false
if [[ $# -gt 0 ]]; then
    [[ $# -eq 1 && $1 == --delete-data ]] || {
        printf 'Usage: %s [--delete-data]\n' "$(basename -- "$0")" >&2
        exit 2
    }
    delete_data=true
fi

command -v flatpak >/dev/null 2>&1 || {
    printf 'uninstall-flatpak.sh: flatpak is unavailable in this Distrobox.\n' >&2
    exit 1
}
flatpak --version >/dev/null 2>&1 || {
    printf 'uninstall-flatpak.sh: flatpak cannot be invoked from this Distrobox.\n' >&2
    exit 1
}
if [[ -f /run/.containerenv && -x /usr/bin/distrobox-host-exec ]]; then
    distrobox-host-exec flatpak --version >/dev/null 2>&1 || {
        printf 'uninstall-flatpak.sh: the Bazzite host Flatpak command is unavailable through Distrobox.\n' >&2
        exit 1
    }
    flatpak_path=$(command -v flatpak)
    host_exec_path=$(command -v distrobox-host-exec)
    if [[ $(readlink -f "$flatpak_path") != $(readlink -f "$host_exec_path") ]]; then
        flatpak_shim_dir=$(mktemp -d)
        trap 'rm -rf "$flatpak_shim_dir"' EXIT
        printf '%s\n' '#!/bin/sh' 'exec /usr/bin/distrobox-host-exec flatpak "$@"' > "$flatpak_shim_dir/flatpak"
        chmod 755 "$flatpak_shim_dir/flatpak"
        PATH="$flatpak_shim_dir:$PATH"
        export PATH
    fi
fi

if ! flatpak info --user "$app_id" >/dev/null 2>&1; then
    printf '%s is not installed for this user.\n' "$app_id"
    exit 0
fi

args=(flatpak uninstall --user --assumeyes)
if [[ $delete_data == true ]]; then
    args+=(--delete-data)
fi
args+=("$app_id")
"${args[@]}"

if [[ $delete_data == false ]]; then
    printf 'Removed %s; application data was kept. Re-run with --delete-data to remove it.\n' "$app_id"
fi
