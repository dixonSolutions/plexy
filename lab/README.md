# Plexy test lab

Disposable QEMU/KVM guests for end-to-end migration tests. Rootless: needs `/dev/kvm`,
`qemu-system-x86_64` (e.g. `brew install qemu`), `podman`, `python3`.

```
~/VMs/plexy-lab/
  base/    pristine images (never written)
  run/     per-guest overlays, UEFI vars, serial logs, QMP/serial sockets
  share/   exported to guests over 9p (mount -t 9p hostshare /mnt/host):
           the static plexy binary and pre-pulled target images (podman save)
```

## Base images used

| file | source |
|---|---|
| `fedora44.qcow2` | Fedora-Cloud-Base-Generic-44-1.7 (btrfs, subvolumes root/home/var/boot) |
| `ubuntu2604.img` | Ubuntu 26.04 server cloud image (ext4) + `ubuntu-desktop-minimal` installed in the guest |
| `ultramarine44-prep.qcow2` | Ultramarine 44 GNOME disk image (ext4), customised with `virt-customize` (user `alice`, sshd) |

Verify downloads against the publishers' checksums — a truncated compressed qcow2 boots to a
bare `grub>` prompt with I/O errors, which is how this lab learned to check.

## Commands

```sh
lab.sh new NAME BASE [--no-seed]   # overlay + cloud-init seed (user tester/tester, root/root, lab ssh key)
EXTRA_DISK=60G lab.sh new ...      # with an empty second disk (ABRoot volumes)
lab.sh start NAME                  # UEFI, virtio, ssh on 127.0.0.1:<port>, VNC+QMP sockets
FIRMWARE=bios lab.sh start NAME    # legacy BIOS
lab.sh wait NAME [secs]            # until ssh answers
lab.sh ssh NAME [cmd]              # as root
lab.sh console NAME 'cmd'          # log in on the serial console (when ssh is not available)
lab.sh screenshot NAME out.png     # QMP screendump
lab.sh stop|kill NAME
lab.sh snapshot|revert NAME TAG    # qcow2 internal snapshots (guest stopped)
lab.sh rm NAME
```

## Scenarios

`e2e.sh <scenario> [--record]` reverts the guest to its `loaded` snapshot, runs
`plexy migrate`, waits for the reboot and runs `verify.sh` with the scenario's expectations.
`--record` writes an asciinema cast to `demos/`.

| scenario | guest | command |
|---|---|---|
| `fedora-silverblue` | Fedora 44 Cloud + htop/nginx/tmux, sshd tweak, data | `--target silverblue --mode image` |
| `ultramarine-bluefin` | Ultramarine 44 GNOME + CLI tools, VLC (RPM Fusion), Cockpit, GNOME extension, dconf, Flatpak | `--target bluefin --mode hybrid` |
| `ubuntu-bluefin` | Ubuntu 26.04 GNOME + nginx/tmux/neovim, Firefox snap | `--target bluefin --mode hybrid` |
| `ubuntu-vanilla` | same, second disk | `--target vanilla --abroot-disk /dev/vdb` |

Firmware note: `EDK2` in Homebrew's QEMU and GNOME Boxes both work; the guests boot via the
removable-media path on first boot and through the entry bootc/ABRoot creates afterwards.
