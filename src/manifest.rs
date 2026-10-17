//! `plexy scan`: a complete, serialisable inventory of the running system.

use crate::desktop::{self, DesktopInfo};
use crate::os::{self, BootInfo, Family, OsInfo};
use crate::pkg::{self, Inventory};
use crate::state::{self, Accounts, SystemSettings, Wrappers};
use crate::util::{self, info, read_opt};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoFile {
    pub path: String,
    pub content: String,
    /// Local key files referenced by the repo definition (gpgkey=file://, signed-by=).
    pub keys: Vec<String>,
    /// Package that ships this file (None = added by hand / `dnf copr` / add-apt-repository).
    #[serde(default)]
    pub owner: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub plexy_version: String,
    pub created: u64,
    pub os: OsInfo,
    pub boot: BootInfo,
    pub desktop: DesktopInfo,
    pub packages: Inventory,
    pub repos: Vec<RepoFile>,
    pub accounts: Accounts,
    pub system: SystemSettings,
    pub wrappers: Wrappers,
}

impl Manifest {
    /// Short tag used to name derived images (hostname, sanitised).
    pub fn hostname_tag(&self) -> String {
        let h = self
            .system
            .hostname
            .clone()
            .unwrap_or_else(|| "host".into());
        let t: String = h
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        if t.is_empty() { "host".into() } else { t }
    }
}

pub fn scan() -> Result<Manifest> {
    let root = Path::new("/");
    info("detecting operating system");
    let os = os::detect_os(root);
    info(format!(
        "{} — family {:?}, package manager {:?}",
        os.pretty_name, os.family, os.pkg_manager
    ));
    let boot = os::detect_boot();
    let desktop = desktop::detect(root);
    info("reading package database (this can take a minute)");
    let mut packages = pkg::scan(&os)?;
    pkg::find_unowned_etc(&mut packages);
    let repos = scan_repos(os.family);
    let accounts = state::scan_accounts(root);
    let system = state::scan_system(root);
    let wrappers = state::scan_wrappers(&accounts);
    Ok(Manifest {
        plexy_version: env!("CARGO_PKG_VERSION").into(),
        created: util::now_unix(),
        os,
        boot,
        desktop,
        packages,
        repos,
        accounts,
        system,
        wrappers,
    })
}

/// Repositories that are not part of the base distribution (third-party,
/// COPR, PPAs, ...) — these must exist in derived images and toolboxes for
/// their packages to be installable.
fn scan_repos(family: Family) -> Vec<RepoFile> {
    let mut out = vec![];
    let dirs: &[&str] = match family {
        Family::Fedora => &["/etc/yum.repos.d"],
        Family::Debian => &["/etc/apt/sources.list.d"],
        Family::Other => &[],
    };
    for d in dirs {
        let Ok(rd) = fs::read_dir(d) else { continue };
        let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            let name = p
                .file_name()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_string();
            if !(name.ends_with(".repo") || name.ends_with(".list") || name.ends_with(".sources")) {
                continue;
            }
            let Some(content) = read_opt(&p) else {
                continue;
            };
            let path = p.display().to_string();
            let owner = match family {
                Family::Fedora => crate::util::try_run("rpm", &["-qf", "--qf", "%{NAME}", &path]),
                Family::Debian => crate::util::try_run("dpkg", &["-S", &path])
                    .and_then(|o| o.split(':').next().map(str::to_string)),
                Family::Other => None,
            }
            .map(|o| o.trim().to_string())
            .filter(|o| !o.is_empty() && !o.contains(' '));
            out.push(RepoFile {
                path,
                keys: referenced_keys(&content),
                content,
                owner,
            });
        }
    }
    out
}

pub fn referenced_keys(content: &str) -> Vec<String> {
    let mut keys = vec![];
    for line in content.lines() {
        let l = line.trim();
        // yum: gpgkey=file:///etc/pki/rpm-gpg/KEY (may list several, space separated)
        if let Some(v) = l
            .strip_prefix("gpgkey")
            .map(|v| v.trim_start_matches([' ', '=']))
        {
            for k in v.split_whitespace() {
                if let Some(p) = k.strip_prefix("file://") {
                    keys.push(p.to_string());
                }
            }
        }
        // deb822: Signed-By: /path ; one-line: [signed-by=/path]
        if let Some(v) = l.strip_prefix("Signed-By:") {
            let v = v.trim();
            if v.starts_with('/') {
                keys.push(v.to_string());
            }
        }
        if let Some(i) = l.find("signed-by=") {
            let rest = &l[i + 10..];
            let end = rest.find([' ', ']']).unwrap_or(rest.len());
            keys.push(rest[..end].to_string());
        }
    }
    keys.sort();
    keys.dedup();
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_key_references() {
        let yum = "[terra]\nname=Terra\ngpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-terra file:///etc/pki/rpm-gpg/other\n";
        assert_eq!(
            referenced_keys(yum),
            vec![
                "/etc/pki/rpm-gpg/RPM-GPG-KEY-terra",
                "/etc/pki/rpm-gpg/other"
            ]
        );
        let deb = "deb [arch=amd64 signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/ubuntu noble stable\n";
        assert_eq!(referenced_keys(deb), vec!["/etc/apt/keyrings/docker.gpg"]);
        let deb822 = "Types: deb\nURIs: https://x\nSigned-By: /usr/share/keyrings/x.gpg\n";
        assert_eq!(referenced_keys(deb822), vec!["/usr/share/keyrings/x.gpg"]);
    }
}
