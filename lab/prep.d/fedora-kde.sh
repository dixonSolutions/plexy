# Fedora 44 with KDE Plasma, SDDM autologin for tester, dark theme, CLI tools.
set -e
dnf -y -q install @kde-desktop-environment
dnf -y -q install htop tmux neovim podman sddm
test -e /usr/lib/systemd/system/sddm.service
systemctl enable sddm
systemctl set-default graphical.target
mkdir -p /etc/sddm.conf.d
printf '[Autologin]\nUser=tester\nSession=plasma\n' > /etc/sddm.conf.d/autologin.conf
printf 'set -g mouse on\n' > /etc/tmux.conf
usermod -aG wheel tester
install -d -o tester -g tester /home/tester/.config /home/tester/Documents
printf '[General]\nColorScheme=BreezeDark\n\n[KDE]\nLookAndFeelPackage=org.kde.breezedark.desktop\n' > /home/tester/.config/kdeglobals
echo "kde notes" > /home/tester/Documents/notes.txt
chown -R tester: /home/tester/.config /home/tester/Documents
