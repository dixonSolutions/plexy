#!/usr/bin/env bash
# plexy VM lab: disposable QEMU/KVM guests for end-to-end migration tests.
#
# Every guest runs on a copy-on-write overlay of a pristine base image, so a
# failed migration costs nothing: `lab.sh rm NAME && lab.sh new NAME BASE`.
# Runs rootless — needs /dev/kvm access, qemu-system-x86_64, and podman (for
# building cloud-init seed ISOs and libguestfs customisation).
set -euo pipefail

LAB=${PLEXY_LAB:-$HOME/VMs/plexy-lab}
BASE=$LAB/base
RUN=$LAB/run
KEY=$LAB/id_ed25519
GUESTFS_IMG=localhost/plexy-guestfs
QEMU=${QEMU:-qemu-system-x86_64}
MEM=${MEM:-6144}
CPUS=${CPUS:-4}
DISK_SIZE=${DISK_SIZE:-40G}

mkdir -p "$RUN" "$LAB/share"
[[ -f $KEY ]] || ssh-keygen -q -t ed25519 -N '' -f "$KEY" -C plexy-lab

firmware() {
    [[ -n ${OVMF_CODE:-} ]] && { echo "$OVMF_CODE"; return; }
    local share
    share=$(dirname "$(readlink -f "$(command -v "$QEMU")")")/../share/qemu
    for f in "$share/edk2-x86_64-code.fd" /usr/share/edk2/ovmf/OVMF_CODE.fd /usr/share/OVMF/OVMF_CODE_4M.fd; do
        [[ -f $f ]] && { echo "$f"; return; }
    done
    echo "no OVMF firmware found" >&2; return 1
}

vars_template() {
    local share
    share=$(dirname "$(readlink -f "$(command -v "$QEMU")")")/../share/qemu
    for f in "$share/edk2-i386-vars.fd" /usr/share/edk2/ovmf/OVMF_VARS.fd /usr/share/OVMF/OVMF_VARS_4M.fd; do
        [[ -f $f ]] && { echo "$f"; return; }
    done
    echo "no OVMF vars template found" >&2; return 1
}

port_of() { cat "$RUN/$1.port"; }

guestfs() {
    podman run --rm -i --device /dev/kvm -v "$LAB:$LAB:z" -w "$LAB" "$GUESTFS_IMG" "$@"
}

make_seed() { # NAME
    local name=$1 dir="$RUN/$1.seed"
    mkdir -p "$dir"
    cat >"$dir/meta-data" <<EOF
instance-id: $name
local-hostname: $name
EOF
    cat >"$dir/user-data" <<EOF
#cloud-config
users:
  - name: tester
    uid: 1000
    groups: [wheel, sudo, adm]
    shell: /bin/bash
    sudo: "ALL=(ALL) NOPASSWD:ALL"
    lock_passwd: false
    ssh_authorized_keys: ["$(cat "$KEY.pub")"]
disable_root: false
chpasswd:
  expire: false
  users:
    - {name: tester, password: tester, type: text}
    - {name: root, password: root, type: text}
ssh_pwauth: true
runcmd:
  - mkdir -p /root/.ssh && cp /home/tester/.ssh/authorized_keys /root/.ssh/ && chmod 600 /root/.ssh/authorized_keys
EOF
    guestfs genisoimage -quiet -output "$dir/seed.iso" -volid cidata -joliet -rock "$dir/user-data" "$dir/meta-data"
}

cmd_new() { # NAME BASEFILE [--no-seed]
    local name=$1 base=$2 fmt
    [[ -e $RUN/$name.qcow2 ]] && { echo "$name exists" >&2; return 1; }
    fmt=$(qemu-img info --output=json "$BASE/$base" | python3 -c 'import json,sys; print(json.load(sys.stdin)["format"])')
    qemu-img create -q -f qcow2 -b "$BASE/$base" -F "$fmt" "$RUN/$name.qcow2" "$DISK_SIZE"
    # Growing the disk leaves the backup GPT header mid-disk, which EDK2 rejects.
    guestfs guestfish -a "$RUN/$name.qcow2" run : part-expand-gpt /dev/sda
    cp "$(vars_template)" "$RUN/$name.vars.fd"
    local port=$((2200 + $(ls "$RUN"/*.port 2>/dev/null | wc -l) + 1))
    while grep -qx "$port" "$RUN"/*.port 2>/dev/null; do port=$((port + 1)); done
    echo "$port" >"$RUN/$name.port"
    [[ ${3:-} == --no-seed ]] || make_seed "$name"
    # EXTRA_DISK=60G gives the guest an empty second disk (e.g. for ABRoot volumes).
    [[ -n ${EXTRA_DISK:-} ]] && qemu-img create -q -f qcow2 "$RUN/$name.disk2.qcow2" "$EXTRA_DISK"
    echo "created $name (ssh port $port)"
}

cmd_start() { # NAME [extra qemu args...]
    local name=$1; shift
    local seed=() fw=() port
    port=$(port_of "$name")
    if [[ ${FIRMWARE:-uefi} == uefi ]]; then
        fw=(-drive "if=pflash,format=raw,readonly=on,file=$(firmware)" -drive "if=pflash,format=raw,file=$RUN/$name.vars.fd")
    fi
    [[ -f $RUN/$name.seed/seed.iso ]] && seed=(-drive "file=$RUN/$name.seed/seed.iso,media=cdrom,readonly=on")
    [[ -f $RUN/$name.disk2.qcow2 ]] && seed+=(-drive "file=$RUN/$name.disk2.qcow2,if=none,id=d1,cache=unsafe,discard=unmap" -device virtio-blk-pci,drive=d1)
    "$QEMU" -name "$name" -enable-kvm -machine q35 -cpu host -m "$MEM" -smp "$CPUS" \
        "${fw[@]}" \
        -drive "file=$RUN/$name.qcow2,if=none,id=d0,cache=unsafe,discard=unmap" -device virtio-blk-pci,drive=d0,bootindex=1 \
        "${seed[@]}" \
        -netdev "user,id=n0,hostfwd=tcp:127.0.0.1:$port-:22" -device virtio-net-pci,netdev=n0 \
        -device virtio-rng-pci -device virtio-vga \
        -virtfs "local,path=$LAB/share,mount_tag=hostshare,security_model=none,readonly=on,id=fs0" \
        -display none -vnc "unix:$RUN/$name.vnc" \
        -qmp "unix:$RUN/$name.qmp,server,nowait" \
        -chardev "socket,id=ser0,path=$RUN/$name.serial.sock,server=on,wait=off,logfile=$RUN/$name.serial.log,logappend=on" \
        -serial chardev:ser0 \
        -pidfile "$RUN/$name.pid" -daemonize "$@"
    echo "started $name"
}

cmd_ssh() { # NAME [cmd...]  (user root)
    local name=$1; shift
    ssh -q -i "$KEY" -p "$(port_of "$name")" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o ConnectTimeout=5 -o ServerAliveInterval=15 -o LogLevel=ERROR "${SSH_USER:-root}@127.0.0.1" "$@"
}

cmd_scp() { # NAME SRC DEST   (prefix remote paths with ':')
    local name=$1 src=$2 dst=$3
    src=${src/#:/root@127.0.0.1:}; dst=${dst/#:/root@127.0.0.1:}
    scp -q -i "$KEY" -P "$(port_of "$name")" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR "$src" "$dst"
}

cmd_wait() { # NAME [timeout-seconds]
    local name=$1 t=${2:-600} i=0
    until cmd_ssh "$name" true 2>/dev/null; do
        sleep 5; i=$((i + 5))
        ((i < t)) || { echo "timeout waiting for $name" >&2; tail -20 "$RUN/$name.serial.log" >&2; return 1; }
    done
    echo "$name reachable after ${i}s"
}

qmp() { # NAME JSON-COMMAND
    printf '{"execute":"qmp_capabilities"}\n%s\n' "$2" | timeout 10 socat - "UNIX-CONNECT:$RUN/$1.qmp" >/dev/null 2>&1 ||
        python3 - "$RUN/$1.qmp" "$2" <<'EOF'
import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.recv(65536)
s.sendall(b'{"execute":"qmp_capabilities"}\n'); time.sleep(0.2); s.recv(65536)
s.sendall(sys.argv[2].encode() + b"\n"); time.sleep(1.5); print(s.recv(65536).decode())
EOF
}

cmd_screenshot() { # NAME OUT.png
    qmp "$1" "{\"execute\":\"screendump\",\"arguments\":{\"filename\":\"$2\",\"format\":\"png\"}}"
    echo "$2"
}

cmd_type() { # NAME TEXT   (types TEXT followed by Enter via QMP key events)
    python3 - "$RUN/$1.qmp" "$2" <<'PYEOF'
import json, socket, sys, time
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.recv(65536)
s.sendall(b'{"execute":"qmp_capabilities"}\n'); time.sleep(0.2); s.recv(65536)
names = {" ": "spc", "-": "minus", ".": "dot", "/": "slash", "\n": "ret"}
def key(*k):
    keys = [{"type": "qcode", "data": x} for x in k]
    s.sendall(json.dumps({"execute": "send-key", "arguments": {"keys": keys}}).encode() + b"\n")
    time.sleep(0.08); s.recv(65536)
for ch in sys.argv[2] + "\n":
    if ch.isupper(): key("shift", ch.lower())
    elif ch in names: key(names[ch])
    else: key(ch)
PYEOF
}

cmd_stop() { # NAME
    local name=$1 i=0
    [[ -f $RUN/$name.pid ]] || return 0
    cmd_ssh "$name" 'systemctl poweroff' 2>/dev/null || qmp "$name" '{"execute":"system_powerdown"}'
    while kill -0 "$(cat "$RUN/$name.pid")" 2>/dev/null; do
        sleep 2; i=$((i + 2))
        ((i < 120)) || { kill "$(cat "$RUN/$name.pid")"; break; }
    done
    rm -f "$RUN/$name.pid"
}

cmd_console() { # NAME 'shell command'   (logs in on ttyS0 as root/root, prints output)
    python3 - "$RUN/$1.serial.sock" "$2" <<'PYEOF'
import socket, sys, time, re
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.settimeout(1)
def read(t):
    buf, end = b"", time.time() + t
    while time.time() < end:
        try:
            d = s.recv(65536)
            if d: buf += d
        except socket.timeout:
            pass
    return buf.decode(errors="replace")
def send(x): s.sendall(x.encode() + b"\r")
send(""); out = read(2)
if "login:" in out:
    send("root"); read(2); send("root"); read(4)
marker = "PLEXY%d" % int(time.time())
send("stty -echo; " + sys.argv[2] + "; printf '%s-%s\\n' " + marker + " END")
out, end = "", time.time() + 600
while marker + "-END" not in out and time.time() < end:
    out += read(2)
out = re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", out)
body = out.split(marker + "-END")[0]
print("\n".join(l for l in body.splitlines() if "stty -echo" not in l).strip())
PYEOF
}

cmd_kill() { [[ -f $RUN/$1.pid ]] && kill "$(cat "$RUN/$1.pid")" 2>/dev/null; rm -f "$RUN/$1.pid"; }

cmd_snapshot() { qemu-img snapshot -c "$2" "$RUN/$1.qcow2"; }   # NAME TAG  (VM stopped)
cmd_revert() { qemu-img snapshot -a "$2" "$RUN/$1.qcow2"; }     # NAME TAG  (VM stopped)

cmd_rm() { cmd_kill "$1" || true; rm -rf "$RUN/$1".*; echo "removed $1"; }

cmd_list() {
    for p in "$RUN"/*.port; do
        [[ -e $p ]] || continue
        local n; n=$(basename "$p" .port)
        local st=stopped
        [[ -f $RUN/$n.pid ]] && kill -0 "$(cat "$RUN/$n.pid")" 2>/dev/null && st=running
        printf '%-24s port=%s %s\n' "$n" "$(cat "$p")" "$st"
    done
}

cmd=${1:-}; shift || true
case $cmd in
new | start | ssh | scp | wait | screenshot | console | type | stop | kill | snapshot | revert | rm | list) "cmd_$cmd" "$@" ;;
guestfs) guestfs "$@" ;;
*) sed -n '2,8p' "$0"; echo "commands: new start ssh scp wait screenshot stop kill snapshot revert rm list guestfs"; exit 1 ;;
esac
