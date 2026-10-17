# How in-place migration works — research notes and lab findings

This is what Plexy is built on: primary-source research (bootc, ostree, Universal Blue,
Vanilla OS sources) checked against real migrations in the [lab](../lab/README.md).
"Observed" means seen on a running VM; "from source" means read in code, not exercised.

## 1. bootc: installing next to a running system

`bootc install to-existing-root` must run *from the target image*:

```sh
podman run --rm --privileged --pid=host --user=root:root \
  -v /dev:/dev -v /var/lib/containers:/var/lib/containers -v /:/target \
  --security-opt label=type:unconfined_t <image> \
  bootc install to-existing-root --acknowledge-destructive --skip-fetch-check \
  --target-imgref=<ref for updates> [--target-transport=containers-storage] [--karg=...]
```

Observed behaviour:

* It **wipes `/boot`** and the ESP's `EFI/` and `loader/` directories, installs a fresh ostree
  deployment under `/ostree` on the *same* root filesystem, and installs GRUB through bootupd. It
  deletes the boot entry it booted from and creates its own ("Fedora", `\EFI\fedora\shimx64.efi`).
  → Plexy archives `/boot` and the ESP first (`/var/lib/plexy/backup`) and refuses when the ESP
  holds other loaders (Windows) unless `--allow-esp-wipe`.
* Nothing outside `/boot` is deleted. After reboot the old root is `/sysroot`.
* Kernel arguments: it carries `root=`, `rootflags=` and `rd.*` from `/proc/cmdline` and adds
  `rw` / `boot=UUID=`. Everything else (e.g. `console=ttyS0`, `nvidia-drm.modeset=1`) is lost
  unless passed as `--karg` — Plexy passes the user's arguments through.
* `/etc/fstab` of the new deployment contains **only `/boot`**, and only when the old entry was
  UUID-based (Ubuntu's `LABEL=BOOT` is dropped → Plexy adds a UUID line itself).
* The new stateroot `/var` (`/ostree/deploy/default/var`) is empty; `/etc/passwd` has only root.
  Without state migration, nobody can log in.
* Locally built images work: `--target-transport=containers-storage` with a `localhost/` image,
  and `bootc status` later reports `Booted image: containers-storage:localhost/plexy/...`.

### Data placement without copying

| old layout | what Plexy does | cost |
|---|---|---|
| `/var`, `/home` are separate btrfs subvolumes or partitions (Fedora 44 Cloud/Workstation) | fstab entries with translated targets (`/home` → `/var/home`); the old `/var` subvolume is mounted at `/var` again (ostree supports a separate `/var`) | zero |
| `/var`, `/home` are directories of `/` (Ubuntu, Ultramarine ext4) | `rename(2)` into the stateroot `var` on the same filesystem | zero |
| crossing mounts (e.g. `/root` in subvolume `root` → `/var` subvolume) | reflink copy + delete (EXDEV fallback) | ~zero on btrfs |

Every move is journaled (`/var/lib/plexy/moves.json`) together with UID/GID remaps, which is what
makes `plexy rollback` possible.

### Things that broke in the lab and why

| symptom | cause | fix |
|---|---|---|
| UEFI shell / `grub>` prompt on lab guests | overlay created with the backing *protocol* format (`file`) instead of `qcow2`; a truncated download | read `.format` from `qemu-img info --output=json`; verify checksums |
| `rename /root/.ssh -> /var/roothome/.ssh: Cross-device link` | `/var` is a different subvolume | reflink copy fallback |
| `No such file or directory` installing first-boot hooks | the data move relocated `/usr/local/bin/plexy` itself | copy from `/proc/self/exe` |
| `Text file busy` | …and on ext4 the rename made the destination *the running binary* | write temp file, `rename` over |
| toolbox build: missing `RPM-GPG-KEY-rpmfusion-…-$releasever` | repo files reference keys through dnf variables | expand `$releasever/$basearch`; carry `/etc/pki/rpm-gpg` |
| Bluefin boots to a text console after Ubuntu migration | Ubuntu has `gdm.service` *disabled* (it runs `gdm3`); replaying "disabled" units disabled Bluefin's GDM | replay disables only within a family, never for display managers / session plumbing |
| no SSH after Ubuntu migration | Debian's unit is `ssh.service`, Fedora's `sshd.service` | unit-name translation table |
| `accounts-daemon`: too many levels of symbolic links | Debian: `/var/spool/mail -> ../mail`; Fedora: `/var/mail -> spool/mail` | never move `/var` symlinks; map mail spools |
| failed `snap-*.mount` units | Ubuntu's snap units copied as "local config" | deny snap files; snaps become Flatpaks |
| `systemd-networkd-wait-online` failed | Ubuntu cloud's networkd enablement replayed onto NetworkManager | across families the target's presets decide `systemd-*` units |
| chronyd fails after rollback | migration remapped `/var/lib/chrony` to the new chrony UID | journal and invert UID/GID remaps |
| nginx routed to Homebrew from Ubuntu | binary in `nginx`, unit in `nginx-common` | dependencies that ship units make a package host-level |
| cockpit routed to the toolbox | meta-package: owns only docs/icons | classify trait-less packages by their dependencies |

### SELinux

* bootc labels the deployment with the image's policy in userspace, even from a non-SELinux host.
* Files Plexy writes are labelled by running `setfiles` *from the target image* against the
  deployment's `/etc` and the migrated `/var` (mounted at `/target`; mounting `/ostree` directly
  trips over the image's `/ostree → sysroot/ostree` symlink).
* Fedora→Fedora migrations keep the old labels on mounted subvolumes (same policy).
* Lab result: Enforcing on every migrated guest; the only AVC denials came from stock components
  (`tuned` `chcon`, GDM greeter).

### Accounts on nss-altfiles systems

System accounts live in `/usr/lib/passwd` / `/usr/lib/group`. Only human accounts go into
`/etc/passwd`. To add a user to `wheel` the group line must be copied from `/usr/lib/group` into
`/etc/group` first — otherwise the membership silently does nothing. Debian's `sudo` group maps to
`wheel`. Numeric ids of system accounts differ between distributions (and between Fedora
variants!), so ownership in migrated `/var` is remapped by account name.

## 2. Universal Blue specifics (observed)

* `brew-setup.service` unpacks Homebrew to `/home/linuxbrew` on first boot and chowns it to UID
  1000; Plexy's first-login stage then installs formulae as that user (verified: `nvim`, `rg`).
* Bluefin ships GNOME apps as Flatpaks; desktop apps from the old system are best mapped to
  Flathub (verified: VLC from RPM Fusion → `org.videolan.VLC`).
* A user's own dconf (enabled extensions, dark mode) wins over Bluefin's defaults after migration —
  the dock is absent if the user never enabled it. That is faithful, if surprising.

## 3. ABRoot (Vanilla OS 3)

From source (Vanilla-OS/ABRoot v2.6, vanilla-installer, Albius, core-image):

* Image: `ghcr.io/vanilla-os/gnome:latest` (`desktop:main` is a legacy alias).
* Volumes are found by **filesystem label**: `vos-boot` (ext4), `vos-a`/`vos-b` (btrfs, thin LVs in
  VG `vos-root`), `vos-init` (ext4 LV holding kernels + `abroot.cfg` per root), `vos-var`.
* `/etc` is an overlay: lower = image, upper = `/var/lib/abroot/etc/vos-a`; upgrades three-way
  merge the upper dir, so migrated accounts/config must be written *through* the overlay.
* Layered packages: `/etc/abroot/packages.add`, replayed through apt at every `abroot upgrade`.
* `lpkg` locks dpkg/apt on the running system — image probes must read `/var/lib/dpkg/status`.
* There is no in-place conversion upstream. ABRoot needs its own volumes, so Plexy builds them on a
  second disk or free space and leaves the old system bootable as a fallback.

## 4. Package intent

* `dnf repoquery --userinstalled` and `apt-mark showmanual` mark everything an image builder or
  installer installed as "user". Plexy combines: install-command history (dnf history, apt
  `history.log` `Commandline:`), the package manager's reason **only for packages installed after
  the OS itself** (install-time baseline: within 30 min of the oldest package), and the target
  image's own package set. Fedora Cloud: 69 "user-installed" by dnf → exactly the 3 the user
  actually installed.
