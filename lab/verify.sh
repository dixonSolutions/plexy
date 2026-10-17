#!/bin/sh
# Post-migration checks, run *inside* a migrated guest as root.
# Expectations come from environment variables (space separated lists):
#   EXPECT_OS        substring of PRETTY_NAME (e.g. "Silverblue", "Bluefin")
#   EXPECT_USERS     user names that must exist (and be able to log in)
#   EXPECT_GROUPS    user:group pairs that must hold
#   EXPECT_FILES     files that must exist with content (data, config)
#   EXPECT_CMDS      commands that must resolve (layered or exported)
#   EXPECT_ACTIVE    systemd units that must be active
#   EXPECT_ENABLED   systemd units that must be enabled
#   EXPECT_FLATPAKS  flatpak app ids that must be installed
#   EXPECT_MOUNTS    mount points that must be mounted
#   EXPECT_GREP      "file:pattern" pairs (config content survived)
#   EXPECT_WAITFILE  wait (up to 15 min) for this file first (e.g. first-login done)
#   EXPECT_USERCMDS  user:command:args run as that user in a login shell
pass=0 fail=0
ok() { pass=$((pass + 1)); printf 'PASS  %s\n' "$*"; }
ko() { fail=$((fail + 1)); printf 'FAIL  %s\n' "$*"; }
chk() { desc=$1; shift; if "$@" >/dev/null 2>&1; then ok "$desc"; else ko "$desc"; fi; }

if [ -n "$EXPECT_WAITFILE" ]; then
  i=0; while [ ! -e "$EXPECT_WAITFILE" ] && [ $i -lt 180 ]; do sleep 5; i=$((i + 1)); done
fi
. /etc/os-release
echo "== $PRETTY_NAME (kernel $(uname -r))"
[ -n "$EXPECT_OS" ] && case "$PRETTY_NAME $NAME $VARIANT" in *"$EXPECT_OS"*) ok "os is $EXPECT_OS" ;; *) ko "os is $EXPECT_OS (got $PRETTY_NAME)" ;; esac
if [ -e /run/ostree-booted ]; then ok "booted from an ostree/bootc deployment"; fi
if command -v abroot >/dev/null 2>&1 && findmnt -n / | grep -q vos-; then ok "booted from an ABRoot root"; fi
command -v bootc >/dev/null 2>&1 && bootc status --format=humanreadable 2>/dev/null | sed -n '1,6p'

for u in $EXPECT_USERS; do
  chk "user $u exists" getent passwd "$u"
  h=$(getent passwd "$u" | cut -d: -f6)
  chk "home of $u present ($h)" test -d "$h"
  chk "password hash of $u carried" sh -c "getent shadow $u | cut -d: -f2 | grep -q '^\\\$'"
done
for p in $EXPECT_GROUPS; do
  u=${p%%:*} g=${p#*:}
  chk "$u in group $g" sh -c "id -nG $u | tr ' ' '\n' | grep -qx $g"
done
for f in $EXPECT_FILES; do chk "file $f" test -s "$f"; done
for c in $EXPECT_CMDS; do
  chk "command $c" sh -c "command -v $c || ls /var/home/*/.local/bin/$c /home/*/.local/bin/$c"
done
for uc in $EXPECT_USERCMDS; do
  u=${uc%%:*} rest=${uc#*:} c=${rest%%:*} a=${rest#*:}
  chk "as $u: $c $a" su - "$u" -c "export XDG_RUNTIME_DIR=/run/user/\$(id -u) PATH=\$HOME/.local/bin:\$PATH; $c $a"
done
for u in $EXPECT_ACTIVE; do chk "unit $u active" systemctl is-active --quiet "$u"; done
for u in $EXPECT_ENABLED; do chk "unit $u enabled" systemctl is-enabled --quiet "$u"; done
for a in $EXPECT_FLATPAKS; do chk "flatpak $a" sh -c "flatpak list --app --columns=application | grep -qx $a"; done
for m in $EXPECT_MOUNTS; do chk "mount $m" findmnt -n "$m"; done
for fp in $EXPECT_GREP; do
  f=${fp%%:*} p=${fp#*:}
  chk "$f contains '$p'" grep -q -- "$p" "$f"
done
failed=$(systemctl --failed --no-legend --plain | awk '{print $1}' | grep -v '^session-.*\.scope$' | tr '\n' ' ')
[ -z "$failed" ] && ok "no failed units" || ko "failed units: $failed"
if [ -e /sys/fs/selinux/enforce ]; then
  echo "   SELinux: $(getenforce), AVC denials this boot: $(journalctl -b -q --no-pager 2>/dev/null | grep -c 'avc:  denied')"
fi
[ -f /var/lib/plexy/report.txt ] && sed 's/^/   firstboot: /' /var/lib/plexy/report.txt
echo "== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
