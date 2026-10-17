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
lab.sh mem                         # RSS of running guests and host headroom
```

## Disk sizes

Toolbox mode keeps the old OS (for rollback), the new deployment, root's container images, the
toolbox archive and each user's unpacked toolbox on one filesystem. `ultra1`'s 40 GB disk was
too small (plexy now refuses that up front). It was grown to 60 GB: `qemu-img convert -l
snapshot.name=loaded`, `qemu-img resize`, then guestfish `part-expand-gpt`, `part-resize`, `e2fsck-f`,
`resize2fs`, and a fresh `loaded` snapshot. Images with internal snapshots cannot be resized
in place.

## Memory budget

QEMU keeps every page of guest RAM the guest has touched, and a migration touches all of it
(image pulls, `podman build`, page cache). Treat each running guest as costing its full `MEM`
(default 6 GiB) plus ~0.5 GiB.

On 2026-10-17 three guests (one left running after its scenario had finished) plus the host's
other work (dev servers, a browser, 17 GB of RAM-backed `/tmp`) exhausted 62 GiB RAM and 8 GiB
swap. The kernel OOM-killed a guest mid-migration and several desktop apps. So:

- `lab.sh start` refuses to start a guest unless the host keeps `RESERVE_MB` (default 12 GiB)
  available afterwards, and at most `MAX_VMS` (default 2) guests run at once. `LAB_NO_MEMORY_GUARD=1`
  overrides the check; do not use it to run scenarios in parallel.
- Guests get `virtio-balloon` with free-page reporting, so memory the guest frees goes back to
  the host. qemu runs with `oom_score_adj=1000`, so under memory pressure a guest is killed first.
- `e2e.sh` powers its guest off when the scenario ends, even when it fails. `KEEP=1` leaves the guest running.
- Do not edit `lab/*.sh` while a scenario is running: bash reads a script as it executes it, so
  a running `e2e.sh` resumes at the wrong place after a long step.
- Run scenarios **one at a time**. Check `lab.sh mem` and `free -h` before you start one. On hosts where
  `/tmp` is tmpfs, never put build trees or VM images in `/tmp`.

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
| `ultramarine-bluefin-toolbox` | Ultramarine 44 GNOME, as above | `--target bluefin --mode toolbox` |
| `silverblue-bluefin` | the Silverblue result of `fedora-silverblue` (already image-based) | `plexy switch --target bluefin` |
| `debian-bluefin` | Debian 13 cloud + htop/nginx/tmux/neovim/ripgrep (prepare with `prep-debian.sh`) | `--target bluefin --mode hybrid` |
| `fedora-rollback` | the migrated `fed1` | `plexy rollback` |

Firmware note: `EDK2` in Homebrew's QEMU and GNOME Boxes both work; the guests boot via the
removable-media path on first boot and through the entry bootc/ABRoot creates afterwards.
