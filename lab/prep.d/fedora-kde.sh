# Fedora 44 KDE Plasma as a user has it after first-boot setup: Plasma Login
# Manager with autologin for tester, Breeze Dark, a few CLI tools.
set -e
dnf -y -q install @kde-desktop-environment
dnf -y -q install htop tmux neovim podman
systemctl set-default graphical.target
systemctl is-enabled plasmalogin.service
# plasma-setup removes its wizard autologin once setup is done.
rm -f /etc/plasmalogin.conf.d/99-plasma-setup.conf
printf '[Autologin]\nUser=tester\nSession=plasma\n' > /etc/plasmalogin.conf.d/autologin.conf
printf 'set -g mouse on\n' > /etc/tmux.conf
usermod -aG wheel tester
install -d -o tester -g tester /home/tester/.config /home/tester/Documents
printf '[General]\nColorScheme=BreezeDark\n\n[KDE]\nLookAndFeelPackage=org.kde.breezedark.desktop\n' > /home/tester/.config/kdeglobals
echo "kde notes" > /home/tester/Documents/notes.txt
chown -R tester: /home/tester/.config /home/tester/Documents
