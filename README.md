# Plexy

**Turn the Linux you already have into an image-based ("immutable") system — in place — keeping your packages, configuration, users, services, desktop settings, Flatpaks, Homebrew and data.**

Plexy (from *flexible*: the OS architecture flexes, your system doesn't break) reads a
traditional `dnf`- or `apt`-managed installation and rebuilds it on top of an atomic image:
Bluefin, Aurora, Bazzite, Fedora Silverblue/Kinoite, any bootc image, or Vanilla OS on ABRoot
A/B partitions. Instead of "back up, reinstall, spend a weekend restoring", it is one command,
a review screen, and a reboot.

> ⚠️ **Status: experimental.** Plexy replaces your operating system. It has been exercised end
> to end in disposable VMs (see [Verified migrations](#verified-migrations)), not on thousands of
> real machines. Have a backup. Nothing here is guaranteed.

```console
# plexy migrate --target bluefin --mode hybrid
  system   : Ultramarine Linux 44 (GNOME Edition) (Fedora, Dnf5)
  desktop  : GNOME (display manager: gdm)
  packages : 1980 installed, 7 user-installed
  ...
  already in target image           3 package(s)
  layer into system image           2 package(s)
      cockpit  gnome-shell-extension-appindicator
  Homebrew                          3 package(s)
      htop  neovim  ripgrep
  toolbox container (exported)      1 package(s)
      vlc
  [a]ccept  [m]ove a package  [g]roup move  [l]ist a group  [q]uit
```

## How it works

```
 scan ──► plan ──► review ──► build ──► apply ──► reboot ──► firstboot / firstlogin
 (what     (where   (you      (derived  (install    new OS     (Flatpaks, toolbox,
  you have) it goes) decide)   image +   next to the            Homebrew, exports,
                               toolbox)  running OS, carry      dconf, CA trust)
                                         all state across)
```

1. **Scan** — inventories the running system: every package and *why* it is there
   (install-command history, package-manager reason, install-time baseline), third-party
   repositories and their keys, modified and unowned files in `/etc`, accounts and group
   memberships, enabled/disabled services, kernel arguments, mounts and btrfs subvolumes,
   the desktop environment and its system-wide extensions and dconf, Flatpak remotes/apps,
   Homebrew, snaps, Nix and distrobox containers.
2. **Plan** — inspects the target image *itself* (its packages, sessions, accounts, tooling —
   nothing about any target is hard-coded) and routes every package to a destination:

   | destination | when | how it survives updates |
   |---|---|---|
   | **base** | the target already ships it | — |
   | **image** | system-level software (services, drivers, shell extensions) or `--mode image` | layered into a derived image (`FROM <target>`), rebuilt locally |
   | **toolbox** | everything else in `--mode toolbox`; unmappable software in other modes | a distrobox/toolbox of your **old** distribution, apps and commands exported to the host |
   | **brew** | CLI tools with a Homebrew formula in `--mode brew`/`hybrid` | Homebrew in your home, config moved to `$(brew --prefix)/etc` |
   | **flatpak** | snaps (which don't run on image-based systems) | Flathub |
   | **skip** | boot chain, package managers, release metadata | provided by the target |

   Cross-family migrations (Ubuntu → Fedora-based images) map names through a curated table,
   mechanical rules (`libfoo-dev` → `foo-devel`, …) and the target's own repositories; what
   cannot be mapped runs unchanged in a toolbox of the source distribution.
3. **Review** — an interactive screen lets you move single packages or whole groups between
   destinations. The plan is plain JSON and the build contexts are plain Containerfiles in
   `/var/lib/plexy/plan/` — edit anything before applying.
4. **Build** — `podman build` of the derived system image and of the toolbox image.
5. **Apply** — installs the image next to the running system and carries all state:
   * **bootc backend**: `bootc install to-existing-root`, then into the new deployment:
     merged accounts (human users, password hashes, `sudo`↔`wheel`, groups that only exist in
     `/usr/lib/group` copied to `/etc/group` so memberships work under nss-altfiles), modified and
     local `/etc` files (minus a deny-list of boot-chain/package-manager/generated files),
     fstab entries with targets translated (`/home` → `/var/home`), unit enablement replayed
     from the old system, `/home` `/var` `/root` `/opt` `/srv` `/usr/local` either **mounted in
     place** (separate partition / btrfs subvolume — zero copy) or **renamed** into the new
     stateroot (same filesystem — zero copy), UID/GID remapping for system accounts whose
     numeric ids differ, and SELinux relabelling with the *target's* policy.
   * **ABRoot backend** (Vanilla OS): builds the A/B layout the Vanilla installer would
     (`vos-boot`, LVM `vos-root` with `init` + thin `root-a`/`root-b`, `vos-var`) on a disk or
     free space you designate, unpacks the image into root A, writes accounts and config into the
     `/etc` overlay's upper directory (so ABRoot's 3-way merge keeps them on upgrade), records
     layered packages in `/etc/abroot/packages.add`, installs GRUB + the EFI entry and copies your
     data. **Your old system is not touched** and stays bootable from the firmware menu.
6. **First boot / first login** — `plexy-firstboot.service` rebuilds dconf databases and CA
   trust and installs Flatpak replacements; the `plexy-firstlogin` user service loads the toolbox
   image, creates the container, exports its apps (menu entries) and commands (`~/.local/bin`),
   installs Homebrew formulae and notifies you.

### Desktop must match

GNOME settings, extensions and dconf mean nothing to Plasma and vice versa, so Plexy reads the
desktop from the target image's session files and **refuses** a target with a different desktop
(override: `--allow-desktop-change`). `--target auto` picks a matching image: GNOME → Bluefin /
Silverblue, KDE → Aurora / Kinoite, COSMIC → COSMIC Atomic, headless → fedora-bootc.

## Usage

```sh
sudo plexy scan -o manifest.json            # inventory only
sudo plexy targets                          # well-known targets
sudo plexy inspect ghcr.io/ublue-os/bluefin:stable
sudo plexy diff --target bluefin --mode hybrid   # what would happen, and why
sudo plexy plan --target bluefin --mode hybrid   # write /var/lib/plexy/plan/ (interactive review)
sudo plexy build                            # build derived + toolbox images
sudo plexy apply --reboot                   # the destructive part (asks you to type 'migrate')

sudo plexy migrate --target auto --mode hybrid --reboot   # all of the above

# Vanilla OS (ABRoot) needs its own volumes:
sudo plexy migrate --target vanilla --abroot-disk /dev/sdb        # dedicated disk (wiped)
sudo plexy migrate --target vanilla --abroot-disk free:/dev/sda   # unpartitioned space

# afterwards, on the new system
plexy export          # re-export toolbox apps/commands
sudo plexy cleanup    # delete the previous OS from /sysroot once you're happy
# already image-based? switch images and keep layered packages:
sudo plexy switch --target aurora
```

Modes (`--mode`): `image` (default: everything you installed is layered), `toolbox` (*all*
non-base packages — including those that came preinstalled with your old OS — go into a toolbox
of your old distro), `brew` (CLI tools to Homebrew), `hybrid` (host-level → image, CLI → brew,
GUI/rest → toolbox). `--include-preinstalled` migrates software your old OS shipped that the
target lacks in any mode.

Any image works as a target: `--target ghcr.io/you/your-image:latest`.

## Building

```sh
cargo build --release                                   # dynamic
cargo build --release --target x86_64-unknown-linux-musl  # static, runs on any distro
cargo test
```

## Test lab

`lab/lab.sh` runs disposable QEMU/KVM guests (rootless) on copy-on-write overlays of pristine
cloud/disk images, with UEFI, SSH, an interactive serial console, QMP screenshots and a 9p share
for pre-pulled images. `lab/verify.sh` checks a migrated guest. See [lab/README.md](lab/README.md).

## Verified migrations

_Filled in by the lab runs; see below._
