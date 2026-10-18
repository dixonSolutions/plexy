# Debian 13 with the Cinnamon desktop (no image-based Cinnamon target exists).
set -e
export DEBIAN_FRONTEND=noninteractive
apt-get update -q
apt-get install -yq task-cinnamon-desktop podman >/dev/null
systemctl set-default graphical.target
