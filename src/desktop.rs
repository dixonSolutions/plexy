//! Desktop environment detection.
//!
//! A migration only feels seamless when the desktop is the same before and
//! after: GNOME settings, extensions and dconf keys mean nothing to Plasma and
//! vice versa. Plexy therefore detects the source desktop and *refuses* a
//! target image whose desktop differs, unless the user explicitly overrides.
//! The target's desktop is read from the image itself (its session files),
//! never from a hard-coded table.

use crate::util::{read_opt, root_join};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Desktop {
    Gnome,
    Kde,
    Xfce,
    Cinnamon,
    Mate,
    Budgie,
    Lxqt,
    Cosmic,
    Pantheon,
    Sway,
    Hyprland,
    /// No graphical session installed (server / cloud image).
    Headless,
    Other,
}

impl Desktop {
    pub fn label(self) -> &'static str {
        match self {
            Desktop::Gnome => "GNOME",
            Desktop::Kde => "KDE Plasma",
            Desktop::Xfce => "Xfce",
            Desktop::Cinnamon => "Cinnamon",
            Desktop::Mate => "MATE",
            Desktop::Budgie => "Budgie",
            Desktop::Lxqt => "LXQt",
            Desktop::Cosmic => "COSMIC",
            Desktop::Pantheon => "Pantheon",
            Desktop::Sway => "Sway",
            Desktop::Hyprland => "Hyprland",
            Desktop::Headless => "none (headless)",
            Desktop::Other => "other",
        }
    }

    /// Classify a session file name (without .desktop) from
    /// /usr/share/{wayland-,x}sessions.
    pub fn from_session(name: &str) -> Option<Desktop> {
        let n = name.to_ascii_lowercase();
        let d = if n.starts_with("gnome")
            || n == "ubuntu"
            || n == "ubuntu-wayland"
            || n == "ubuntu-xorg"
        {
            // Ubuntu ships its GNOME session as "ubuntu.desktop".
            Desktop::Gnome
        } else if n.starts_with("plasma") || n.contains("kde") {
            Desktop::Kde
        } else if n.starts_with("xfce") {
            Desktop::Xfce
        } else if n.starts_with("cinnamon") {
            Desktop::Cinnamon
        } else if n.starts_with("mate") {
            Desktop::Mate
        } else if n.starts_with("budgie") {
            Desktop::Budgie
        } else if n.starts_with("lxqt") {
            Desktop::Lxqt
        } else if n.starts_with("cosmic") {
            Desktop::Cosmic
        } else if n.starts_with("pantheon") {
            Desktop::Pantheon
        } else if n.starts_with("sway") {
            Desktop::Sway
        } else if n.starts_with("hyprland") {
            Desktop::Hyprland
        } else if n.contains("classic") || n.contains("custom") || n.contains("default") {
            return None;
        } else {
            Desktop::Other
        };
        Some(d)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DesktopInfo {
    /// Every desktop with an installed session file.
    pub installed: BTreeSet<Desktop>,
    /// The one we consider primary (display-manager default / most complete).
    pub primary: Option<Desktop>,
    pub display_manager: Option<String>,
    pub sessions: Vec<String>,
    /// System-wide GNOME Shell extensions (uuid) installed under /usr/share.
    pub gnome_system_extensions: Vec<String>,
    /// System dconf databases/profiles that carry local settings.
    pub dconf_local: Vec<String>,
}

/// Inspect a filesystem tree (the live root, or an unpacked image) for desktops.
pub fn detect(root: &Path) -> DesktopInfo {
    let mut info = DesktopInfo::default();
    for dir in ["usr/share/wayland-sessions", "usr/share/xsessions"] {
        let Ok(rd) = fs::read_dir(root.join(dir)) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".desktop") else {
                continue;
            };
            info.sessions.push(stem.to_string());
            if let Some(d) = Desktop::from_session(stem) {
                info.installed.insert(d);
            }
        }
    }
    info.sessions.sort();
    info.sessions.dedup();

    // display-manager.service is a symlink to gdm/sddm/lightdm/...
    let dm = fs::read_link(root_join(
        root,
        "/etc/systemd/system/display-manager.service",
    ))
    .ok();
    info.display_manager = dm.and_then(|p| {
        p.file_name()
            .map(|f| f.to_string_lossy().trim_end_matches(".service").to_string())
    });
    if info.display_manager.is_none()
        && let Some(t) = read_opt(root_join(root, "/etc/X11/default-display-manager"))
    {
        info.display_manager = t.trim().rsplit('/').next().map(str::to_string);
    }

    info.primary = match info.display_manager.as_deref() {
        Some("gdm") | Some("gdm3") if info.installed.contains(&Desktop::Gnome) => {
            Some(Desktop::Gnome)
        }
        Some("sddm") | Some("plasmalogin") if info.installed.contains(&Desktop::Kde) => {
            Some(Desktop::Kde)
        }
        Some("cosmic-greeter") if info.installed.contains(&Desktop::Cosmic) => {
            Some(Desktop::Cosmic)
        }
        _ => None,
    }
    .or_else(|| {
        // Prefer the "big" desktops when several are installed.
        [
            Desktop::Gnome,
            Desktop::Kde,
            Desktop::Cosmic,
            Desktop::Cinnamon,
            Desktop::Xfce,
            Desktop::Budgie,
        ]
        .into_iter()
        .find(|d| info.installed.contains(d))
    })
    .or_else(|| info.installed.iter().next().copied());
    if info.installed.is_empty() {
        info.primary = Some(Desktop::Headless);
    }

    if let Ok(rd) = fs::read_dir(root.join("usr/share/gnome-shell/extensions")) {
        info.gnome_system_extensions = rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        info.gnome_system_extensions.sort();
    }
    for p in ["etc/dconf/db", "etc/dconf/profile"] {
        if let Ok(rd) = fs::read_dir(root.join(p)) {
            for e in rd.flatten() {
                let path = e.path();
                // Compiled databases are regenerated by `dconf update`; keep the keyfile dirs.
                if path.is_dir() || p.ends_with("profile") {
                    info.dconf_local
                        .push(format!("/{}", path.strip_prefix(root).unwrap().display()));
                }
            }
        }
    }
    info
}

/// Result of comparing source and target desktops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compat {
    Same,
    /// Source is headless: any target works.
    SourceHeadless,
    /// Target is headless but source is graphical.
    TargetHeadless,
    Different {
        source: Desktop,
        target: Desktop,
    },
}

pub fn compare(source: &DesktopInfo, target: &DesktopInfo) -> Compat {
    let s = source.primary.unwrap_or(Desktop::Headless);
    let t = target.primary.unwrap_or(Desktop::Headless);
    if s == Desktop::Headless {
        return Compat::SourceHeadless;
    }
    if t == Desktop::Headless {
        return Compat::TargetHeadless;
    }
    if s == t || target.installed.contains(&s) {
        Compat::Same
    } else {
        Compat::Different {
            source: s,
            target: t,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(sessions: &[&str], dm: Option<&str>) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("plexy-de-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("usr/share/wayland-sessions")).unwrap();
        fs::create_dir_all(d.join("etc/systemd/system")).unwrap();
        for s in sessions {
            fs::write(
                d.join(format!("usr/share/wayland-sessions/{s}.desktop")),
                "",
            )
            .unwrap();
        }
        if let Some(dm) = dm {
            std::os::unix::fs::symlink(
                format!("/usr/lib/systemd/system/{dm}.service"),
                d.join("etc/systemd/system/display-manager.service"),
            )
            .unwrap();
        }
        d
    }

    #[test]
    fn session_classification() {
        assert_eq!(Desktop::from_session("gnome-wayland"), Some(Desktop::Gnome));
        assert_eq!(Desktop::from_session("ubuntu"), Some(Desktop::Gnome));
        assert_eq!(Desktop::from_session("plasma"), Some(Desktop::Kde));
        assert_eq!(Desktop::from_session("plasmax11"), Some(Desktop::Kde));
        assert_eq!(
            Desktop::from_session("budgie-desktop"),
            Some(Desktop::Budgie)
        );
        assert_eq!(Desktop::from_session("gnome-classic"), Some(Desktop::Gnome));
    }

    #[test]
    fn detection_uses_display_manager() {
        let d = tree(&["gnome", "plasma"], Some("sddm"));
        let i = detect(&d);
        assert_eq!(i.primary, Some(Desktop::Kde));
        assert_eq!(i.display_manager.as_deref(), Some("sddm"));
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn kde_to_gnome_is_refused() {
        let src = tree(&["plasma"], Some("sddm"));
        let dst = tree(&["gnome"], Some("gdm"));
        let c = compare(&detect(&src), &detect(&dst));
        assert_eq!(
            c,
            Compat::Different {
                source: Desktop::Kde,
                target: Desktop::Gnome
            }
        );
        fs::remove_dir_all(src).unwrap();
        fs::remove_dir_all(dst).unwrap();
    }

    #[test]
    fn headless_source_accepts_anything() {
        let src = tree(&[], None);
        let dst = tree(&["gnome"], Some("gdm"));
        assert_eq!(
            compare(&detect(&src), &detect(&dst)),
            Compat::SourceHeadless
        );
        fs::remove_dir_all(src).unwrap();
        fs::remove_dir_all(dst).unwrap();
    }
}
