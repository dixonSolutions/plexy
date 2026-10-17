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

declare -A VM ARGS EXPECT SNAP CMD CUSTOMIZE NOREBOOT EXPECT_OUTPUT
VM[fedora-silverblue]=fed1
ARGS[fedora-silverblue]="--target silverblue --mode image"
EXPECT[fedora-silverblue]='EXPECT_OS=Silverblue EXPECT_USERS=tester EXPECT_GROUPS=tester:wheel
EXPECT_FILES="/var/home/tester/projects/note.txt /etc/tmux.conf" EXPECT_CMDS="htop tmux nginx"
EXPECT_ACTIVE="nginx.service sshd.service" EXPECT_MOUNTS="/var /var/home" EXPECT_GREP="/etc/tmux.conf:mouse"'

VM[ultramarine-bluefin]=ultra1
ARGS[ultramarine-bluefin]="--target bluefin --mode hybrid"
EXPECT[ultramarine-bluefin]='EXPECT_WAITFILE=/var/home/alice/.local/state/plexy/firstlogin.done EXPECT_USERCMDS="alice:nvim:--version alice:rg:--version" EXPECT_SESSION=alice EXPECT_OS=Bluefin EXPECT_USERS=alice EXPECT_GROUPS=alice:wheel
EXPECT_FILES="/var/home/alice/Documents/thesis.txt /etc/dconf/db/local.d/00-plexy-test /etc/gdm/custom.conf"
EXPECT_CMDS="cockpit-bridge" EXPECT_ENABLED="cockpit.socket" EXPECT_FLATPAKS=org.gnome.Calculator
EXPECT_GREP="/etc/gdm/custom.conf:AutomaticLogin=alice"'

VM[ultramarine-bluefin-toolbox]=ultra1
ARGS[ultramarine-bluefin-toolbox]="--target bluefin --mode toolbox"
EXPECT[ultramarine-bluefin-toolbox]='EXPECT_OS=Bluefin EXPECT_USERS=alice EXPECT_GROUPS=alice:wheel
EXPECT_FILES="/var/home/alice/Documents/thesis.txt" EXPECT_ENABLED="cockpit.socket"
EXPECT_WAITFILE=/var/home/alice/.local/state/plexy/firstlogin.done
EXPECT_USERCMDS="alice:nvim:--version alice:rg:--version"'

VM[ubuntu-bluefin]=ubu1
SNAP[ubuntu-bluefin]=loaded-bluefin
ARGS[ubuntu-bluefin]="--target bluefin --mode hybrid"
EXPECT[ubuntu-bluefin]='EXPECT_WAITFILE=/var/home/tester/.local/state/plexy/firstlogin.done EXPECT_USERCMDS="tester:nvim:--version tester:rg:--version" EXPECT_SESSION=tester EXPECT_OS=Bluefin EXPECT_USERS=tester EXPECT_GROUPS=tester:wheel
EXPECT_FILES="/etc/tmux.conf /var/home/tester/Documents/notes.txt" EXPECT_CMDS="nginx" EXPECT_ACTIVE="nginx.service gdm.service sshd.service"
EXPECT_FLATPAKS=org.mozilla.firefox EXPECT_GREP="/etc/gdm/custom.conf:AutomaticLogin=tester"'

VM[ubuntu-vanilla]=ubu1
SNAP[ubuntu-vanilla]=loaded-vanilla
ARGS[ubuntu-vanilla]="--target vanilla --mode image --abroot-disk /dev/vdb"
EXPECT[ubuntu-vanilla]='EXPECT_SESSION=tester EXPECT_OS=Vanilla EXPECT_USERS=tester EXPECT_GROUPS=tester:sudo
EXPECT_FILES="/etc/tmux.conf /home/tester/Documents/notes.txt" EXPECT_CMDS="htop nginx tmux" EXPECT_ACTIVE="nginx.service"
EXPECT_GREP="/etc/gdm3/daemon.conf:AutomaticLogin=tester"'

VM[debian-bluefin]=deb1
ARGS[debian-bluefin]="--target bluefin --mode hybrid"
EXPECT[debian-bluefin]='EXPECT_STARTUSER=tester EXPECT_WAITFILE=/var/home/tester/.local/state/plexy/firstlogin.done EXPECT_USERCMDS="tester:nvim:--version tester:rg:--version" EXPECT_OS=Bluefin EXPECT_USERS=tester EXPECT_GROUPS=tester:wheel
EXPECT_FILES="/var/home/tester/projects/note.txt /etc/tmux.conf" EXPECT_CMDS="nginx htop tmux"
EXPECT_ACTIVE="nginx.service sshd.service" EXPECT_GREP="/etc/tmux.conf:mouse"'

# fedora-split: plan -> build -> apply as separate commands, with --customize.
VM[fedora-split]=fed1
CUSTOMIZE[fedora-split]='RUN echo plexy-custom > /etc/plexy-custom-marker'
CMD[fedora-split]="plexy plan --target silverblue --mode image --customize /root/extra.Containerfile --yes && plexy build && plexy apply --yes --reboot"
EXPECT[fedora-split]='EXPECT_OS=Silverblue EXPECT_USERS=tester EXPECT_GROUPS=tester:wheel
EXPECT_FILES="/var/home/tester/projects/note.txt /etc/tmux.conf /etc/plexy-custom-marker" EXPECT_CMDS="htop tmux nginx"
EXPECT_ACTIVE="nginx.service sshd.service" EXPECT_GREP="/etc/plexy-custom-marker:plexy-custom"'

# debian-dryrun: --dry-run must change nothing.
VM[debian-dryrun]=deb1
CMD[debian-dryrun]="plexy --dry-run migrate --target bluefin --mode hybrid --yes --reboot"
NOREBOOT[debian-dryrun]=1

# ultra40-refusal: the original 40 GB Ultramarine disk is too small for toolbox
# mode; plexy must refuse before building and leave the system as it was.
VM[ultra40-refusal]=ultra40
CMD[ultra40-refusal]="plexy migrate --target bluefin --mode toolbox --yes --reboot"
NOREBOOT[ultra40-refusal]=1
EXPECT_OUTPUT[ultra40-refusal]='GiB needed'

# debian-cleanup / ultramarine-export: run on the migrated guests.
VM[debian-cleanup]=deb1
VM[ultramarine-export]=ultra1

# silverblue-bluefin: run right after fedora-silverblue. Starts from an
# image-based OS and moves it to another image with `plexy switch`.
VM[silverblue-bluefin]=fed1
ARGS[silverblue-bluefin]="--target bluefin"
EXPECT[silverblue-bluefin]='EXPECT_OS=Bluefin EXPECT_USERS=tester EXPECT_GROUPS=tester:wheel
EXPECT_FILES="/var/home/tester/projects/note.txt /etc/tmux.conf" EXPECT_CMDS="htop tmux nginx"
EXPECT_ACTIVE="nginx.service sshd.service" EXPECT_GREP="/etc/tmux.conf:mouse"'

VM[fedora-rollback]=fed1
EXPECT[fedora-rollback]='EXPECT_OS="Cloud Edition" EXPECT_USERS=tester EXPECT_GROUPS=tester:wheel
EXPECT_FILES="/home/tester/projects/note.txt /etc/tmux.conf" EXPECT_CMDS="htop tmux nginx"
EXPECT_ACTIVE="nginx.service sshd.service chronyd.service"'

vm=${VM[$scenario]:?unknown scenario}
# Power the guest off when the scenario ends (pass, fail or Ctrl-C). A guest
# left running after its scenario keeps its full RAM pinned; three of them plus
# the host workload once OOM-killed the whole desktop. KEEP=1 leaves it up.
[[ ${KEEP:-} == 1 ]] || trap '"$L" stop "$vm" >/dev/null 2>&1 || "$L" kill "$vm" || true' EXIT
L_VERIFY() { "$L" scp "$vm" "$HERE/verify.sh" :/root/verify.sh
    "$L" ssh "$vm" "env ${EXPECT[$scenario]//$'\n'/ } sh /root/verify.sh" | tee "$LAB/run/$scenario.verify.log"; }

# fedora-rollback: run on an already-migrated fed1: roll back, reboot, verify the old system.
if [[ $scenario == debian-cleanup ]]; then
    "$L" list | grep -q "^$vm .*running" || { "$L" start "$vm"; "$L" wait "$vm" 600; }
    "$L" ssh "$vm" 'test -e /run/ostree-booted' || { echo "$vm is not migrated: run debian-bluefin first" >&2; exit 1; }
    "$L" ssh "$vm" 'plexy cleanup --yes' | tail -5
    "$L" ssh "$vm" 'for d in usr etc bin lib opt srv; do [ -e /sysroot/$d ] && echo "LEFT /sysroot/$d"; done; true' | tee "$LAB/run/$scenario.left.log"
    grep -q LEFT "$LAB/run/$scenario.left.log" && { echo "cleanup left old files behind" >&2; exit 1; }
    "$L" ssh "$vm" 'systemctl reboot' || true
    sleep 30
    "$L" wait "$vm" 900
    EXPECT[$scenario]=${EXPECT[debian-bluefin]}
    L_VERIFY
    exit
fi
if [[ $scenario == ultramarine-export ]]; then
    "$L" list | grep -q "^$vm .*running" || { "$L" start "$vm"; "$L" wait "$vm" 600; }
    "$L" ssh "$vm" 'test -e /run/ostree-booted' || { echo "$vm is not migrated: run ultramarine-bluefin-toolbox first" >&2; exit 1; }
    "$L" ssh "$vm" 'rm -f /var/home/alice/.local/bin/rg; su - alice -c "plexy export"' | tee "$LAB/run/$scenario.out.log"
    grep -qE '[0-9]+ apps/commands exported' "$LAB/run/$scenario.out.log" || { echo "plexy export reported nothing" >&2; exit 1; }
    EXPECT[$scenario]='EXPECT_OS=Bluefin EXPECT_USERCMDS="alice:nvim:--version alice:rg:--version"'
    L_VERIFY
    exit
fi
if [[ $scenario == silverblue-bluefin ]]; then
    "$L" list | grep -q "^$vm .*running" || { "$L" start "$vm"; "$L" wait "$vm" 600; }
    "$L" ssh "$vm" 'grep -qi silverblue /etc/os-release' ||
        { echo "$vm is not on Silverblue: run fedora-silverblue first" >&2; exit 1; }
    "$L" ssh "$vm" 'mkdir -p /var/mnt/host; mountpoint -q /var/mnt/host || mount -t 9p -o trans=virtio,version=9p2000.L hostshare /var/mnt/host
        podman image exists ghcr.io/ublue-os/bluefin:stable || podman load -q -i /var/mnt/host/bluefin.tar'
    cmd="/var/mnt/host/plexy switch ${ARGS[$scenario]:-} --yes && systemctl reboot"
    port=$(cat "$LAB/run/$vm.port")
    ssh_cmd="ssh -t -q -i $LAB/id_ed25519 -p $port -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null root@127.0.0.1"
    if [[ $record == --record ]]; then
        mkdir -p "$HERE/../demos"
        asciinema rec --overwrite -q -i 2 -c "$ssh_cmd '. /etc/os-release; echo \"# \$PRETTY_NAME -> plexy switch ${ARGS[$scenario]:-}\"; $cmd'" \
            "$HERE/../demos/$scenario.cast" || true
    else
        $ssh_cmd "$cmd" || true
    fi
    sleep 40
    "$L" wait "$vm" 900
    L_VERIFY
    "$L" screenshot "$vm" "$LAB/run/$scenario.png" >/dev/null || true
    exit
fi
if [[ $scenario == fedora-rollback ]]; then
    # The previous scenario powered the migrated guest off; boot it again.
    "$L" list | grep -q "^$vm .*running" || { "$L" start "$vm"; "$L" wait "$vm" 600; }
    "$L" ssh "$vm" 'mkdir -p /var/mnt/host; mountpoint -q /var/mnt/host || mount -t 9p -o trans=virtio,version=9p2000.L hostshare /var/mnt/host; /var/mnt/host/plexy rollback --yes --reboot' || true
    sleep 30
    "$L" wait "$vm" 600
    L_VERIFY
    exit
fi
"$L" kill "$vm" || true
"$L" revert "$vm" "${SNAP[$scenario]:-loaded}"
# qcow2 snapshots cover only the main disk; give the guest a blank second
# disk again, or LVM finds the volumes an earlier run left on it.
d2="$LAB/run/$vm.disk2.qcow2"
if [[ -f $d2 && ${KEEP_DISK2:-} != 1 ]]; then
    size=$(qemu-img info --output=json "$d2" | python3 -c 'import json,sys; print(json.load(sys.stdin)["virtual-size"])')
    qemu-img create -q -f qcow2 "$d2" "$size"
fi
"$L" start "$vm"
"$L" wait "$vm" 300
"$L" ssh "$vm" 'mkdir -p /mnt/host; mountpoint -q /mnt/host || mount -t 9p -o trans=virtio,version=9p2000.L hostshare /mnt/host 2>/dev/null
    install -m755 /mnt/host/plexy /usr/local/bin/plexy' ||
    { "$L" scp "$vm" "$LAB/share/plexy" :/usr/local/bin/plexy && "$L" ssh "$vm" 'chmod 755 /usr/local/bin/plexy'; }

if [[ -n ${CUSTOMIZE[$scenario]:-} ]]; then
    printf '%s\n' "${CUSTOMIZE[$scenario]}" | "$L" ssh "$vm" 'cat > /root/extra.Containerfile'
fi
if [[ -n ${NOREBOOT[$scenario]:-} ]]; then
    "$L" ssh "$vm" 'ls -la --time-style=+%s /boot /boot/efi 2>/dev/null | md5sum' >"$LAB/run/$scenario.boot.before"
fi
cmd=${CMD[$scenario]:-"plexy migrate ${ARGS[$scenario]:-} --yes --reboot"}
port=$(cat "$LAB/run/$vm.port")
ssh_cmd="ssh -t -q -i $LAB/id_ed25519 -p $port -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null root@127.0.0.1"
if [[ $record == --record ]]; then
    mkdir -p "$HERE/../demos"
    asciinema rec --overwrite -q -i 2 -c "$ssh_cmd '. /etc/os-release; echo \"# \$PRETTY_NAME -> plexy ${ARGS[$scenario]:-}\"; $cmd'" \
        "$HERE/../demos/$scenario.cast" || true
else
    $ssh_cmd "$cmd" || true
fi
# Scenarios that must not change the system: same OS, no /ostree, /boot as before.
if [[ -n ${NOREBOOT[$scenario]:-} ]]; then
    sleep 5
    out="$LAB/run/$scenario.check.log"
    {
        "$L" ssh "$vm" '. /etc/os-release; echo "os: $PRETTY_NAME"; test -e /ostree && echo "FAIL /ostree exists" || echo "PASS no /ostree"'
        "$L" ssh "$vm" 'ls -la --time-style=+%s /boot /boot/efi 2>/dev/null | md5sum' | cmp -s - "$LAB/run/$scenario.boot.before" &&
            echo "PASS /boot unchanged" || echo "FAIL /boot changed"
        if [[ -n ${EXPECT_OUTPUT[$scenario]:-} ]]; then
            grep -qa "${EXPECT_OUTPUT[$scenario]}" "$HERE/../demos/$scenario.cast" 2>/dev/null &&
                echo "PASS output mentions '${EXPECT_OUTPUT[$scenario]}'" || echo "FAIL output lacks '${EXPECT_OUTPUT[$scenario]}'"
        fi
    } | tee "$out"
    ! grep -q '^FAIL' "$out"
    exit
fi
# A successful migrate reboots the guest; if ssh still answers right away it failed.
sleep 15
# timeout: a half-replaced system can accept ssh and then hang the login.
if timeout 60 "$L" ssh "$vm" true 2>/dev/null &&
    ! timeout 60 "$L" ssh "$vm" 'test -e /run/ostree-booted || findmnt -n / | grep -q vos-'; then
    echo "plexy migrate did not reboot into the new system; aborting verification" >&2
    exit 1
fi

# wait for the reboot into the new system
sleep 20
"$L" wait "$vm" 900
"$L" scp "$vm" "$HERE/verify.sh" :/root/verify.sh
"$L" ssh "$vm" "env ${EXPECT[$scenario]//$'\n'/ } sh /root/verify.sh" | tee "$LAB/run/$scenario.verify.log"
"$L" screenshot "$vm" "$LAB/run/$scenario.png" >/dev/null || true
