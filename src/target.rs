//! Target images.
//!
//! Well-known targets, their update remotes and available conversions live in
//! the registry (`registry/targets.toml`, see `crate::registry`). Nothing about
//! a target's *contents* is hard-coded: its package set, desktop, accounts and
//! tooling are read from the image itself by `inspect()`, so any bootc or
//! ABRoot image can be used via `--target <ref>`.

use crate::desktop::{self, DesktopInfo};
use crate::os::Family;
use crate::util::{self, debug, info, run, try_run};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// ostree/bootc: `bootc install to-existing-root`, updates via `bootc upgrade`.
    Bootc,
    /// Vanilla OS ABRoot: A/B root partitions + shared var partition.
    Abroot,
}

/// Facts read out of a target image.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageFacts {
    pub image: String,
    pub backend: Option<Backend>,
    pub family: Option<Family>,
    pub os_id: String,
    pub os_version: String,
    pub pretty_name: String,
    pub packages: BTreeSet<String>,
    pub desktop: DesktopInfo,
    /// System accounts shipped by the image (/usr/lib/passwd + /etc/passwd).
    pub passwd: String,
    pub group: String,
    pub has_distrobox: bool,
    pub has_toolbox: bool,
    pub has_flatpak: bool,
    /// Homebrew is bundled (Universal Blue unpacks it on first boot).
    pub has_brew: bool,
    /// rpm-ostree is present: packages can be layered client-side on top of
    /// the upstream image, so the system can follow the upstream remote.
    #[serde(default)]
    pub has_rpm_ostree: bool,
    pub bootc_label: bool,
    pub selinux: bool,
    /// /usr/local is part of the read-only image (fedora-bootc), not a
    /// symlink into /var: anything installed there must be baked into it.
    #[serde(default)]
    pub usr_local_in_usr: bool,
}

pub fn pull(image: &str) -> Result<()> {
    if try_run("podman", &["image", "exists", image]).is_some() {
        debug(format!("{image} already present"));
        return Ok(());
    }
    if image.starts_with("localhost/") {
        bail!("local image {image} not found in container storage");
    }
    util::run_visible("podman", &["pull", image])
}

/// The script runs inside the target image and prints sections that
/// `parse_facts` understands. Plain POSIX sh: images have no bash guarantee.
const PROBE: &str = r#"
sec() { echo "@@@$1"; }
sec os; cat /usr/lib/os-release 2>/dev/null || cat /etc/os-release
sec pkgs
if command -v rpm >/dev/null 2>&1; then rpm -qa --qf '%{NAME}\n'; fi
# read the dpkg database directly: Vanilla OS locks the dpkg/apt binaries (lpkg)
if [ -f /var/lib/dpkg/status ]; then awk '/^Package: /{p=$2} /^Status: install ok installed/{print p}' /var/lib/dpkg/status; fi
sec sessions; ls /usr/share/wayland-sessions /usr/share/xsessions 2>/dev/null
sec dm; readlink /etc/systemd/system/display-manager.service 2>/dev/null; readlink /usr/lib/systemd/system/display-manager.service 2>/dev/null; true
sec passwd; cat /usr/lib/passwd /etc/passwd 2>/dev/null
sec group; cat /usr/lib/group /etc/group 2>/dev/null
sec tools
for t in distrobox toolbox flatpak brew bootc abroot rpm-ostree; do command -v $t >/dev/null 2>&1 && echo $t; done
[ -e /usr/share/homebrew.tar.zst ] && echo brew-bundle
[ -d /home/linuxbrew/.linuxbrew ] && echo brew-bundle
[ -d /usr/share/abroot ] && echo abroot
[ -L /usr/local ] || echo usrlocal-in-usr
[ -e /etc/abroot/abroot.json ] && echo abroot
[ -d /usr/share/selinux/targeted ] || [ -d /etc/selinux/targeted ] && echo selinux
sec end
"#;

pub fn inspect(image: &str) -> Result<ImageFacts> {
    pull(image)?;
    info(format!("inspecting {image}"));
    let out = run(
        "podman",
        &[
            "run",
            "--rm",
            "--net=none",
            "--entrypoint",
            "/bin/sh",
            image,
            "-c",
            PROBE,
        ],
    )
    .with_context(|| format!("probing {image}"))?;
    let labels = try_run(
        "podman",
        &["image", "inspect", "--format", "{{json .Labels}}", image],
    )
    .unwrap_or_default();
    let mut f = parse_facts(&out);
    f.image = image.to_string();
    f.bootc_label =
        labels.contains("\"containers.bootc\":\"1\"") || labels.contains("ostree.bootable");
    f.backend = if f.bootc_label {
        Some(Backend::Bootc)
    } else if out.lines().any(|l| l == "abroot") {
        Some(Backend::Abroot)
    } else {
        None
    };
    Ok(f)
}

pub fn parse_facts(out: &str) -> ImageFacts {
    let mut f = ImageFacts::default();
    let mut sec = "";
    let mut os = String::new();
    let mut sessions = vec![];
    let mut dm = None;
    for line in out.lines() {
        if let Some(s) = line.strip_prefix("@@@") {
            sec = match s {
                "os" => "os",
                "pkgs" => "pkgs",
                "sessions" => "sessions",
                "dm" => "dm",
                "passwd" => "passwd",
                "group" => "group",
                "tools" => "tools",
                _ => "",
            };
            continue;
        }
        match sec {
            "os" => {
                os.push_str(line);
                os.push('\n');
            }
            "pkgs" if !line.trim().is_empty() => {
                f.packages.insert(line.trim().to_string());
            }
            "sessions" => {
                if let Some(s) = line.trim().strip_suffix(".desktop") {
                    sessions.push(s.to_string());
                }
            }
            "dm" if !line.trim().is_empty() && dm.is_none() => {
                dm = line
                    .trim()
                    .rsplit('/')
                    .next()
                    .map(|x| x.trim_end_matches(".service").to_string());
            }
            "passwd" => {
                f.passwd.push_str(line);
                f.passwd.push('\n');
            }
            "group" => {
                f.group.push_str(line);
                f.group.push('\n');
            }
            "tools" => match line.trim() {
                "distrobox" => f.has_distrobox = true,
                "toolbox" => f.has_toolbox = true,
                "flatpak" => f.has_flatpak = true,
                "brew" | "brew-bundle" => f.has_brew = true,
                "rpm-ostree" => f.has_rpm_ostree = true,
                "selinux" => f.selinux = true,
                "usrlocal-in-usr" => f.usr_local_in_usr = true,
                _ => {}
            },
            _ => {}
        }
    }
    let kv = util::parse_kv(&os);
    f.os_id = kv.get("ID").cloned().unwrap_or_default();
    f.os_version = kv.get("VERSION_ID").cloned().unwrap_or_default();
    f.pretty_name = kv.get("PRETTY_NAME").cloned().unwrap_or_default();
    f.family = Some(Family::from_os_release(
        &f.os_id,
        kv.get("ID_LIKE").map(String::as_str).unwrap_or(""),
    ));
    // Re-use the desktop classifier on the session list we collected.
    let mut d = DesktopInfo {
        display_manager: dm,
        ..Default::default()
    };
    for s in &sessions {
        d.sessions.push(s.clone());
        if let Some(x) = desktop::Desktop::from_session(s) {
            d.installed.insert(x);
        }
    }
    d.primary = match d.display_manager.as_deref() {
        Some("gdm") if d.installed.contains(&desktop::Desktop::Gnome) => {
            Some(desktop::Desktop::Gnome)
        }
        Some("sddm") | Some("plasmalogin") if d.installed.contains(&desktop::Desktop::Kde) => {
            Some(desktop::Desktop::Kde)
        }
        _ => d.installed.iter().next().copied(),
    };
    if d.installed.is_empty() {
        d.primary = Some(desktop::Desktop::Headless);
    }
    f.desktop = d;
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_output_parsing() {
        let out = "@@@os\nID=bluefin\nID_LIKE=\"fedora\"\nVERSION_ID=44\n@@@pkgs\nbash\ngnome-shell\n\
                   @@@sessions\ngnome.desktop\ngnome-classic.desktop\n@@@dm\n/usr/lib/systemd/system/gdm.service\n\
                   @@@passwd\nroot:x:0:0:root:/root:/bin/bash\n@@@group\nwheel:x:10:\n@@@tools\ndistrobox\nflatpak\nbrew-bundle\nselinux\n@@@end\n";
        let f = parse_facts(out);
        assert_eq!(f.os_id, "bluefin");
        assert_eq!(f.family, Some(Family::Fedora));
        assert!(f.packages.contains("gnome-shell"));
        assert_eq!(f.desktop.primary, Some(desktop::Desktop::Gnome));
        assert_eq!(f.desktop.display_manager.as_deref(), Some("gdm"));
        assert!(f.has_distrobox && f.has_flatpak && f.has_brew && f.selinux && !f.has_toolbox);
        assert!(f.group.contains("wheel"));
        assert!(!f.usr_local_in_usr);
        let bootc =
            parse_facts("@@@os\nID=fedora\n@@@tools\nusrlocal-in-usr\nrpm-ostree\n@@@end\n");
        assert!(bootc.usr_local_in_usr && bootc.has_rpm_ostree);
    }
}
