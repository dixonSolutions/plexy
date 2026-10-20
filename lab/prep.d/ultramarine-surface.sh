# linux-surface's kernel and Surface support on Ultramarine, installed the way
# their documentation does it. The guest must then be rebooted into
# kernel-surface before the snapshot is taken (see lab/README.md).
set -e
curl -fsSL https://pkg.surfacelinux.com/fedora/linux-surface.repo -o /etc/yum.repos.d/linux-surface.repo
dnf -y -q install --allowerasing kernel-surface iptsd libwacom-surface
k=$(rpm -q --qf '%{VERSION}-%{RELEASE}.%{ARCH}\n' kernel-surface-core | sort -V | tail -1)
# Ultramarine 44 ships no /etc/default/grub, so grub.cfg boots entry 0 and
# ignores the saved default; use Fedora's GRUB_DEFAULT=saved.
[ -e /etc/default/grub ] || printf 'GRUB_DEFAULT=saved\nGRUB_ENABLE_BLSCFG=true\nGRUB_TIMEOUT=1\n' > /etc/default/grub
grub2-mkconfig -o /boot/grub2/grub.cfg >/dev/null 2>&1
grubby --set-default "/boot/vmlinuz-$k"
echo "default kernel: $(grubby --default-kernel)"
