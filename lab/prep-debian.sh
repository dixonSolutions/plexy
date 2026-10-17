#!/usr/bin/env bash
# Prepare the Debian 13 guest for the debian-bluefin scenario:
# a customised trixie system with the target image preloaded, saved as the
# "loaded" snapshot that e2e.sh reverts to. Run once; one guest at a time.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
L="$HERE/lab.sh"
VM=${VM:-deb1}

"$L" list | grep -q "^$VM " || DISK_SIZE=40G "$L" new "$VM" debian13.qcow2
"$L" start "$VM"
trap '"$L" stop "$VM" || "$L" kill "$VM" || true' EXIT
"$L" wait "$VM" 600
"$L" ssh "$VM" 'set -e
    cloud-init status --wait >/dev/null || true
    growpart /dev/vda 1 2>/dev/null || true; resize2fs "$(findmnt -no SOURCE /)" >/dev/null 2>&1 || true
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -q
    apt-get install -yq --no-install-recommends htop tmux nginx neovim ripgrep podman skopeo
    systemctl enable --now nginx
    printf "set -g mouse on\nset -g history-limit 50000\n" >/etc/tmux.conf
    usermod -aG sudo tester
    install -d -o tester -g tester /home/tester/projects
    echo "debian data survives" >/home/tester/projects/note.txt; chown tester: /home/tester/projects/note.txt
    df -h / | tail -1'
# Preload the target image. Debian's cloud kernel may lack 9p; stream it then.
"$L" ssh "$VM" 'mkdir -p /mnt/host; mount -t 9p -o trans=virtio,version=9p2000.L hostshare /mnt/host 2>/dev/null &&
    podman load -q -i /mnt/host/bluefin.tar' ||
    "$L" ssh "$VM" 'podman load -q' <"${PLEXY_LAB:-$HOME/VMs/plexy-lab}/share/bluefin.tar"
"$L" ssh "$VM" 'podman image exists ghcr.io/ublue-os/bluefin:stable && df -h / | tail -1'
"$L" stop "$VM"
trap - EXIT
"$L" snapshot "$VM" loaded
echo "$VM prepared (snapshot: loaded)"
