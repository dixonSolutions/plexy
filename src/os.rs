//! Detect what we are running on: distribution family, package manager,
//! whether the system is already image-based, and how it boots.

use crate::util::{self, parse_kv, read_opt, try_run};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    /// Fedora, RHEL, CentOS, Ultramarine, Nobara, ... (rpm + dnf)
    Fedora,
    /// Debian, Ubuntu, Mint, Pop!_OS, Vanilla OS, ... (dpkg + apt)
    Debian,
    Other,
}

impl Family {
    pub fn from_os_release(id: &str, id_like: &str) -> Family {
        let words: Vec<&str> = std::iter::once(id)
            .chain(id_like.split_whitespace())
            .collect();
        for w in &words {
            match *w {
                "fedora" | "rhel" | "centos" | "ultramarine" | "nobara" | "almalinux" | "rocky" => {
                    return Family::Fedora;
                }
                "debian" | "ubuntu" | "vanilla" | "linuxmint" | "pop" | "elementary" => {
                    return Family::Debian;
                }
                _ => {}
            }
        }
        Family::Other
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PkgManager {
    Dnf5,
    Dnf4,
    Apt,
    /// Already image-based Fedora (Silverblue, Bluefin, ...): rpm-ostree / bootc.
    RpmOstree,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OsInfo {
    pub id: String,
    pub id_like: String,
    pub name: String,
    pub version_id: String,
    pub pretty_name: String,
    pub family: Family,
    pub pkg_manager: PkgManager,
    /// True when booted from an ostree/bootc deployment.
    pub image_based: bool,
    /// For image-based hosts: the image reference currently booted.
    pub booted_image: Option<String>,
    pub arch: String,
    pub selinux: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BootInfo {
    pub uefi: bool,
    /// Filesystem type of /
    pub root_fstype: String,
    pub root_source: String,
    pub root_uuid: Option<String>,
    /// btrfs subvolume mounted at / (e.g. "root" on Fedora, "@" on Ubuntu installs)
    pub root_subvol: Option<String>,
    /// Separate mounts that hold user data and must stay mounted after migration.
    pub data_mounts: Vec<Mount>,
    pub boot_separate: bool,
    pub esp: Option<String>,
    /// Kernel arguments worth carrying over (filtered /proc/cmdline).
    pub kargs: Vec<String>,
    /// Root lives on LUKS / LVM: affects first-boot unlock and kargs.
    pub root_on_luks: bool,
    pub root_on_lvm: bool,
    pub kernel: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Mount {
    pub target: String,
    pub source: String,
    pub fstype: String,
    pub options: String,
    pub uuid: Option<String>,
    pub subvol: Option<String>,
}

pub fn detect_os(root: &Path) -> OsInfo {
    let rel = read_opt(util::root_join(root, "/etc/os-release"))
        .or_else(|| read_opt(util::root_join(root, "/usr/lib/os-release")))
        .unwrap_or_default();
    let kv = parse_kv(&rel);
    let g = |k: &str| kv.get(k).cloned().unwrap_or_default();
    let family = Family::from_os_release(&g("ID"), &g("ID_LIKE"));
    let image_based = Path::new("/run/ostree-booted").exists() && root == Path::new("/");
    let booted_image = if image_based {
        booted_image_ref()
    } else {
        None
    };
    let pkg_manager = if image_based {
        PkgManager::RpmOstree
    } else if util::have("dnf5") || Path::new("/usr/bin/dnf5").exists() {
        PkgManager::Dnf5
    } else if util::have("dnf") || util::have("yum") {
        PkgManager::Dnf4
    } else if util::have("apt-get") {
        PkgManager::Apt
    } else {
        PkgManager::Unknown
    };
    OsInfo {
        id: g("ID"),
        id_like: g("ID_LIKE"),
        name: g("NAME"),
        version_id: g("VERSION_ID"),
        pretty_name: g("PRETTY_NAME"),
        family,
        pkg_manager,
        image_based,
        booted_image,
        arch: std::env::consts::ARCH.to_string(),
        selinux: Path::new("/sys/fs/selinux/enforce").exists(),
    }
}

fn booted_image_ref() -> Option<String> {
    let js = try_run("bootc", &["status", "--format=json"])?;
    let v: serde_json::Value = serde_json::from_str(&js).ok()?;
    v.pointer("/status/booted/image/image/image")
        .and_then(|x| x.as_str())
        .map(str::to_string)
}

/// Kernel arguments that are distro/installation specific and must NOT be
/// carried into the new deployment (bootc computes its own root= etc).
const KARG_DROP_PREFIXES: &[&str] = &[
    "BOOT_IMAGE=",
    "root=",
    "rootflags=",
    "rootfstype=",
    "ro",
    "rw",
    "initrd=",
    "ostree=",
    "rd.lvm.lv=",
    "resume=",
    "rhgb",
    "quiet",
    "splash",
    "vt.handoff=",
    "rd.luks.uuid=",
    "rd.luks.name=",
    "cryptdevice=",
    "systemd.unit=",
    "console=",
    "no_timer_check",
    "net.ifnames=",
    "biosdevname=",
    "earlyprintk=",
    "$vt_handoff",
    "boot=",
    "crashkernel=",
];

pub fn filter_kargs(cmdline: &str) -> Vec<String> {
    cmdline
        .split_whitespace()
        .filter(|a| {
            !KARG_DROP_PREFIXES.iter().any(|p| {
                if p.ends_with('=') {
                    a.starts_with(p)
                } else {
                    a == p
                }
            })
        })
        .map(str::to_string)
        .collect()
}

pub fn parse_findmnt_json(js: &str) -> Vec<Mount> {
    let mut out = vec![];
    let Ok(v) = serde_json::from_str::<serde_json::Value>(js) else {
        return out;
    };
    fn walk(v: &serde_json::Value, out: &mut Vec<Mount>) {
        if let Some(arr) = v.as_array() {
            for x in arr {
                walk(x, out);
            }
            return;
        }
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        if !s("target").is_empty() {
            let options = s("options");
            let subvol = options
                .split(',')
                .find_map(|o| o.strip_prefix("subvol="))
                .map(|x| x.trim_start_matches('/').to_string());
            out.push(Mount {
                target: s("target"),
                source: s("source"),
                fstype: s("fstype"),
                options,
                uuid: v.get("uuid").and_then(|x| x.as_str()).map(str::to_string),
                subvol,
            });
        }
        if let Some(c) = v.get("children") {
            walk(c, out);
        }
        if let Some(c) = v.get("filesystems") {
            walk(c, out);
        }
    }
    walk(&v, &mut out);
    out
}

/// Mount points under which user data lives and that are separate filesystems
/// (or btrfs subvolumes) from /. These are re-mounted under /var after migration.
const DATA_MOUNT_POINTS: &[&str] = &[
    "/home", "/var", "/srv", "/opt", "/var/lib", "/var/log", "/root",
];

pub fn detect_boot() -> BootInfo {
    let mut b = BootInfo {
        uefi: Path::new("/sys/firmware/efi").exists(),
        kernel: try_run("uname", &["-r"])
            .unwrap_or_default()
            .trim()
            .to_string(),
        ..Default::default()
    };
    let cmdline = read_opt("/proc/cmdline").unwrap_or_default();
    b.kargs = filter_kargs(&cmdline);
    let mounts = try_run(
        "findmnt",
        &["-J", "-o", "TARGET,SOURCE,FSTYPE,OPTIONS,UUID"],
    )
    .map(|j| parse_findmnt_json(&j))
    .unwrap_or_default();
    for m in &mounts {
        match m.target.as_str() {
            "/" => {
                b.root_fstype = m.fstype.clone();
                b.root_source = m.source.clone();
                b.root_uuid = m.uuid.clone();
                b.root_subvol = m.subvol.clone();
            }
            "/boot" => b.boot_separate = true,
            "/boot/efi" | "/efi" => b.esp = Some(m.source.clone()),
            t if DATA_MOUNT_POINTS.contains(&t) => b.data_mounts.push(m.clone()),
            _ => {}
        }
    }
    // `findmnt` prints btrfs sources as /dev/vda3[/root]; strip the subvol suffix.
    if let Some(i) = b.root_source.find('[') {
        b.root_source.truncate(i);
    }
    for m in &mut b.data_mounts {
        if let Some(i) = m.source.find('[') {
            m.source.truncate(i);
        }
    }
    if let Some(tree) = try_run("lsblk", &["-s", "-n", "-o", "TYPE", &b.root_source]) {
        b.root_on_luks = tree.lines().any(|l| l.trim() == "crypt");
        b.root_on_lvm = tree.lines().any(|l| l.trim() == "lvm");
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_detection() {
        assert_eq!(
            Family::from_os_release("ultramarine", "fedora"),
            Family::Fedora
        );
        assert_eq!(Family::from_os_release("ubuntu", "debian"), Family::Debian);
        assert_eq!(
            Family::from_os_release("linuxmint", "ubuntu debian"),
            Family::Debian
        );
        assert_eq!(Family::from_os_release("bluefin", "fedora"), Family::Fedora);
        assert_eq!(Family::from_os_release("arch", ""), Family::Other);
    }

    #[test]
    fn kargs_filtering_keeps_user_args() {
        let k = filter_kargs(
            "BOOT_IMAGE=(hd0,gpt2)/vmlinuz-6.9 root=UUID=abc ro rootflags=subvol=root rhgb quiet \
             nvidia-drm.modeset=1 mitigations=off console=ttyS0",
        );
        assert_eq!(k, vec!["nvidia-drm.modeset=1", "mitigations=off"]);
    }

    #[test]
    fn findmnt_parsing_with_btrfs_subvols() {
        let js = r#"{"filesystems":[{"target":"/","source":"/dev/vda4[/root]","fstype":"btrfs",
          "options":"rw,relatime,seclabel,compress=zstd:1,subvol=/root","uuid":"u1",
          "children":[{"target":"/home","source":"/dev/vda4[/home]","fstype":"btrfs",
          "options":"rw,subvol=/home","uuid":"u1"}]}]}"#;
        let m = parse_findmnt_json(js);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].subvol.as_deref(), Some("root"));
        assert_eq!(m[1].target, "/home");
        assert_eq!(m[1].subvol.as_deref(), Some("home"));
    }
}
