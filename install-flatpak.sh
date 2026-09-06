#!/usr/bin/env bash
set -Eeuo pipefail

repo_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
app_id=io.github.resolve_ingest.ResolveIngest
manifest="$repo_dir/packaging/flatpak/$app_id.yml"
cargo_sources="$repo_dir/packaging/flatpak/cargo-sources.json"
build_dir="${XDG_CACHE_HOME:-${HOME:-/tmp}/.cache}/resolve-ingest-flatpak"

die() {
    printf 'install-flatpak.sh: %s\n' "$*" >&2
    exit 1
}

command -v flatpak >/dev/null 2>&1 || die "flatpak is unavailable. Run this from the Fedora Distrobox with host Flatpak integration."
flatpak --version >/dev/null 2>&1 || die "flatpak cannot be invoked from this Distrobox. Install/enable Flatpak on the Bazzite host and re-enter the Distrobox."
if [[ -f /run/.containerenv && -x /usr/bin/distrobox-host-exec ]]; then
    distrobox-host-exec flatpak --version >/dev/null 2>&1 || die "the Bazzite host Flatpak command is unavailable through Distrobox."
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
command -v flatpak-builder >/dev/null 2>&1 || die "flatpak-builder is missing. Install it inside the Fedora Distrobox with: sudo dnf install flatpak-builder"
flatpak-builder --version >/dev/null 2>&1 || die "flatpak-builder is present but cannot run in this Distrobox."

[[ -f "$manifest" ]] || die "manifest not found: $manifest"
[[ -f "$cargo_sources" ]] || die "Cargo source metadata not found: $cargo_sources"

has_flathub=false
while IFS= read -r remote; do
    if [[ $remote == flathub ]]; then
        has_flathub=true
        break
    fi
done < <(flatpak remotes --user --columns=name 2>/dev/null)
if [[ $has_flathub == false ]]; then
    die "the user Flathub remote is missing. Add it with: distrobox-host-exec flatpak remote-add --if-not-exists --user flathub https://dl.flathub.org/repo/flathub.flatpakrepo"
fi

printf 'Building and installing %s for the current user...\n' "$app_id"
flatpak-builder \
    --user \
    --install \
    --force-clean \
    --delete-build-dirs \
    --disable-rofiles-fuse \
    --install-deps-from=flathub \
    "$build_dir" \
    "$manifest"

printf '\nInstalled %s. Launch it from KDE or with: distrobox-host-exec flatpak run %s\n' "$app_id" "$app_id"
