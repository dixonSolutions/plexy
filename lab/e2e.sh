#!/usr/bin/env bash
# End-to-end migration scenarios on lab guests.
#
#   lab/e2e.sh <scenario> [--record]
#
# Each scenario reverts a prepared guest to its "loaded" snapshot (customised
# system + target image preloaded), runs `plexy migrate`, waits for the reboot
# into the new OS and runs lab/verify.sh with the scenario's expectations.
# --record captures the migration as an asciinema cast in demos/.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
LAB=${PLEXY_LAB:-$HOME/VMs/plexy-lab}
L="$HERE/lab.sh"
scenario=${1:?scenario}
record=${2:-}

declare -A VM ARGS EXPECT
VM[fedora-silverblue]=fed1
ARGS[fedora-silverblue]="--target silverblue --mode image"
EXPECT[fedora-silverblue]='EXPECT_OS=Silverblue EXPECT_USERS=tester EXPECT_GROUPS=tester:wheel
EXPECT_FILES="/var/home/tester/projects/note.txt /etc/tmux.conf" EXPECT_CMDS="htop tmux nginx"
EXPECT_ACTIVE="nginx.service sshd.service" EXPECT_MOUNTS="/var /var/home" EXPECT_GREP="/etc/tmux.conf:mouse"'

VM[ultramarine-bluefin]=ultra1
ARGS[ultramarine-bluefin]="--target bluefin --mode hybrid"
EXPECT[ultramarine-bluefin]='EXPECT_OS=Bluefin EXPECT_USERS=alice EXPECT_GROUPS=alice:wheel
EXPECT_FILES="/var/home/alice/Documents/thesis.txt /etc/dconf/db/local.d/00-plexy-test /etc/gdm/custom.conf"
EXPECT_CMDS="cockpit-bridge" EXPECT_ENABLED="cockpit.socket" EXPECT_FLATPAKS=org.gnome.Calculator
EXPECT_GREP="/etc/gdm/custom.conf:AutomaticLogin=alice"'

VM[ubuntu-bluefin]=ubu1
ARGS[ubuntu-bluefin]="--target bluefin --mode hybrid"
EXPECT[ubuntu-bluefin]='EXPECT_OS=Bluefin EXPECT_USERS=tester EXPECT_GROUPS=tester:wheel
EXPECT_FILES="/etc/tmux.conf" EXPECT_CMDS="nginx" EXPECT_ACTIVE="nginx.service"'

VM[ubuntu-vanilla]=ubu1
ARGS[ubuntu-vanilla]="--target vanilla --mode image --abroot-disk /dev/vdb"
EXPECT[ubuntu-vanilla]='EXPECT_OS=Vanilla EXPECT_USERS=tester EXPECT_GROUPS=tester:sudo
EXPECT_FILES="/etc/tmux.conf" EXPECT_CMDS="htop nginx" EXPECT_ACTIVE="nginx.service"'

vm=${VM[$scenario]:?unknown scenario}
"$L" kill "$vm" || true
"$L" revert "$vm" loaded
"$L" start "$vm"
"$L" wait "$vm" 300
"$L" ssh "$vm" 'mkdir -p /mnt/host; mountpoint -q /mnt/host || mount -t 9p -o trans=virtio,version=9p2000.L hostshare /mnt/host; install -m755 /mnt/host/plexy /usr/local/bin/plexy'

cmd="plexy migrate ${ARGS[$scenario]} --yes --reboot"
port=$(cat "$LAB/run/$vm.port")
ssh_cmd="ssh -t -q -i $LAB/id_ed25519 -p $port -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null root@127.0.0.1"
if [[ $record == --record ]]; then
    mkdir -p "$HERE/../demos"
    asciinema rec --overwrite -q -i 2 -c "$ssh_cmd '. /etc/os-release; echo \"# \$PRETTY_NAME -> plexy ${ARGS[$scenario]}\"; $cmd'" \
        "$HERE/../demos/$scenario.cast" || true
else
    $ssh_cmd "$cmd" || true
fi
# A successful migrate reboots the guest; if ssh still answers right away it failed.
sleep 15
if "$L" ssh "$vm" true 2>/dev/null && ! "$L" ssh "$vm" 'test -e /run/ostree-booted || findmnt -n / | grep -q vos-'; then
    echo "plexy migrate did not reboot into the new system; aborting verification" >&2
    exit 1
fi

# wait for the reboot into the new system
sleep 20
"$L" wait "$vm" 900
"$L" scp "$vm" "$HERE/verify.sh" :/root/verify.sh
"$L" ssh "$vm" "env ${EXPECT[$scenario]//$'\n'/ } sh /root/verify.sh" | tee "$LAB/run/$scenario.verify.log"
"$L" screenshot "$vm" "$LAB/run/$scenario.png" >/dev/null || true
