# openSUSE Tumbleweed: zypper, not a supported source family.
set -e
zypper -n -q install htop tmux || true
