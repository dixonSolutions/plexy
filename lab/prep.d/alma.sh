# AlmaLinux 10 server: EPEL, a few tools, nginx, user data.
set -e
dnf -y -q install epel-release
dnf -y -q install htop tmux nginx podman vim-enhanced
systemctl enable --now nginx
printf 'set -g mouse on\nset -g history-limit 50000\n' > /etc/tmux.conf
usermod -aG wheel tester
install -d -o tester -g tester /home/tester/projects
echo "alma data survives" > /home/tester/projects/note.txt
chown tester: /home/tester/projects/note.txt
