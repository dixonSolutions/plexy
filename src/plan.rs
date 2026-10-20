//! The migration plan: where every package, config file and piece of state
//! ends up on the new system. A plan is a JSON document the user can review
//! and edit before anything touches the disk.

use crate::desktop::{self, Compat, Desktop};
use crate::manifest::Manifest;
use crate::mapping::{self, is_infrastructure};
use crate::os::Family;
use crate::pkg::ConfigKind;
use crate::target::{Backend, ImageFacts};
use crate::util::{self, info, warn};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Write};

/// Where a package goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dest {
    /// Already part of the target image: nothing to do.
    Base,
    /// Layered into a derived bootc image built from the target.
    Image,
    /// Installed into a distrobox/toolbox of the *source* distribution and
    /// exported (apps + commands) to the host.
    Toolbox,
    /// Installed with Homebrew (CLI tools).
    Brew,
    /// Replaced by a Flatpak from Flathub.
    Flatpak,
    /// Not migrated (boot chain, package managers, unavailable, or by choice).
    Skip,
}

impl Dest {
    pub fn label(self) -> &'static str {
        match self {
            Dest::Base => "already in target image",
            Dest::Image => "layer into system image",
            Dest::Toolbox => "toolbox container (exported)",
            Dest::Brew => "Homebrew",
            Dest::Flatpak => "Flatpak",
            Dest::Skip => "skip",
        }
    }
}

/// Default routing policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Everything the user installed is layered into the system image.
    Image,
    /// Every non-base package (user-installed AND preinstalled) goes into a
    /// toolbox of the source distro; only host-level software is layered.
    Toolbox,
    /// CLI tools go to Homebrew where a formula exists; the rest is layered.
    Brew,
    /// Host-level → image, CLI → Homebrew, GUI → Flatpak/toolbox, rest → toolbox.
    Hybrid,
}

/// What kind of software a package is, from the files it ships.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PkgTraits {
    /// Ships systemd system units, udev rules, kernel modules, PAM/polkit or
    /// D-Bus system services: must live on the host.
    pub host_level: bool,
    /// Ships .desktop launchers.
    pub gui: bool,
    /// Ships executables in a bin directory.
    pub cli: bool,
    pub binaries: Vec<String>,
    pub desktop_files: Vec<String>,
    pub units: Vec<String>,
    /// GNOME Shell extension (needs to be on the host to be loaded by the shell).
    pub shell_extension: bool,
}

pub fn classify_files(files: &str) -> PkgTraits {
    let mut t = PkgTraits::default();
    for f in files.lines() {
        let f = f.trim();
        if f.starts_with("/usr/lib/systemd/system/") || f.starts_with("/lib/systemd/system/") {
            if f.ends_with(".service")
                || f.ends_with(".socket")
                || f.ends_with(".timer")
                || f.ends_with(".mount")
            {
                t.units.push(f.rsplit('/').next().unwrap().to_string());
                t.host_level = true;
            }
        } else if f.starts_with("/usr/lib/udev/rules.d/")
            || f.starts_with("/lib/udev/rules.d/")
            || f.starts_with("/usr/lib/modules/")
            || f.starts_with("/lib/modules/")
            || f.starts_with("/usr/lib64/security/")
            || f.starts_with("/usr/lib/x86_64-linux-gnu/security/")
            || f.starts_with("/usr/share/polkit-1/rules.d/")
            || f.starts_with("/usr/share/dbus-1/system-services/")
            || f.starts_with("/usr/lib/NetworkManager/")
            || f.starts_with("/usr/lib/x86_64-linux-gnu/NetworkManager/")
            || f.starts_with("/usr/share/wayland-sessions/")
            || f.starts_with("/usr/share/xsessions/")
            || f.starts_with("/usr/lib/firmware/")
            || f.starts_with("/lib/firmware/")
            || f.starts_with("/usr/lib/sysusers.d/")
        {
            t.host_level = true;
        } else if f.starts_with("/usr/share/gnome-shell/extensions/") {
            t.shell_extension = true;
            t.host_level = true;
        } else if f.starts_with("/usr/share/applications/") && f.ends_with(".desktop") {
            // Launchers for terminal programs (htop, nvim) or hidden helpers do
            // not make a package a desktop app.
            let entry = std::fs::read_to_string(f).unwrap_or_default();
            let hidden = entry
                .lines()
                .any(|l| matches!(l.trim(), "Terminal=true" | "NoDisplay=true"));
            if !hidden {
                t.gui = true;
            }
            t.desktop_files
                .push(f.rsplit('/').next().unwrap().to_string());
        } else if ["/usr/bin/", "/usr/sbin/", "/bin/", "/sbin/", "/usr/games/"]
            .iter()
            .any(|d| f.starts_with(d))
            && f.len() > 9
        {
            t.cli = true;
            t.binaries.push(f.to_string());
        }
    }
    t.binaries.sort();
    t.binaries.dedup();
    t
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedPkg {
    pub name: String,
    pub dest: Dest,
    /// Name(s) in the destination ecosystem (rpm/deb names, formula, flatpak id).
    pub target_names: Vec<String>,
    pub why: String,
    /// "user" when the user installed it, "preinstalled" when it came with the OS.
    pub origin: String,
    #[serde(default)]
    pub traits: PkgTraits,
    /// Packages it replaces (unversioned Conflicts/Obsoletes, e.g.
    /// libwacom-surface → libwacom): removed from the target's base when
    /// it ships them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaces: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigAction {
    /// Copy into the new system's /etc.
    Etc,
    /// Bake into the toolbox image (config for containerised software).
    Toolbox,
    /// Copy into Homebrew's etc prefix (and /etc).
    Brew,
    Skip,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedConfig {
    pub path: String,
    pub kind: ConfigKind,
    pub package: Option<String>,
    pub action: ConfigAction,
    pub why: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolboxPlan {
    /// "distrobox" or "toolbox"
    pub engine: String,
    pub name: String,
    /// Container base: the source distribution's toolbox image.
    pub base_image: String,
    pub image_tag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub plexy_version: String,
    pub created: u64,
    pub mode: Mode,
    pub source: String,
    pub source_family: Family,
    pub source_desktop: Option<Desktop>,
    pub target_image: String,
    pub target_name: String,
    pub target_family: Family,
    pub target_desktop: Option<Desktop>,
    pub backend: Backend,
    /// Tag of the derived image Plexy builds (bootc backend).
    pub derived_image: String,
    pub packages: Vec<PlannedPkg>,
    pub configs: Vec<PlannedConfig>,
    /// Repository definition files copied into derived images (same family only).
    pub repo_files: Vec<String>,
    /// Repository files copied into the toolbox image (source distro).
    #[serde(default)]
    pub toolbox_repo_files: Vec<String>,
    pub toolbox: Option<ToolboxPlan>,
    pub flatpaks_to_install: Vec<String>,
    pub units_to_enable: Vec<String>,
    #[serde(default)]
    pub units_to_disable: Vec<String>,
    pub kargs: Vec<String>,
    pub warnings: Vec<String>,
    /// Image reference the migrated system follows for updates (the
    /// registry's `update.remote`; plans from older versions: empty).
    #[serde(default)]
    pub update_remote: String,
    /// Switch to the signature-verified reference (ostree-image-signed).
    #[serde(default)]
    pub update_signed: bool,
    /// How layered packages survive updates. Older plans: derived image.
    #[serde(default)]
    pub layering: crate::registry::Layering,
    /// Registry status of this conversion (verified / untested / ...).
    #[serde(default)]
    pub conversion_status: Option<String>,
    /// The target's /usr/local is inside the read-only image: plexy itself
    /// is baked into the derived image so its first-boot units can run.
    #[serde(default)]
    pub plexy_in_image: bool,
    /// The source's own kernel, when the target ships a different one.
    #[serde(default)]
    pub kernel: Option<crate::kernel::KernelPlan>,
}

impl Plan {
    /// Image reference for updates, falling back to the target image.
    pub fn remote(&self) -> &str {
        if self.update_remote.is_empty() {
            &self.target_image
        } else {
            &self.update_remote
        }
    }

    pub fn by_dest(&self, d: Dest) -> Vec<&PlannedPkg> {
        self.packages.iter().filter(|p| p.dest == d).collect()
    }
    pub fn names_for(&self, d: Dest) -> Vec<String> {
        let mut v: Vec<String> = self
            .by_dest(d)
            .iter()
            .flat_map(|p| p.target_names.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }
}

/// Paths under /etc that are never copied: they are owned by the boot chain,
/// the package manager, the account merge, or regenerated on the new system.
pub fn etc_denied(path: &str) -> bool {
    const EXACT: &[&str] = &[
        "/etc/fstab",
        "/etc/crypttab",
        "/etc/mtab",
        "/etc/os-release",
        "/etc/resolv.conf",
        "/etc/ld.so.cache",
        "/etc/passwd",
        "/etc/passwd-",
        "/etc/group",
        "/etc/group-",
        "/etc/shadow",
        "/etc/shadow-",
        "/etc/gshadow",
        "/etc/gshadow-",
        "/etc/subuid",
        "/etc/subuid-",
        "/etc/subgid",
        "/etc/subgid-",
        "/etc/.pwd.lock",
        "/etc/default/grub",
        "/etc/kernel/cmdline",
        "/etc/machine-info",
        "/etc/adjtime",
        "/etc/aliases.db",
        "/etc/udev/hwdb.bin",
        "/etc/selinux/config",
        "/etc/sysconfig/kernel",
        "/etc/nsswitch.conf",
        "/etc/sudoers",
        "/etc/login.defs",
        "/etc/environment",
        "/etc/profile",
        "/etc/bashrc",
        "/etc/bash.bashrc",
        "/etc/issue",
        "/etc/issue.net",
        "/etc/motd",
        "/etc/lsb-release",
        "/etc/debian_version",
        "/etc/redhat-release",
        "/etc/system-release",
        "/etc/system-release-cpe",
        "/etc/fedora-release",
        "/etc/hostname.bak",
        "/etc/.updated",
        "/etc/ostree-mkinitcpio",
        "/etc/locale.gen",
        "/etc/timezone",
    ];
    const PREFIX: &[&str] = &[
        "/etc/grub",
        "/etc/kernel/",
        "/etc/dracut",
        "/etc/initramfs-tools",
        "/etc/yum.repos.d/",
        "/etc/dnf/",
        "/etc/yum/",
        "/etc/apt/",
        "/etc/dpkg/",
        "/etc/pki/rpm-gpg/",
        "/etc/rpm/",
        "/etc/selinux/",
        "/etc/alternatives/",
        "/etc/ssl/certs/",
        "/etc/pki/ca-trust/extracted/",
        "/etc/pki/tls/certs/",
        "/etc/ld.so.conf",
        "/etc/cloud/",
        "/etc/netplan/",
        "/etc/sysconfig/network-scripts/",
        "/etc/systemd/system/getty.target.wants/",
        "/etc/systemd/system/cloud-",
        "/etc/ostree/",
        "/etc/containers/storage.conf",
        // The target's signature policy decides how its own updates are
        // verified; the old system's must never replace it.
        "/etc/containers/policy.json",
        "/etc/containers/registries.d/",
        "/etc/pki/containers/",
        "/etc/X11/xorg.conf.d/00-keyboard.conf",
        "/etc/apparmor",
        "/etc/update-motd.d/",
        "/etc/cron.daily/apt",
        "/etc/logrotate.d/apt",
        "/etc/kernel-img.conf",
        "/etc/default/apport",
        "/etc/snap/",
        "/etc/profile.d/apps-bin-path.sh",
        "/etc/rc",
        "/etc/init.d/",
        "/etc/udev/rules.d/70-persistent",
        "/etc/lvm/archive/",
        "/etc/lvm/backup/",
        "/etc/lvm/cache/",
        "/etc/abroot",
        "/etc/sudoers.d/90-cloud-init",
        "/etc/ssh/sshd_config.d/50-cloud-init",
        "/etc/ssh/sshd_config.d/60-cloudimg",
        "/etc/authselect/",
        "/etc/pam.d/switchable-auth",
        "/etc/pam.d/system-auth",
        "/etc/pam.d/password-auth",
        "/etc/pam.d/fingerprint-auth",
        "/etc/pam.d/smartcard-auth",
        "/etc/pam.d/postlogin",
        "/etc/dconf/db/distro.d/",
        "/etc/issue.d/",
        "/etc/sysconfig/bootloader",
        "/etc/shells",
        "/etc/systemd/system/dbus",
        "/etc/systemd/system/display-manager.service",
        "/etc/systemd/user/dbus",
        "/etc/vconsole.conf.bak",
        "/etc/plymouth/",
        "/etc/crypto-policies/back-ends/",
    ];
    // Snaps do not exist on the targets: their mount units, udev rules and
    // services would only fail.
    if path.contains("snapd")
        || path.starts_with("/etc/systemd/system/snap")
        || path.contains("/snap.")
        || path.starts_with("/etc/udev/rules.d/70-snap")
    {
        return true;
    }
    // Enablement symlinks are recreated by replaying unit state on the target;
    // copying them would resurrect units that do not exist there.
    if [".wants/", ".requires/", ".upholds/"]
        .iter()
        .any(|w| path.contains(w))
    {
        return true;
    }
    // default.target & co: the target's default (graphical vs multi-user) wins.
    if path.starts_with("/etc/systemd/system/")
        && path.ends_with(".target")
        && !path[20..].contains('/')
    {
        return true;
    }
    // Compiled dconf databases are rebuilt by `dconf update` on first boot.
    if let Some(rest) = path.strip_prefix("/etc/dconf/db/")
        && !rest.contains('/')
    {
        return true;
    }
    EXACT.contains(&path)
        || PREFIX.iter().any(|p| path.starts_with(p))
        || path.ends_with(".rpmnew")
        || path.ends_with(".rpmsave")
        || path.ends_with(".dpkg-old")
        || path.ends_with(".dpkg-dist")
        || path.ends_with('~')
        || path.ends_with(".bak")
        || path.ends_with(".swp")
}

/// Config that is meaningful on any distribution (copied even across families).
pub fn etc_portable(path: &str) -> bool {
    const EXACT: &[&str] = &[
        "/etc/hostname",
        "/etc/machine-id",
        "/etc/localtime",
        "/etc/hosts",
        "/etc/vconsole.conf",
        "/etc/locale.conf",
        "/etc/tmux.conf",
        "/etc/vimrc",
        "/etc/inputrc",
        "/etc/gdm/custom.conf",
        "/etc/gdm3/custom.conf",
        "/etc/sddm.conf",
        "/etc/wgetrc",
        "/etc/gitconfig",
        "/etc/exports",
        "/etc/chrony.conf",
    ];
    const PREFIX: &[&str] = &[
        "/etc/NetworkManager/system-connections/",
        "/etc/NetworkManager/conf.d/",
        "/etc/ssh/ssh_host_",
        "/etc/ssh/sshd_config.d/",
        "/etc/ssh/ssh_config.d/",
        "/etc/sudoers.d/",
        "/etc/sysctl.d/",
        "/etc/modprobe.d/",
        "/etc/modules-load.d/",
        "/etc/udev/rules.d/",
        "/etc/security/limits.d/",
        "/etc/systemd/system/",
        "/etc/systemd/user/",
        "/etc/systemd/network/",
        "/etc/systemd/resolved.conf.d/",
        "/etc/systemd/journald.conf.d/",
        "/etc/systemd/logind.conf.d/",
        "/etc/tmpfiles.d/",
        "/etc/sysusers.d/",
        "/etc/wireguard/",
        "/etc/containers/registries.conf.d/",
        "/etc/containers/systemd/",
        "/etc/pki/ca-trust/source/anchors/",
        "/etc/dconf/db/",
        "/etc/dconf/profile/",
        "/etc/X11/xorg.conf.d/",
        "/etc/fonts/local.conf",
        "/etc/environment.d/",
        "/etc/profile.d/",
        "/etc/cups/printers.conf",
        "/etc/cups/ppd/",
        "/etc/libvirt/qemu/",
        "/etc/nginx/",
        "/etc/httpd/",
        "/etc/docker/daemon.json",
        "/etc/tailscale/",
        "/etc/firewalld/zones/",
        "/etc/gdm/",
        "/etc/sddm.conf.d/",
        "/etc/xdg/",
        "/etc/polkit-1/rules.d/",
        "/etc/samba/smb.conf",
        "/etc/opt/",
    ];
    EXACT.contains(&path) || PREFIX.iter().any(|p| path.starts_with(p))
}

/// Portable either as written or once translated (/etc/gdm3/ -> /etc/gdm/).
pub fn etc_portable_any(path: &str, from: Family, to: Family) -> bool {
    etc_portable(path) || etc_portable(&etc_translate(path, from, to))
}

/// Translate a source path to its location on the target distribution.
pub fn etc_translate(path: &str, from: Family, to: Family) -> String {
    if from == Family::Debian && to == Family::Fedora {
        if let Some(r) = path.strip_prefix("/etc/gdm3/") {
            return format!("/etc/gdm/{r}");
        }
        if path == "/etc/default/locale" {
            return "/etc/locale.conf".into();
        }
        if let Some(r) = path.strip_prefix("/usr/local/share/ca-certificates/") {
            return format!("/etc/pki/ca-trust/source/anchors/{r}");
        }
    }
    if from == Family::Fedora
        && to == Family::Debian
        && let Some(r) = path.strip_prefix("/etc/gdm/")
    {
        return format!("/etc/gdm3/{r}");
    }
    path.to_string()
}

/// Inputs gathered outside the planner (file listings, brew index, ...).
pub struct PlanInputs<'a> {
    pub manifest: &'a Manifest,
    pub target: &'a ImageFacts,
    pub target_name: String,
    pub mode: Mode,
    /// package -> file listing (from rpm -ql / dpkg -L)
    pub files: BTreeMap<String, String>,
    pub brew_index: BTreeMap<String, String>,
    pub allow_desktop_change: bool,
    /// Route packages that came with the old OS like user-installed ones.
    pub include_preinstalled: bool,
    /// package -> packages it replaces (unversioned Conflicts/Obsoletes).
    pub replaces: BTreeMap<String, Vec<String>>,
    pub kernel: Option<crate::kernel::KernelPlan>,
}

pub fn build(inp: PlanInputs) -> Result<Plan> {
    let m = inp.manifest;
    let t = inp.target;
    let from = m.os.family;
    let to = t.family.unwrap_or(Family::Other);
    let mut warnings = vec![];

    // --- desktop compatibility ---------------------------------------------
    match desktop::compare(&m.desktop, &t.desktop) {
        Compat::Same | Compat::SourceHeadless => {}
        Compat::TargetHeadless => {
            let msg = format!(
                "target {} has no desktop session but this system runs {}",
                t.image,
                m.desktop.primary.map(Desktop::label).unwrap_or("?")
            );
            if !inp.allow_desktop_change {
                bail!(
                    "{msg}. Pick a desktop image (see `plexy targets`) or pass --allow-desktop-change"
                );
            }
            warnings.push(msg);
        }
        Compat::Different { source, target } => {
            let msg = format!(
                "desktop mismatch: this system runs {} but {} ships {}. Settings, extensions and \
                 session state would not carry over",
                source.label(),
                t.image,
                target.label()
            );
            if !inp.allow_desktop_change {
                bail!(
                    "{msg}. Choose a {} target (see `plexy targets`) or pass --allow-desktop-change",
                    source.label()
                );
            }
            warnings.push(msg);
        }
    }

    let backend = t.backend.unwrap_or(Backend::Bootc);
    let user: BTreeSet<&str> = m
        .packages
        .user_installed
        .iter()
        .map(|p| p.name.as_str())
        .collect();

    // --- which packages are candidates -------------------------------------
    let all = inp.mode == Mode::Toolbox || inp.include_preinstalled;
    let candidates: Vec<(String, &str)> = match all {
        true => m
            .packages
            .installed
            .keys()
            .map(|n| {
                (
                    n.clone(),
                    if user.contains(n.as_str()) {
                        "user"
                    } else {
                        "preinstalled"
                    },
                )
            })
            .collect(),
        false => m
            .packages
            .user_installed
            .iter()
            .map(|p| (p.name.clone(), "user"))
            .collect(),
    };

    let toolbox_possible = matches!(from, Family::Fedora | Family::Debian);
    let mut pkgs = vec![];
    for (name, origin) in candidates {
        let traits = inp
            .files
            .get(&name)
            .map(|f| classify_files(f))
            .unwrap_or_default();
        let mut p = PlannedPkg {
            name: name.clone(),
            dest: Dest::Skip,
            target_names: vec![],
            why: String::new(),
            origin: origin.into(),
            traits: traits.clone(),
            replaces: vec![],
        };
        if is_infrastructure(&name) || is_distro_identity(&name, &m.os.id) {
            p.why =
                "boot chain / package manager / release metadata: provided by the target".into();
            pkgs.push(p);
            continue;
        }
        let mapped = mapping::map_package(&name, from, to, &t.packages);
        // Already in the target image?
        if let Some(mp) = &mapped {
            if !mp.names.is_empty() && mp.names.iter().all(|n| t.packages.contains(n)) {
                p.dest = Dest::Base;
                p.target_names = mp.names.clone();
                p.why = format!("provided by {}", t.pretty_name);
                pkgs.push(p);
                continue;
            }
            if mp.names.is_empty() {
                p.why = "no equivalent needed on the target".into();
                pkgs.push(p);
                continue;
            }
        }
        // Libraries and pure dependencies: in toolbox mode they ride along as
        // dependencies of the leaf packages; never route them individually.
        let is_lib = name.starts_with("lib") && !traits.cli && !traits.gui;
        if is_lib && origin == "preinstalled" {
            if inp.mode == Mode::Toolbox {
                p.dest = Dest::Toolbox;
                p.target_names = vec![name.clone()];
                p.why = "library (dependency of containerised software)".into();
            } else {
                p.why = "library: installed automatically with the software that needs it".into();
            }
            pkgs.push(p);
            continue;
        }

        let image_ok = mapped
            .as_ref()
            .map(|mp| !mp.names.is_empty())
            .unwrap_or(false);
        let image_names = mapped
            .as_ref()
            .map(|mp| mp.names.clone())
            .unwrap_or_default();
        let formula = if traits.cli && !traits.host_level && !traits.gui {
            mapping::brew_formula(&name, &inp.brew_index)
        } else {
            None
        };

        let replaces = match from == to {
            true => inp.replaces.get(&name).cloned().unwrap_or_default(),
            false => vec![],
        };
        let route_host = |p: &mut PlannedPkg, why: &str| {
            if image_ok {
                p.dest = Dest::Image;
                p.target_names = image_names.clone();
                p.replaces = replaces.clone();
                p.why = why.into();
            } else {
                p.dest = Dest::Skip;
                p.why = format!(
                    "{why}, but no package with this name exists for {}",
                    t.pretty_name
                );
            }
        };

        match inp.mode {
            Mode::Image => {
                if image_ok {
                    route_host(&mut p, "user-installed");
                } else if toolbox_possible {
                    p.dest = Dest::Toolbox;
                    p.target_names = vec![name.clone()];
                    p.why =
                        "no equivalent package for the target; keep it in a source-distro toolbox"
                            .into();
                } else {
                    p.why = "no equivalent package found".into();
                }
            }
            Mode::Brew => {
                if let Some(f) = formula {
                    p.dest = Dest::Brew;
                    p.target_names = vec![f];
                    p.why = "command-line tool with a Homebrew formula".into();
                } else if image_ok {
                    route_host(&mut p, "no Homebrew formula (or not a CLI tool)");
                } else if toolbox_possible {
                    p.dest = Dest::Toolbox;
                    p.target_names = vec![name.clone()];
                    p.why = "no target package and no formula; keep it in a toolbox".into();
                }
            }
            Mode::Toolbox | Mode::Hybrid => {
                if traits.host_level && origin == "preinstalled" {
                    p.why = format!(
                        "system service shipped by {} but not by the target; layer it in the review if you use it",
                        m.os.name
                    );
                } else if traits.host_level {
                    route_host(
                        &mut p,
                        "system-level (services/drivers/shell extensions must run on the host)",
                    );
                } else if inp.mode == Mode::Hybrid
                    && traits.gui
                    && mapping::app_flatpak(&name).is_some()
                {
                    p.dest = Dest::Flatpak;
                    p.target_names = vec![mapping::app_flatpak(&name).unwrap().to_string()];
                    p.why = "desktop app: the Flathub build replaces the distro package".into();
                } else if let (Mode::Hybrid, Some(f)) = (inp.mode, formula.clone()) {
                    p.dest = Dest::Brew;
                    p.target_names = vec![f];
                    p.why = "command-line tool with a Homebrew formula".into();
                } else if toolbox_possible {
                    p.dest = Dest::Toolbox;
                    p.target_names = vec![name.clone()];
                    p.why = if traits.gui {
                        "desktop app: runs in the toolbox, launcher exported to the host menu"
                            .into()
                    } else {
                        "runs in the toolbox, commands exported to ~/.local/bin".into()
                    };
                } else {
                    route_host(&mut p, "no toolbox for this distribution");
                }
            }
        }
        pkgs.push(p);
    }

    // Snaps never run on the targets: offer Flathub equivalents.
    let mut flatpaks = vec![];
    for s in &m.wrappers.snaps {
        match &s.flatpak {
            Some(f) => {
                flatpaks.push(f.clone());
                pkgs.push(PlannedPkg {
                    name: format!("snap:{}", s.name),
                    dest: Dest::Flatpak,
                    target_names: vec![f.clone()],
                    why: "snaps are not supported on the target; Flathub equivalent".into(),
                    origin: "user".into(),
                    traits: PkgTraits::default(),
                    replaces: vec![],
                });
            }
            None => warnings.push(format!(
                "snap '{}' has no known Flatpak equivalent; reinstall it manually",
                s.name
            )),
        }
    }
    for p in pkgs
        .iter()
        .filter(|p| p.dest == Dest::Flatpak && !p.name.starts_with("snap:"))
    {
        flatpaks.extend(p.target_names.iter().cloned());
    }
    flatpaks.sort();
    flatpaks.dedup();
    pkgs.sort_by(|a, b| a.dest.cmp(&b.dest).then(a.name.cmp(&b.name)));

    // --- configs ------------------------------------------------------------
    // Copy every local /etc change only between compatible platforms: any
    // pair of Fedora-family systems (remixes share Fedora's base), or the
    // same Debian-family distribution. Otherwise only portable config.
    let same_platform = from == to && (from == Family::Fedora || m.os.id == t.os_id);
    let dest_of: BTreeMap<&str, Dest> = pkgs.iter().map(|p| (p.name.as_str(), p.dest)).collect();
    let mut configs = vec![];
    for c in &m.packages.config_changes {
        if c.kind == ConfigKind::Deleted {
            continue;
        }
        let mut pc = PlannedConfig {
            path: c.path.clone(),
            kind: c.kind,
            package: c.package.clone(),
            action: ConfigAction::Skip,
            why: String::new(),
        };
        if etc_denied(&c.path) {
            pc.why = "managed by the boot chain / package manager / account merge".into();
        } else if let Some(d) = c.package.as_deref().and_then(|p| dest_of.get(p)) {
            match d {
                Dest::Toolbox => {
                    pc.action = ConfigAction::Toolbox;
                    pc.why = "config of containerised software".into();
                }
                Dest::Brew => {
                    pc.action = ConfigAction::Brew;
                    pc.why = "config of a tool moved to Homebrew".into();
                }
                Dest::Skip if !same_platform && !etc_portable_any(&c.path, from, to) => {
                    pc.why = "package not migrated".into();
                }
                _ => {
                    pc.action = ConfigAction::Etc;
                    pc.why = "local modification".into();
                }
            }
        } else if same_platform || etc_portable_any(&c.path, from, to) {
            pc.action = ConfigAction::Etc;
            pc.why = match c.kind {
                ConfigKind::Unowned => "created locally".into(),
                _ => "local modification".into(),
            };
        } else {
            pc.why = "distribution-specific file (different family)".into();
        }
        configs.push(pc);
    }

    // Repositories: carry only what the target lacks (third-party, COPR,
    // remix repos like Terra); the target's own distro repos are already there.
    let needed = |r: &&crate::manifest::RepoFile| {
        !is_distro_repo(&r.content)
            && match &r.owner {
                None => true,
                Some(o) => !t.packages.contains(o),
            }
    };
    // A repo definition only works in the system image when the image is the
    // same platform: any Fedora-family pair ($releasever based), but for the
    // Debian family only the same distribution and release (suites are
    // hard-coded in sources.list). The toolbox *is* the source distro.
    let image_repos_ok = from == to
        && (from == Family::Fedora || (m.os.id == t.os_id && m.os.version_id == t.os_version));
    let repo_files: Vec<String> = if image_repos_ok {
        m.repos
            .iter()
            .filter(needed)
            .map(|r| r.path.clone())
            .collect()
    } else {
        vec![]
    };
    let toolbox_repo_files: Vec<String> = m
        .repos
        .iter()
        .filter(|r| {
            r.owner
                .as_deref()
                .map(|o| {
                    !mapping::is_infrastructure(o)
                        || o.contains("rpmfusion")
                        || o.starts_with("terra")
                        || o.starts_with("ultramarine-repos")
                })
                .unwrap_or(true)
        })
        .map(|r| r.path.clone())
        .collect();
    if !image_repos_ok
        && m.repos
            .iter()
            .any(|r| r.owner.is_none() && !is_distro_repo(&r.content))
    {
        warnings.push(format!(
            "third-party repositories from {} cannot be used by {}; their packages are routed to the toolbox",
            m.os.pretty_name, t.pretty_name
        ));
    }

    let toolbox = if pkgs.iter().any(|p| p.dest == Dest::Toolbox) {
        let engine = if t.has_distrobox {
            "distrobox"
        } else if t.has_toolbox {
            "toolbox"
        } else {
            "distrobox"
        };
        if !t.has_distrobox && !t.has_toolbox {
            warnings.push("target has neither distrobox nor toolbox; distrobox will be layered into the image".into());
        }
        Some(ToolboxPlan {
            engine: engine.into(),
            name: format!("{}-{}", m.os.id, m.os.version_id.replace('.', "")),
            base_image: toolbox_base_image(&m.os.id, from, &m.os.version_id),
            image_tag: format!("localhost/plexy-toolbox-{}:{}", m.os.id, m.os.version_id),
        })
    } else {
        None
    };

    // Units: replay the old system's enabled state, translating names across
    // families. Disables are only replayed within a family, and never for
    // units the target's desktop/session depends on.
    let mut units_to_enable: Vec<String> = m
        .system
        .enabled_units
        .iter()
        // across platforms the target's own presets decide systemd's units
        // (Ubuntu's systemd-networkd would fight Fedora's or Vanilla's
        // NetworkManager; systemd-networkd-wait-online then fails at boot)
        .filter(|u| same_platform || !u.starts_with("systemd-"))
        .map(|u| translate_unit(u, from, to))
        .collect();
    units_to_enable.sort();
    units_to_enable.dedup();
    let units_to_disable: Vec<String> = if same_platform {
        m.system
            .disabled_units
            .iter()
            .filter(|u| !unit_protected(u))
            .cloned()
            .collect()
    } else {
        vec![]
    };

    let derived_image = format!(
        "localhost/plexy/{}:{}",
        inp.target_name.replace(['/', ':', '.'], "-"),
        m.hostname_tag()
    );
    let mut plan = Plan {
        plexy_version: env!("CARGO_PKG_VERSION").into(),
        created: util::now_unix(),
        mode: inp.mode,
        source: m.os.pretty_name.clone(),
        source_family: from,
        source_desktop: m.desktop.primary,
        target_image: t.image.clone(),
        target_name: inp.target_name,
        target_family: to,
        target_desktop: t.desktop.primary,
        backend,
        derived_image,
        packages: pkgs,
        configs,
        repo_files,
        toolbox_repo_files,
        toolbox,
        flatpaks_to_install: flatpaks,
        units_to_enable,
        units_to_disable,
        kargs: m.boot.kargs.clone(),
        warnings,
        update_remote: t.image.clone(),
        update_signed: false,
        layering: crate::registry::Layering::Image,
        conversion_status: None,
        plexy_in_image: t.usr_local_in_usr && backend == Backend::Bootc,
        kernel: inp.kernel,
    };
    let companions: Vec<crate::kernel::Companion> = m
        .kernel
        .as_ref()
        .map(|k| k.companions.clone())
        .unwrap_or_default();
    apply_kernel(&mut plan, &companions);
    Ok(plan)
}

/// Route the source kernel's packages and companions by `plan.kernel.keep`:
/// kept, the companions are layered with it (adding the ones that were only
/// dependencies on the old system, from `companions`); otherwise they stay
/// behind with the kernel. Called again when the review toggles it.
pub fn apply_kernel(plan: &mut Plan, companions: &[crate::kernel::Companion]) {
    let Some(k) = plan.kernel.clone() else {
        return;
    };
    let skipped: BTreeMap<&str, &str> = k
        .skipped
        .iter()
        .map(|(n, w)| (n.as_str(), w.as_str()))
        .collect();
    for p in plan.packages.iter_mut() {
        if k.packages.contains(&p.name) {
            p.dest = Dest::Skip;
            p.target_names = vec![];
            p.why = if k.keep {
                format!(
                    "source kernel: replaces {} {} through an rpm-ostree override",
                    k.target_base, k.target_kver
                )
            } else {
                "boot chain: the target's own kernel replaces it (--keep-kernel keeps it)".into()
            };
        } else if k.companions.contains(&p.name) {
            if k.keep {
                p.dest = Dest::Image;
                p.target_names = vec![p.name.clone()];
                p.why = format!(
                    "installed with the source kernel {} (from {})",
                    k.base, k.repo
                );
            } else {
                p.dest = Dest::Skip;
                p.why = format!(
                    "belongs to the source kernel {}, which is not kept (--keep-kernel keeps it)",
                    k.base
                );
            }
        } else if let Some(why) = skipped.get(p.name.as_str()) {
            p.dest = Dest::Skip;
            p.why = why.to_string();
        }
    }
    if k.keep {
        for c in companions.iter().filter(|c| k.companions.contains(&c.name)) {
            if plan.packages.iter().any(|p| p.name == c.name) {
                continue;
            }
            plan.packages.push(PlannedPkg {
                name: c.name.clone(),
                dest: Dest::Image,
                target_names: vec![c.name.clone()],
                why: format!(
                    "installed with the source kernel {} (from {})",
                    k.base, k.repo
                ),
                origin: "preinstalled".into(),
                traits: PkgTraits {
                    host_level: c.host_level,
                    ..Default::default()
                },
                replaces: c.replaces.clone(),
            });
        }
        if let Some(f) = &k.repo_file
            && !plan.repo_files.contains(f)
        {
            plan.repo_files.push(f.clone());
        }
    }
    for p in plan.packages.iter_mut() {
        if let Some(c) = companions.iter().find(|c| c.name == p.name)
            && p.replaces.is_empty()
        {
            p.replaces = c.replaces.clone();
        }
    }
    plan.packages
        .sort_by(|a, b| a.dest.cmp(&b.dest).then(a.name.cmp(&b.name)));
}

/// Service names that differ between the Debian and Fedora families.
const UNIT_NAMES: &[(&str, &str)] = &[
    ("ssh.service", "sshd.service"),
    ("ssh.socket", "sshd.service"),
    ("cron.service", "crond.service"),
    ("apache2.service", "httpd.service"),
    ("gdm3.service", "gdm.service"),
    ("smbd.service", "smb.service"),
    ("nmbd.service", "nmb.service"),
    ("isc-dhcp-server.service", "dhcpd.service"),
    ("bind9.service", "named.service"),
    ("named.service", "named.service"),
    ("mysql.service", "mariadb.service"),
    ("redis-server.service", "redis.service"),
    ("nfs-kernel-server.service", "nfs-server.service"),
    ("ufw.service", "firewalld.service"),
    ("rsyslog.service", "rsyslog.service"),
    ("chrony.service", "chronyd.service"),
    ("systemd-timesyncd.service", "chronyd.service"),
];

pub fn translate_unit(u: &str, from: Family, to: Family) -> String {
    match (from, to) {
        (Family::Debian, Family::Fedora) => UNIT_NAMES
            .iter()
            .find(|(d, _)| *d == u)
            .map(|(_, f)| f.to_string()),
        (Family::Fedora, Family::Debian) => UNIT_NAMES
            .iter()
            .find(|(_, f)| *f == u)
            .map(|(d, _)| d.to_string()),
        _ => None,
    }
    .unwrap_or_else(|| u.to_string())
}

/// Units whose disabled state must never be replayed onto the target: the
/// display manager and session plumbing the new desktop needs.
pub fn unit_protected(u: &str) -> bool {
    const PROTECTED: &[&str] = &[
        "gdm.service",
        "gdm3.service",
        "sddm.service",
        "lightdm.service",
        "plasmalogin.service",
        "cosmic-greeter.service",
        "display-manager.service",
        "dbus.service",
        "dbus-broker.service",
        "NetworkManager.service",
        "polkit.service",
        "upower.service",
        "rtkit-daemon.service",
        "accounts-daemon.service",
        "bluetooth.service",
        "switcheroo-control.service",
    ];
    PROTECTED.contains(&u)
        || u.starts_with("systemd-")
        || u.ends_with(".target")
        || u.starts_with("ostree")
        || u.starts_with("bootc")
        || u.starts_with("rpm-ostree")
}

/// Packages that make up the *identity* of the old distribution — its
/// release files, branding, keys, installer and default configs. They are
/// not software the user runs and conflict with any other distribution's
/// release packages (e.g. inside a Fedora toolbox).
pub fn is_distro_identity(pkg: &str, os_id: &str) -> bool {
    let prefixed = !os_id.is_empty() && pkg.starts_with(&format!("{os_id}-"));
    prefixed
        || pkg.ends_with("-logos")
        || pkg.contains("-logos-")
        || pkg.contains("-backgrounds")
        || pkg.ends_with("-gpg-keys")
        || pkg.contains("-release-")
        || pkg.contains("system-configs")
        || pkg.starts_with("taidan-")
}

/// Repository definitions that only point at a distribution's own archive
/// (the toolbox image and the target already have those).
pub fn is_distro_repo(content: &str) -> bool {
    const DISTRO_HOSTS: &[&str] = &[
        "archive.ubuntu.com",
        "security.ubuntu.com",
        "ports.ubuntu.com",
        "deb.debian.org",
        "security.debian.org",
        "mirrors.fedoraproject.org",
        "download.fedoraproject.org",
        "dl.fedoraproject.org",
        "cdn.fedoraproject.org",
    ];
    let mut urls: Vec<String> = Vec::new();
    for w in content
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .flat_map(str::split_whitespace)
        .filter(|w| w.contains("://") || w.starts_with("metalink=") || w.starts_with("baseurl="))
    {
        // Debian cloud images: `URIs: mirror+file:///etc/apt/mirrors/debian.list`,
        // a file listing the actual mirrors. Judge those instead.
        match w.strip_prefix("mirror+file://") {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(list) => urls.extend(
                    list.lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty() && !l.starts_with('#'))
                        .map(|l| l.split_whitespace().next().unwrap_or(l).to_string()),
                ),
                Err(_) => urls.push(w.to_string()),
            },
            None => urls.push(w.to_string()),
        }
    }
    !urls.is_empty()
        && urls
            .iter()
            .all(|u| DISTRO_HOSTS.iter().any(|h| u.contains(h)))
}

/// Container image used for the source-distro toolbox.
pub fn toolbox_base_image(id: &str, family: Family, version: &str) -> String {
    match (id, family) {
        ("fedora", _) => format!("registry.fedoraproject.org/fedora-toolbox:{version}"),
        ("ubuntu", _) => format!("quay.io/toolbx/ubuntu-toolbox:{version}"),
        ("debian", _) => format!("quay.io/toolbx-images/debian-toolbox:{version}"),
        // Enterprise Linux clones: their own toolbox images, by major version
        // (a Fedora toolbox would be the wrong distribution and release).
        ("almalinux", _) | ("rocky", _) | ("centos", _) => {
            let major = version.split('.').next().unwrap_or(version);
            match id {
                "rocky" => format!("quay.io/toolbx-images/rockylinux-toolbox:{major}"),
                "centos" => format!("quay.io/toolbx-images/centos-toolbox:stream{major}"),
                _ => format!("quay.io/toolbx-images/{id}-toolbox:{major}"),
            }
        }
        // Fedora remixes (Ultramarine, Nobara, ...) run fine on the Fedora
        // toolbox image plus their own repositories (copied from the host).
        (_, Family::Fedora) => format!("registry.fedoraproject.org/fedora-toolbox:{version}"),
        (_, Family::Debian) => "quay.io/toolbx-images/debian-toolbox:latest".into(),
        _ => "registry.fedoraproject.org/fedora-toolbox:latest".into(),
    }
}

// ------------------------------------------------------------- reporting ---

pub fn print_summary(p: &Plan) {
    println!();
    println!(
        "  source : {} ({:?}, desktop: {})",
        p.source,
        p.source_family,
        p.source_desktop.map(Desktop::label).unwrap_or("?")
    );
    println!(
        "  target : {} [{}] ({:?}, desktop: {}, backend: {:?})",
        p.target_name,
        p.target_image,
        p.target_family,
        p.target_desktop.map(Desktop::label).unwrap_or("?"),
        p.backend
    );
    println!("  mode   : {:?}", p.mode);
    println!();
    for d in [
        Dest::Base,
        Dest::Image,
        Dest::Toolbox,
        Dest::Brew,
        Dest::Flatpak,
        Dest::Skip,
    ] {
        let v = p.by_dest(d);
        if v.is_empty() {
            continue;
        }
        println!("  {:<30} {:>5} package(s)", d.label(), v.len());
        if matches!(d, Dest::Image | Dest::Brew | Dest::Flatpak)
            || (d == Dest::Toolbox && v.len() <= 40)
        {
            let names: Vec<String> = v
                .iter()
                .map(|x| {
                    if x.target_names.len() == 1 && x.target_names[0] == x.name {
                        x.name.clone()
                    } else {
                        format!("{}→{}", x.name, x.target_names.join("+"))
                    }
                })
                .collect();
            for chunk in names.chunks(6) {
                println!("      {}", chunk.join("  "));
            }
        }
    }
    let n = |a: ConfigAction| p.configs.iter().filter(|c| c.action == a).count();
    println!();
    println!(
        "  config files: {} → /etc, {} → toolbox, {} → brew, {} skipped",
        n(ConfigAction::Etc),
        n(ConfigAction::Toolbox),
        n(ConfigAction::Brew),
        n(ConfigAction::Skip)
    );
    if let Some(tb) = &p.toolbox {
        println!(
            "  toolbox: {} '{}' from {}",
            tb.engine, tb.name, tb.base_image
        );
    }
    if let Some(k) = &p.kernel {
        print_kernel(k);
    }
    for w in &p.warnings {
        warn(w);
    }
    println!();
}

fn print_kernel(k: &crate::kernel::KernelPlan) {
    println!();
    println!(
        "  kernel : this system runs {} {} (from {}); the target ships {} {}",
        k.base,
        k.source_kver,
        if k.repo.is_empty() { "?" } else { &k.repo },
        k.target_base,
        k.target_kver
    );
    if !k.keep {
        match &k.blocked {
            Some(b) => println!("           the target's kernel is used: {b}"),
            None => println!(
                "           the target's kernel is used ([k] in the review or --keep-kernel keeps {})",
                k.base
            ),
        }
        return;
    }
    let wrap = |label: &str, names: Vec<String>| {
        for (i, chunk) in names.chunks(6).enumerate() {
            println!(
                "           {:<9}{}",
                if i == 0 { label } else { "" },
                chunk.join("  ")
            );
        }
    };
    println!("           kept: rpm-ostree override, replayed at every update");
    wrap(
        "install",
        k.packages
            .iter()
            .map(|p| match k.resolved.get(p) {
                Some(v) if *p == k.base => format!("{p} ({v})"),
                _ => p.clone(),
            })
            .collect(),
    );
    wrap("with", k.companions.clone());
    wrap("remove", k.remove.clone());
    for (p, why) in &k.lost {
        warn(format!("{p} is removed with the target's kernel: {why}"));
    }
    if !k.source_modules.is_empty() {
        warn(format!(
            "modules built for {} {} are not carried: {}",
            k.base,
            k.source_kver,
            k.source_modules.join(" ")
        ));
    }
    for (p, why) in &k.skipped {
        println!("           not carried: {p} — {why}");
    }
    for n in &k.notes {
        println!("           note: {n}");
    }
}

/// Interactive review: let the user move whole groups or single packages.
pub fn review(
    plan: &mut Plan,
    companions: &[crate::kernel::Companion],
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<bool> {
    loop {
        print_summary(plan);
        writeln!(
            out,
            "  [a]ccept  [m]ove a package  [g]roup move  [l]ist a group{}  [q]uit without changes",
            match &plan.kernel {
                Some(k) if k.keep => "  [k] use the target's kernel",
                Some(_) => "  [k]eep the source kernel",
                None => "",
            }
        )?;
        write!(out, "  choice> ")?;
        out.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(false);
        }
        match line.trim() {
            "a" | "" => return Ok(true),
            "q" => return Ok(false),
            "k" if plan.kernel.is_some() => {
                let k = plan.kernel.as_mut().unwrap();
                match (&k.blocked, k.keep) {
                    (Some(b), false) => writeln!(out, "  the source kernel cannot be kept: {b}")?,
                    _ => k.keep = !k.keep,
                }
                apply_kernel(plan, companions);
            }
            "l" => {
                let d = ask_dest(input, out, "which group")?;
                for p in plan.by_dest(d) {
                    writeln!(out, "    {:<40} {}", p.name, p.why)?;
                }
                write!(out, "  (enter to continue)")?;
                out.flush()?;
                let mut s = String::new();
                input.read_line(&mut s)?;
            }
            "m" => {
                write!(out, "  package name> ")?;
                out.flush()?;
                let mut n = String::new();
                input.read_line(&mut n)?;
                let n = n.trim().to_string();
                let d = ask_dest(input, out, "move to")?;
                match plan.packages.iter_mut().find(|p| p.name == n) {
                    Some(p) => {
                        p.dest = d;
                        if p.target_names.is_empty() {
                            p.target_names = vec![p.name.clone()];
                        }
                        p.why = "chosen by user".into();
                    }
                    None => writeln!(out, "  no such package in the plan")?,
                }
            }
            "g" => {
                let from = ask_dest(input, out, "move every package from")?;
                let to = ask_dest(input, out, "to")?;
                for p in plan.packages.iter_mut().filter(|p| p.dest == from) {
                    p.dest = to;
                    if p.target_names.is_empty() {
                        p.target_names = vec![p.name.clone()];
                    }
                    p.why = "chosen by user (group move)".into();
                }
            }
            _ => writeln!(out, "  ?")?,
        }
    }
}

fn ask_dest(input: &mut dyn BufRead, out: &mut dyn Write, prompt: &str) -> Result<Dest> {
    loop {
        write!(out, "  {prompt} (base/image/toolbox/brew/flatpak/skip)> ")?;
        out.flush()?;
        let mut s = String::new();
        if input.read_line(&mut s)? == 0 {
            bail!("input closed");
        }
        let d = match s.trim() {
            "base" => Dest::Base,
            "image" => Dest::Image,
            "toolbox" => Dest::Toolbox,
            "brew" => Dest::Brew,
            "flatpak" => Dest::Flatpak,
            "skip" => Dest::Skip,
            _ => continue,
        };
        return Ok(d);
    }
}

pub fn save(plan: &Plan, path: &std::path::Path) -> Result<()> {
    util::write_file(path, serde_json::to_string_pretty(plan)?)?;
    info(format!("plan written to {}", path.display()));
    Ok(())
}

pub fn load(path: &std::path::Path) -> Result<Plan> {
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::{Companion, KernelPlan};

    /// A Fedora-family source running kernel-surface, planned for a target
    /// that ships its stock kernel.
    fn surface_plan(keep: bool) -> (Plan, Vec<Companion>) {
        let mut m: Manifest =
            serde_json::from_str(include_str!("../tests/data/manifest-min.json")).unwrap();
        m.os.id = "ultramarine".into();
        m.os.family = Family::Fedora;
        for n in ["kernel-surface", "iptsd", "htop", "libwacom-surface"] {
            m.packages.installed.insert(n.into(), "1-1".into());
            m.packages.user_installed.push(crate::pkg::UserPkg {
                name: n.into(),
                evidence: "history".into(),
            });
        }
        let companions = vec![
            Companion {
                name: "iptsd".into(),
                srpm: "iptsd".into(),
                host_level: true,
                ..Default::default()
            },
            Companion {
                name: "libwacom-surface".into(),
                srpm: "libwacom-surface".into(),
                replaces: vec!["libwacom".into()],
                ..Default::default()
            },
            Companion {
                name: "libwacom-surface-data".into(),
                srpm: "libwacom-surface".into(),
                replaces: vec!["libwacom-data".into()],
                ..Default::default()
            },
        ];
        m.kernel = Some(crate::kernel::SourceKernel {
            companions: companions.clone(),
            ..Default::default()
        });
        let t = ImageFacts {
            image: "ghcr.io/ublue-os/bluefin:stable".into(),
            family: Some(Family::Fedora),
            os_id: "bluefin".into(),
            pretty_name: "Bluefin".into(),
            packages: ["libwacom", "libwacom-data", "bash"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            ..Default::default()
        };
        let mut files = BTreeMap::new();
        files.insert(
            "iptsd".to_string(),
            "/usr/lib/systemd/system/iptsd@.service\n/usr/bin/iptsd\n".to_string(),
        );
        files.insert(
            "libwacom-surface".to_string(),
            "/usr/lib/udev/rules.d/65-libwacom.rules\n".to_string(),
        );
        let kernel = KernelPlan {
            keep,
            base: "kernel-surface".into(),
            repo: "linux-surface".into(),
            packages: vec!["kernel-surface".into(), "kernel-surface-core".into()],
            target_base: "kernel".into(),
            target_kver: "7.2.5-200.fc44.x86_64".into(),
            companions: vec![
                "iptsd".into(),
                "libwacom-surface".into(),
                "libwacom-surface-data".into(),
            ],
            repo_file: Some("/etc/yum.repos.d/linux-surface.repo".into()),
            ..Default::default()
        };
        let mut replaces = BTreeMap::new();
        replaces.insert("libwacom-surface".to_string(), vec!["libwacom".to_string()]);
        let p = build(PlanInputs {
            manifest: &m,
            target: &t,
            target_name: "bluefin".into(),
            mode: Mode::Hybrid,
            files,
            brew_index: BTreeMap::new(),
            allow_desktop_change: true,
            include_preinstalled: false,
            replaces,
            kernel: Some(kernel),
        })
        .unwrap();
        (p, companions)
    }

    fn dest_of(p: &Plan, n: &str) -> Option<Dest> {
        p.packages.iter().find(|x| x.name == n).map(|x| x.dest)
    }

    #[test]
    fn kept_kernel_layers_its_companions() {
        let (p, _) = surface_plan(true);
        assert_eq!(dest_of(&p, "kernel-surface"), Some(Dest::Skip));
        assert_eq!(dest_of(&p, "iptsd"), Some(Dest::Image));
        assert_eq!(dest_of(&p, "libwacom-surface"), Some(Dest::Image));
        // only a dependency on the old system, added for the kernel
        assert_eq!(dest_of(&p, "libwacom-surface-data"), Some(Dest::Image));
        let data = p
            .packages
            .iter()
            .find(|x| x.name == "libwacom-surface-data")
            .unwrap();
        assert_eq!(data.replaces, vec!["libwacom-data"]);
        assert!(
            p.repo_files
                .contains(&"/etc/yum.repos.d/linux-surface.repo".to_string())
        );
        assert!(
            p.packages
                .iter()
                .find(|x| x.name == "kernel-surface")
                .unwrap()
                .why
                .contains("rpm-ostree override")
        );
    }

    #[test]
    fn dropped_kernel_leaves_its_companions_behind_and_review_toggles() {
        let (mut p, companions) = surface_plan(false);
        assert_eq!(dest_of(&p, "iptsd"), Some(Dest::Skip));
        assert_eq!(dest_of(&p, "libwacom-surface-data"), None);
        let mut out = Vec::new();
        let mut input = std::io::Cursor::new("k\na\n");
        assert!(review(&mut p, &companions, &mut input, &mut out).unwrap());
        assert!(p.kernel.as_ref().unwrap().keep);
        assert_eq!(dest_of(&p, "iptsd"), Some(Dest::Image));
        assert_eq!(dest_of(&p, "libwacom-surface-data"), Some(Dest::Image));
        // blocked: the toggle refuses
        let (mut p, companions) = surface_plan(false);
        p.kernel.as_mut().unwrap().blocked = Some("no repository".into());
        let mut out = Vec::new();
        let mut input = std::io::Cursor::new("k\na\n");
        review(&mut p, &companions, &mut input, &mut out).unwrap();
        assert!(!p.kernel.as_ref().unwrap().keep);
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("cannot be kept: no repository")
        );
    }

    #[test]
    fn replacing_packages_carry_what_they_replace() {
        let (p, _) = surface_plan(false);
        // htop has no conflicts; nothing to replace
        assert!(
            p.packages
                .iter()
                .filter(|x| x.name == "htop")
                .all(|x| x.replaces.is_empty())
        );
    }

    #[test]
    fn debian_mirror_file_sources_are_distro_repos() {
        let dir = std::env::temp_dir().join(format!("plexy-mirrors-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ok = dir.join("debian.list");
        std::fs::write(&ok, "https://deb.debian.org/debian\n").unwrap();
        let bad = dir.join("other.list");
        std::fs::write(
            &bad,
            "https://deb.debian.org/debian\nhttps://repo.example.com/apt\n",
        )
        .unwrap();
        let src = |p: &std::path::Path| {
            format!(
                "Types: deb\nURIs: mirror+file://{}\nSuites: trixie\n",
                p.display()
            )
        };
        assert!(is_distro_repo(&src(&ok)));
        assert!(!is_distro_repo(&src(&bad)));
        assert!(!is_distro_repo(&src(&dir.join("missing.list"))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gdm_autologin_is_portable_between_debian_platforms() {
        assert!(etc_portable_any(
            "/etc/gdm3/custom.conf",
            Family::Debian,
            Family::Debian
        ));
        assert!(etc_portable_any(
            "/etc/gdm3/custom.conf",
            Family::Debian,
            Family::Fedora
        ));
        assert!(!etc_portable_any(
            "/etc/pam.d/common-auth",
            Family::Debian,
            Family::Debian
        ));
    }

    #[test]
    fn file_classification() {
        let t = classify_files(
            "/usr/bin/nginx\n/usr/lib/systemd/system/nginx.service\n/etc/nginx/nginx.conf\n",
        );
        assert!(t.host_level && t.cli);
        assert_eq!(t.units, vec!["nginx.service"]);
        let t = classify_files(
            "/usr/bin/plexyapp\n/usr/share/applications/plexy-test-app.desktop\n/usr/share/man/man1/plexyapp.1.gz\n",
        );
        assert!(t.cli && t.gui && !t.host_level);
        let t = classify_files(
            "/usr/share/gnome-shell/extensions/appindicatorsupport@rgcjonas.gmail.com/metadata.json\n",
        );
        assert!(t.shell_extension && t.host_level);
        let t = classify_files("/usr/bin/\n/usr/share/doc/x/README\n");
        assert!(!t.cli);
    }

    #[test]
    fn etc_policies() {
        assert!(etc_denied("/etc/fstab"));
        assert!(etc_denied("/etc/yum.repos.d/fedora.repo"));
        assert!(etc_denied("/etc/apt/sources.list"));
        assert!(etc_denied("/etc/nginx/nginx.conf.rpmnew"));
        assert!(!etc_denied("/etc/nginx/nginx.conf"));
        assert!(etc_denied(
            "/etc/systemd/system/multi-user.target.wants/nginx.service"
        ));
        assert!(etc_denied("/etc/systemd/system/default.target"));
        assert!(!etc_denied("/etc/systemd/system/myapp.service"));
        assert!(!etc_denied(
            "/etc/systemd/system/nginx.service.d/override.conf"
        ));
        assert!(etc_denied("/etc/dconf/db/local"));
        assert!(!etc_denied("/etc/dconf/db/local.d/00-custom"));
        assert!(etc_denied("/etc/authselect/system-auth"));
        assert!(etc_denied("/etc/containers/policy.json"));
        assert!(etc_denied("/etc/containers/registries.d/default.yaml"));
        assert!(etc_denied("/etc/systemd/system/snap-firefox-9057.mount"));
        assert!(etc_denied("/etc/systemd/system/snapd.service.d/x.conf"));
        assert!(!etc_denied("/etc/snapper/configs/root"));
        assert!(etc_portable_any(
            "/etc/gdm3/custom.conf",
            Family::Debian,
            Family::Fedora
        ));
        assert!(etc_portable(
            "/etc/NetworkManager/system-connections/home.nmconnection"
        ));
        assert!(etc_portable("/etc/ssh/ssh_host_ed25519_key"));
        assert!(!etc_portable("/etc/apt/apt.conf.d/10periodic"));
    }

    #[test]
    fn etc_translation() {
        assert_eq!(
            etc_translate("/etc/gdm3/custom.conf", Family::Debian, Family::Fedora),
            "/etc/gdm/custom.conf"
        );
        assert_eq!(
            etc_translate("/etc/gdm/custom.conf", Family::Fedora, Family::Debian),
            "/etc/gdm3/custom.conf"
        );
        assert_eq!(
            etc_translate("/etc/hosts", Family::Debian, Family::Fedora),
            "/etc/hosts"
        );
    }

    #[test]
    fn distro_identity_packages() {
        assert!(is_distro_identity(
            "ultramarine-release-common",
            "ultramarine"
        ));
        assert!(is_distro_identity("ultramarine-logos", "ultramarine"));
        assert!(is_distro_identity("terra-gpg-keys", "ultramarine"));
        assert!(is_distro_identity("fedora-logos-httpd", "fedora"));
        assert!(is_distro_identity(
            "taidan-ultramarine-configs",
            "ultramarine"
        ));
        assert!(!is_distro_identity("vlc", "ultramarine"));
        assert!(!is_distro_identity("neovim", "ubuntu"));
    }

    #[test]
    fn unit_translation_and_protection() {
        assert_eq!(
            translate_unit("ssh.service", Family::Debian, Family::Fedora),
            "sshd.service"
        );
        assert_eq!(
            translate_unit("ssh.socket", Family::Debian, Family::Fedora),
            "sshd.service"
        );
        assert_eq!(
            translate_unit("sshd.service", Family::Fedora, Family::Debian),
            "ssh.service"
        );
        assert_eq!(
            translate_unit("nginx.service", Family::Debian, Family::Fedora),
            "nginx.service"
        );
        assert_eq!(
            translate_unit("ssh.service", Family::Fedora, Family::Fedora),
            "ssh.service"
        );
        assert!(unit_protected("gdm.service"));
        assert!(unit_protected("reboot.target"));
        assert!(unit_protected("systemd-sysext.service"));
        assert!(!unit_protected("cups.service"));
    }

    #[test]
    fn distro_repo_detection() {
        assert!(is_distro_repo(
            "Types: deb\nURIs: http://archive.ubuntu.com/ubuntu/\nSuites: resolute\n"
        ));
        assert!(is_distro_repo(
            "[fedora]\nmetalink=https://mirrors.fedoraproject.org/metalink?repo=fedora-$releasever\n"
        ));
        assert!(!is_distro_repo(
            "deb [signed-by=/k.gpg] https://download.docker.com/linux/ubuntu noble stable\n"
        ));
        assert!(!is_distro_repo(
            "[terra]\nbaseurl=https://repos.fyralabs.com/terra$releasever\n"
        ));
    }

    #[test]
    fn toolbox_images() {
        assert_eq!(
            toolbox_base_image("fedora", Family::Fedora, "44"),
            "registry.fedoraproject.org/fedora-toolbox:44"
        );
        assert_eq!(
            toolbox_base_image("ultramarine", Family::Fedora, "44"),
            "registry.fedoraproject.org/fedora-toolbox:44"
        );
        assert_eq!(
            toolbox_base_image("rocky", Family::Fedora, "10.1"),
            "quay.io/toolbx-images/rockylinux-toolbox:10"
        );
        assert_eq!(
            toolbox_base_image("almalinux", Family::Fedora, "9.6"),
            "quay.io/toolbx-images/almalinux-toolbox:9"
        );
        assert_eq!(
            toolbox_base_image("centos", Family::Fedora, "10"),
            "quay.io/toolbx-images/centos-toolbox:stream10"
        );
        assert_eq!(
            toolbox_base_image("ubuntu", Family::Debian, "26.04"),
            "quay.io/toolbx/ubuntu-toolbox:26.04"
        );
    }
}
