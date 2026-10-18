#!/usr/bin/env bash
# Prepare a lab guest for a scenario: create it from a base image, run a setup
# script inside it, pull target images there, save the "loaded" snapshot that
# e2e.sh reverts to. One guest at a time.
#
#   lab/prep.sh VM BASE SETUP [IMAGE...]
#   (SETUP is lab/prep.d/SETUP.sh, run as root inside the guest)
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
L="$HERE/lab.sh"
vm=${1:?vm} base=${2:?base} setup=${3:?setup}; shift 3
script="$HERE/prep.d/$setup.sh"
[[ -f $script ]] || { echo "no $script" >&2; exit 1; }

"$L" list | grep -q "^$vm " || DISK_SIZE=${DISK_SIZE:-40G} "$L" new "$vm" "$base"
"$L" start "$vm"
trap '"$L" stop "$vm" || "$L" kill "$vm" || true' EXIT
"$L" wait "$vm" 900
"$L" ssh "$vm" 'cloud-init status --wait >/dev/null 2>&1 || true'
"$L" ssh "$vm" 'bash -s' <"$script"
for img in "$@"; do
    "$L" ssh "$vm" "podman pull -q $img >/dev/null && echo pulled $img"
done
"$L" ssh "$vm" 'sync; df -h / | tail -1'
"$L" stop "$vm"
trap - EXIT
"$L" snapshot "$vm" loaded
echo "$vm prepared (snapshot: loaded)"
