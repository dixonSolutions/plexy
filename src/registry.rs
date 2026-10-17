//! The target registry: which image-based OSes Plexy can migrate to, how each
//! one receives updates after the migration, and which conversions are
//! available from which source systems.
//!
//! The registry is data, not code. `registry/targets.toml` is compiled in;
//! files in `/etc/plexy/targets.d/` and `~/.config/plexy/targets.d/` add
//! targets or replace built-in ones by `id`. See the comments at the top of
//! `registry/targets.toml` for the schema.

use crate::desktop::Desktop;
use crate::os::{Family, OsInfo};
use crate::plan::Mode;
use crate::target::Backend;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

const BUILTIN: &str = include_str!("../registry/targets.toml");

/// How packages Plexy layers survive updates of the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Layering {
    /// rpm-ostree client-side layering on top of the upstream image: the
    /// installed system follows the upstream remote directly.
    Client,
    /// A derived image built locally FROM the remote, rebuilt by `plexy update`.
    #[default]
    Image,
    /// ABRoot's /etc/abroot/packages.add, replayed at every `abroot upgrade`.
    Abroot,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Update {
    /// Image the installed system follows; defaults to the target's image.
    #[serde(default)]
    pub remote: Option<String>,
    /// Verify signatures with the image's own policy (ostree-image-signed).
    #[serde(default)]
    pub signed: bool,
    #[serde(default)]
    pub layering: Layering,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    /// A traditional install, migrated with `plexy migrate`.
    Mutable,
    /// Already image-based, moved with `plexy switch`.
    ImageBased,
    #[default]
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Verified,
    Untested,
    Unsupported,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversion {
    /// os-release IDs / VARIANT_IDs, or "family:fedora" / "family:debian".
    pub from: Vec<String>,
    #[serde(default)]
    pub source: SourceKind,
    #[serde(default)]
    pub modes: Vec<Mode>,
    pub status: Status,
    #[serde(default)]
    pub evidence: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub id: String,
    pub name: String,
    pub image: String,
    pub backend: Backend,
    pub family: Family,
    pub desktop: Desktop,
    #[serde(default = "default_priority")]
    pub priority: u32,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub update: Update,
    #[serde(default, rename = "conversion")]
    pub conversions: Vec<Conversion>,
    /// File the entry came from (filled in when loading).
    #[serde(skip)]
    pub origin: String,
}

fn default_priority() -> u32 {
    50
}

impl Target {
    /// Image reference the migrated system follows for updates.
    pub fn remote(&self) -> &str {
        self.update.remote.as_deref().unwrap_or(&self.image)
    }

    /// The conversion entry that applies to `src` (most specific first:
    /// an explicit os ID beats a family wildcard; unsupported beats untested
    /// at the same specificity so refusals cannot be shadowed).
    pub fn conversion_for(&self, src: &OsInfo) -> Option<&Conversion> {
        let kind = if src.image_based {
            SourceKind::ImageBased
        } else {
            SourceKind::Mutable
        };
        let mut best: Option<(u8, &Conversion)> = None;
        for c in &self.conversions {
            if c.source != SourceKind::Any && c.source != kind {
                continue;
            }
            let Some(score) = match_score(&c.from, src) else {
                continue;
            };
            let better = match best {
                None => true,
                Some((s, b)) => {
                    score > s
                        || (score == s && c.status > b.status && c.status == Status::Unsupported)
                }
            };
            if better {
                best = Some((score, c));
            }
        }
        best.map(|(_, c)| c)
    }
}

/// 2 = the source's os ID or variant is listed, 1 = its family is, None = no match.
fn match_score(from: &[String], src: &OsInfo) -> Option<u8> {
    let family = match src.family {
        Family::Fedora => "family:fedora",
        Family::Debian => "family:debian",
        Family::Other => "family:other",
    };
    let mut score = None;
    for f in from {
        if f == &src.id || (!src.variant_id.is_empty() && f == &src.variant_id) {
            return Some(2);
        }
        if f == family || src.id_like.split_whitespace().any(|l| l == f) {
            score = Some(1);
        }
    }
    score
}

#[derive(Debug, Deserialize)]
struct File {
    #[serde(default, rename = "target")]
    targets: Vec<Target>,
}

#[derive(Debug, Clone)]
pub struct Registry {
    pub targets: Vec<Target>,
    pub files: Vec<String>,
}

/// Directories searched for additional registry files, lowest priority first.
pub fn override_dirs() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/etc/plexy/targets.d")];
    let home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    if let Some(h) = home {
        v.push(h.join("plexy/targets.d"));
    }
    v
}

impl Registry {
    pub fn parse(text: &str, origin: &str) -> Result<Vec<Target>> {
        let f: File = toml::from_str(text).with_context(|| format!("parsing {origin}"))?;
        Ok(f.targets
            .into_iter()
            .map(|mut t| {
                t.origin = origin.to_string();
                t
            })
            .collect())
    }

    /// The built-in registry merged with every override directory.
    pub fn load() -> Result<Registry> {
        Self::load_from(&override_dirs())
    }

    pub fn load_from(dirs: &[PathBuf]) -> Result<Registry> {
        let mut r = Registry {
            targets: Self::parse(BUILTIN, "built-in")?,
            files: vec!["built-in".into()],
        };
        for d in dirs {
            let Ok(rd) = fs::read_dir(d) else { continue };
            let mut files: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "toml"))
                .collect();
            files.sort();
            for f in files {
                let text = fs::read_to_string(&f)?;
                let origin = f.display().to_string();
                for t in Self::parse(&text, &origin)? {
                    r.targets.retain(|x| x.id != t.id);
                    r.targets.push(t);
                }
                r.files.push(origin);
            }
        }
        Ok(r)
    }

    pub fn get(&self, id: &str) -> Option<&Target> {
        self.targets.iter().find(|t| t.id == id)
    }

    /// Resolve `--target`: a registry id, or any image reference (in which
    /// case the registry entry whose image matches, if any, is returned too).
    pub fn resolve(&self, name_or_ref: &str) -> (String, Option<&Target>) {
        match self.get(name_or_ref) {
            Some(t) => (t.image.clone(), Some(t)),
            None => (
                name_or_ref.to_string(),
                self.targets.iter().find(|t| t.image == name_or_ref),
            ),
        }
    }

    /// Targets whose expected desktop is `want` and which accept `src`, best
    /// first: verified conversions before untested ones, then by priority.
    pub fn candidates(&self, src: &OsInfo, want: Desktop) -> Vec<&Target> {
        let mut v: Vec<(&Target, Status)> = self
            .targets
            .iter()
            .filter(|t| t.desktop == want)
            .filter_map(|t| t.conversion_for(src).map(|c| (t, c.status)))
            .filter(|(_, s)| *s != Status::Unsupported)
            .collect();
        v.sort_by_key(|(t, s)| (*s, t.priority, t.id.clone()));
        v.into_iter().map(|(t, _)| t).collect()
    }

    /// Every target that accepts `src`, with the matching conversion.
    pub fn conversions_from<'a>(&'a self, src: &OsInfo) -> Vec<(&'a Target, &'a Conversion)> {
        let mut v: Vec<(&Target, &Conversion)> = self
            .targets
            .iter()
            .filter_map(|t| t.conversion_for(src).map(|c| (t, c)))
            .collect();
        v.sort_by_key(|(t, c)| (c.status, t.desktop as u8, t.priority, t.id.clone()));
        v
    }
}

/// A source description from a name given on the command line
/// (`--from ubuntu`), for listing conversions without scanning.
pub fn source_from_name(name: &str, image_based: bool) -> OsInfo {
    let family = Family::from_os_release(name, "");
    let (id, variant) = match name {
        "silverblue" | "kinoite" => ("fedora".to_string(), name.to_string()),
        _ => (name.to_string(), String::new()),
    };
    OsInfo {
        id,
        id_like: String::new(),
        variant_id: variant,
        name: name.into(),
        version_id: String::new(),
        pretty_name: name.into(),
        family: if family == Family::Other && matches!(name, "silverblue" | "kinoite") {
            Family::Fedora
        } else {
            family
        },
        pkg_manager: crate::os::PkgManager::Unknown,
        image_based,
        booted_image: None,
        arch: std::env::consts::ARCH.into(),
        selinux: false,
    }
}

pub fn status_label(s: Status) -> &'static str {
    match s {
        Status::Verified => "verified",
        Status::Untested => "untested",
        Status::Unsupported => "unsupported",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(id: &str, family: Family, image_based: bool) -> OsInfo {
        let mut o = source_from_name(id, image_based);
        o.family = family;
        o
    }

    #[test]
    fn builtin_registry_parses_and_ids_are_unique() {
        let r = Registry::load_from(&[]).unwrap();
        assert!(r.targets.len() >= 10);
        let mut ids: Vec<&str> = r.targets.iter().map(|t| t.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), r.targets.len());
        let b = r.get("bluefin").unwrap();
        assert_eq!(b.remote(), "ghcr.io/ublue-os/bluefin:stable");
        assert!(b.update.signed);
        assert_eq!(b.update.layering, Layering::Client);
        assert_eq!(r.get("vanilla").unwrap().update.layering, Layering::Abroot);
        assert_eq!(
            r.get("fedora-bootc").unwrap().update.layering,
            Layering::Image
        );
    }

    #[test]
    fn conversions_prefer_specific_entries() {
        let r = Registry::load_from(&[]).unwrap();
        let ubuntu = src("ubuntu", Family::Debian, false);
        let c = r.get("bluefin").unwrap().conversion_for(&ubuntu).unwrap();
        assert_eq!(c.status, Status::Verified);
        let mint = src("linuxmint", Family::Debian, false);
        let c = r.get("bluefin").unwrap().conversion_for(&mint).unwrap();
        assert_eq!(c.status, Status::Untested);
        // image-based sources cannot go to ABRoot
        let sb = src("silverblue", Family::Fedora, true);
        assert_eq!(
            r.get("vanilla")
                .unwrap()
                .conversion_for(&sb)
                .unwrap()
                .status,
            Status::Unsupported
        );
        // and the switch from Silverblue to Bluefin is verified
        assert_eq!(
            r.get("bluefin")
                .unwrap()
                .conversion_for(&sb)
                .unwrap()
                .status,
            Status::Verified
        );
    }

    #[test]
    fn candidates_by_desktop_verified_first() {
        let r = Registry::load_from(&[]).unwrap();
        let fedora = src("fedora", Family::Fedora, false);
        let c: Vec<&str> = r
            .candidates(&fedora, Desktop::Gnome)
            .iter()
            .map(|t| t.id.as_str())
            .collect();
        // Fedora -> Silverblue is verified, so it comes before untested Bluefin
        assert_eq!(c[0], "silverblue");
        assert!(c.contains(&"bluefin"));
        let kde: Vec<&str> = r
            .candidates(&fedora, Desktop::Kde)
            .iter()
            .map(|t| t.id.as_str())
            .collect();
        assert!(kde.contains(&"aurora") && !kde.contains(&"bluefin"));
    }

    #[test]
    fn override_files_replace_and_add() {
        let d = std::env::temp_dir().join(format!("plexy-reg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::write(
            d.join("10-mine.toml"),
            r#"
[[target]]
id = "bluefin"
name = "Bluefin (pinned)"
image = "ghcr.io/ublue-os/bluefin:gts"
backend = "bootc"
family = "fedora"
desktop = "gnome"
[target.update]
signed = true
layering = "client"

[[target]]
id = "myimage"
name = "My image"
image = "ghcr.io/me/os:latest"
backend = "bootc"
family = "fedora"
desktop = "gnome"
[target.update]
remote = "ghcr.io/me/os:stable"
layering = "image"
[[target.conversion]]
from = ["fedora"]
modes = ["image"]
status = "untested"
"#,
        )
        .unwrap();
        let r = Registry::load_from(std::slice::from_ref(&d)).unwrap();
        assert_eq!(
            r.get("bluefin").unwrap().image,
            "ghcr.io/ublue-os/bluefin:gts"
        );
        let m = r.get("myimage").unwrap();
        assert_eq!(m.remote(), "ghcr.io/me/os:stable");
        assert!(m.origin.ends_with("10-mine.toml"));
        assert_eq!(r.files.len(), 2);
        let (img, t) = r.resolve("ghcr.io/me/os:latest");
        assert_eq!(img, "ghcr.io/me/os:latest");
        assert_eq!(t.unwrap().id, "myimage");
        fs::remove_dir_all(d).unwrap();
    }
}
