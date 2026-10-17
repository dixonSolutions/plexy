//! Package inventory: what is installed, what the *user* asked for, which
//! repositories it came from, and which package-owned config files were edited.
//!
//! Two backends: rpm (dnf4/dnf5) and dpkg (apt). Image-based Fedora hosts
//! (rpm-ostree / bootc) report their layered packages instead.

use crate::os::{OsInfo, PkgManager};
use crate::util::{debug, run, try_run, warn};
use anyhow::Result;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Inventory {
    pub manager: String,
    /// name -> version of every installed package.
    pub installed: BTreeMap<String, String>,
    /// Packages the user explicitly installed (not pulled in as dependencies).
    pub user_installed: Vec<UserPkg>,
    /// name -> repository id the package was installed from (when known).
    pub origin: BTreeMap<String, String>,
    /// Package-owned config files with local modifications, and unowned /etc files.
    pub config_changes: Vec<ConfigChange>,
    /// Every path owned by a package under /etc (used to classify unowned files).
    #[serde(skip)]
    pub owned_etc: BTreeSet<String>,
    /// For image-based hosts: packages removed from the base image.
    pub removed_from_base: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserPkg {
    pub name: String,
    /// Why we think the user wanted it: "history" (seen on an install command
    /// line), "reason" (package manager marks it user-installed), "layered".
    pub evidence: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum ConfigKind {
    /// Package-owned config file whose content differs from the packaged one.
    Modified,
    /// Package-owned config file that was deleted.
    Deleted,
    /// File under /etc that no package owns (created by the admin or a tool).
    Unowned,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigChange {
    pub path: String,
    pub kind: ConfigKind,
    pub package: Option<String>,
}

pub fn scan(os: &OsInfo) -> Result<Inventory> {
    match os.pkg_manager {
        PkgManager::Dnf5 | PkgManager::Dnf4 => scan_rpm(os.pkg_manager),
        PkgManager::Apt => scan_deb(),
        PkgManager::RpmOstree => scan_rpm_ostree(),
        PkgManager::Unknown => {
            warn(
                "no supported package manager found (dnf/apt/rpm-ostree); packages will not be migrated",
            );
            Ok(Inventory::default())
        }
    }
}

// ---------------------------------------------------------------- rpm / dnf --

fn scan_rpm(pm: PkgManager) -> Result<Inventory> {
    let mut inv = Inventory {
        manager: if pm == PkgManager::Dnf5 {
            "dnf5"
        } else {
            "dnf"
        }
        .into(),
        ..Default::default()
    };
    for line in run("rpm", &["-qa", "--qf", "%{NAME}\t%{VERSION}-%{RELEASE}\n"])?.lines() {
        if let Some((n, v)) = line.split_once('\t')
            && n != "gpg-pubkey"
        {
            inv.installed.insert(n.into(), v.into());
        }
    }
    let dnf = if pm == PkgManager::Dnf5 {
        "dnf5"
    } else {
        "dnf"
    };

    // Repository each package came from.
    if let Some(out) = try_run(
        dnf,
        &[
            "repoquery",
            "--installed",
            "-q",
            "--qf",
            "%{name}\t%{from_repo}\n",
        ],
    ) {
        for line in out.lines() {
            if let Some((n, r)) = line.split_once('\t')
                && !r.is_empty()
            {
                inv.origin.insert(n.into(), r.into());
            }
        }
    }

    // Packages the user asked for: the package manager's "user" install reason,
    // reinforced by names that appear on install command lines in history.
    let reason: BTreeSet<String> = try_run(
        dnf,
        &["repoquery", "--userinstalled", "-q", "--qf", "%{name}\n"],
    )
    .map(|o| {
        o.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    })
    .unwrap_or_default();
    let history = dnf_history_requests(dnf);
    let baseline = install_baseline(&rpm_install_times(), BASELINE_WINDOW);
    inv.user_installed = merge_user_sets(&inv.installed, &reason, &history, &baseline);

    // Config files: compare on-disk digests with the rpm database.
    let qf = "[%{FILEFLAGS}\t%{=FILEDIGESTALGO}\t%{FILEDIGESTS}\t%{=NAME}\t%{FILENAMES}\n]";
    let out = run("rpm", &["-qa", "--qf", qf])?;
    for line in out.lines() {
        let f: Vec<&str> = line.splitn(5, '\t').collect();
        if f.len() != 5 {
            continue;
        }
        let path = f[4];
        if path.starts_with("/etc/") {
            inv.owned_etc.insert(path.to_string());
        }
        let flags: u32 = f[0].parse().unwrap_or(0);
        const RPMFILE_CONFIG: u32 = 1;
        const RPMFILE_GHOST: u32 = 1 << 6;
        if flags & RPMFILE_CONFIG == 0 || flags & RPMFILE_GHOST != 0 || f[2].is_empty() {
            continue;
        }
        let algo: u32 = f[1].parse().unwrap_or(8);
        let p = Path::new(path);
        match fs::symlink_metadata(p) {
            Err(_) => inv.config_changes.push(ConfigChange {
                path: path.into(),
                kind: ConfigKind::Deleted,
                package: Some(f[3].into()),
            }),
            Ok(md) if md.is_file() => {
                let Ok(data) = fs::read(p) else { continue };
                if let Some(d) = digest_hex(algo, &data)
                    && d != f[2]
                {
                    inv.config_changes.push(ConfigChange {
                        path: path.into(),
                        kind: ConfigKind::Modified,
                        package: Some(f[3].into()),
                    });
                }
            }
            Ok(_) => {}
        }
    }
    Ok(inv)
}

/// Hex digest for rpm's PGPHASHALGO ids (1 = MD5, 8 = SHA256, ...).
fn digest_hex(algo: u32, data: &[u8]) -> Option<String> {
    match algo {
        1 => Some(hex(&Md5::digest(data))),
        8 => Some(sha256_hex(data)),
        _ => None,
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Compact SHA-256 (FIPS 180-4) so we do not pull in another dependency.
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bitlen = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [
                t1.wrapping_add(t2),
                v[0],
                v[1],
                v[2],
                v[3].wrapping_add(t1),
                v[4],
                v[5],
                v[6],
            ];
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

/// Package names that appeared as arguments of `install` commands in dnf history.
fn dnf_history_requests(dnf: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    // dnf5 and dnf4 both print the command line in `history list`; parse the
    // arguments of install-like commands, ignoring options and paths.
    let Some(list) = try_run(dnf, &["history", "list"]) else {
        return out;
    };
    for line in list.lines() {
        for (i, w) in line.split_whitespace().enumerate() {
            if (w == "install" || w == "in") && i > 0 {
                for arg in line.split_whitespace().skip(i + 1) {
                    if arg.starts_with('-') || arg == "|" {
                        break;
                    }
                    if arg.contains('/') || arg.ends_with(".rpm") {
                        continue;
                    }
                    out.insert(arg.trim_end_matches(',').to_string());
                }
                break;
            }
        }
    }
    out
}

/// Packages installed together with the operating system: everything whose
/// install time is within `window` seconds of the oldest package.
pub fn install_baseline(times: &BTreeMap<String, u64>, window: u64) -> BTreeSet<String> {
    let Some(first) = times.values().copied().filter(|t| *t > 0).min() else {
        return BTreeSet::new();
    };
    times
        .iter()
        .filter(|(_, t)| **t <= first + window)
        .map(|(n, _)| n.clone())
        .collect()
}

const BASELINE_WINDOW: u64 = 30 * 60;

fn rpm_install_times() -> BTreeMap<String, u64> {
    try_run("rpm", &["-qa", "--qf", "%{NAME}\t%{INSTALLTIME}\n"])
        .map(|o| {
            o.lines()
                .filter_map(|l| {
                    l.split_once('\t')
                        .and_then(|(n, t)| t.parse().ok().map(|t| (n.to_string(), t)))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn deb_install_times(names: impl Iterator<Item = String>) -> BTreeMap<String, u64> {
    let mut m = BTreeMap::new();
    for n in names {
        for p in [
            format!("/var/lib/dpkg/info/{n}.list"),
            format!("/var/lib/dpkg/info/{n}:amd64.list"),
        ] {
            if let Ok(t) = fs::metadata(&p).and_then(|md| md.modified()) {
                let secs = t
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                m.insert(n.clone(), secs);
                break;
            }
        }
    }
    m
}

/// Combine the signals. Install-command history always counts; the package
/// manager's "user" reason only counts for packages installed *after* the OS
/// itself (image builders and installers mark everything as user-installed).
fn merge_user_sets(
    installed: &BTreeMap<String, String>,
    reason: &BTreeSet<String>,
    history: &BTreeSet<String>,
    baseline: &BTreeSet<String>,
) -> Vec<UserPkg> {
    let mut v: Vec<UserPkg> = vec![];
    let mut seen = BTreeSet::new();
    for n in history {
        if installed.contains_key(n) && seen.insert(n.clone()) {
            v.push(UserPkg {
                name: n.clone(),
                evidence: "history".into(),
            });
        }
    }
    for n in reason {
        if baseline.contains(n) {
            continue;
        }
        if installed.contains_key(n) && seen.insert(n.clone()) {
            v.push(UserPkg {
                name: n.clone(),
                evidence: "reason".into(),
            });
        }
    }
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

// ------------------------------------------------------------- dpkg / apt --

fn scan_deb() -> Result<Inventory> {
    let mut inv = Inventory {
        manager: "apt".into(),
        ..Default::default()
    };
    let q = run(
        "dpkg-query",
        &["-W", "-f", "${db:Status-Abbrev}\t${Package}\t${Version}\n"],
    )?;
    for line in q.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() == 3 && f[0].starts_with("ii") {
            inv.installed.insert(f[1].into(), f[2].into());
        }
    }

    let manual: BTreeSet<String> = try_run("apt-mark", &["showmanual"])
        .map(|o| {
            o.lines()
                .map(|l| l.trim().split(':').next().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default();
    // Packages present at install time are the distribution's, not the user's.
    let baseline = deb_install_baseline();
    let manual: BTreeSet<String> = if baseline.is_empty() {
        manual
    } else {
        manual.difference(&baseline).cloned().collect()
    };
    let history = apt_history_requests();
    let times = deb_install_times(inv.installed.keys().cloned());
    let baseline = install_baseline(&times, BASELINE_WINDOW);
    inv.user_installed = merge_user_sets(&inv.installed, &manual, &history, &baseline);

    // Origins from apt-cache policy are expensive; record archive per package lazily
    // only for user packages.
    for p in &inv.user_installed {
        if let Some(pol) = try_run("apt-cache", &["policy", &p.name])
            && let Some(o) = parse_apt_policy_origin(&pol)
        {
            inv.origin.insert(p.name.clone(), o);
        }
    }

    // Conffiles: dpkg records the md5 of the shipped version.
    let cf = run("dpkg-query", &["-W", "-f", "${Package}\n${Conffiles}\n"])?;
    let mut pkg = String::new();
    for line in cf.lines() {
        if !line.starts_with(' ') {
            pkg = line.trim().to_string();
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 2 || f.get(2) == Some(&"obsolete") || f.get(2) == Some(&"remove-on-upgrade") {
            continue;
        }
        let path = f[0];
        inv.owned_etc.insert(path.to_string());
        match fs::read(path) {
            Err(_) => inv.config_changes.push(ConfigChange {
                path: path.into(),
                kind: ConfigKind::Deleted,
                package: Some(pkg.clone()),
            }),
            Ok(data) => {
                if hex(&Md5::digest(&data)) != f[1] {
                    inv.config_changes.push(ConfigChange {
                        path: path.into(),
                        kind: ConfigKind::Modified,
                        package: Some(pkg.clone()),
                    });
                }
            }
        }
    }
    // Every other file dpkg owns under /etc (from the .list files).
    if let Ok(rd) = fs::read_dir("/var/lib/dpkg/info") {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("list")
                && let Ok(t) = fs::read_to_string(&p)
            {
                for l in t.lines().filter(|l| l.starts_with("/etc/")) {
                    inv.owned_etc.insert(l.to_string());
                }
            }
        }
    }
    Ok(inv)
}

/// /var/log/installer/initial-status.gz lists packages present right after the
/// installer ran (Ubuntu/Debian installers write it; cloud images do not).
fn deb_install_baseline() -> BTreeSet<String> {
    let p = "/var/log/installer/initial-status.gz";
    if !Path::new(p).exists() {
        return BTreeSet::new();
    }
    try_run("zcat", &[p])
        .map(|t| {
            t.lines()
                .filter_map(|l| l.strip_prefix("Package: "))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Package names passed to `apt install` / `apt-get install` per
/// /var/log/apt/history.log (+ rotated .gz logs).
fn apt_history_requests() -> BTreeSet<String> {
    let mut text = String::new();
    if let Ok(rd) = fs::read_dir("/var/log/apt") {
        for e in rd.flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|x| x.to_str()).unwrap_or("");
            if !name.starts_with("history.log") {
                continue;
            }
            if name.ends_with(".gz") {
                if let Some(t) = try_run("zcat", &[p.as_os_str()]) {
                    text.push_str(&t);
                }
            } else if let Ok(t) = fs::read_to_string(&p) {
                text.push_str(&t);
            }
        }
    }
    parse_apt_history(&text)
}

pub fn parse_apt_history(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut removed = BTreeSet::new();
    for line in text.lines() {
        let Some(cmd) = line.strip_prefix("Commandline: ") else {
            continue;
        };
        let words: Vec<&str> = cmd.split_whitespace().collect();
        let Some(pos) = words
            .iter()
            .position(|w| matches!(*w, "install" | "remove" | "purge"))
        else {
            continue;
        };
        let installing = words[pos] == "install";
        for w in &words[pos + 1..] {
            if w.starts_with('-') || w.contains('/') || w.ends_with(".deb") {
                continue;
            }
            let n = w.split(['=', ':']).next().unwrap_or(w).to_string();
            if installing {
                out.insert(n.clone());
                removed.remove(&n);
            } else {
                out.remove(&n);
                removed.insert(n);
            }
        }
    }
    out
}

fn parse_apt_policy_origin(policy: &str) -> Option<String> {
    // The line after "*** <installed version>" holds "<prio> <url> <suite>/<component> ...".
    let mut lines = policy.lines();
    while let Some(l) = lines.next() {
        if l.trim_start().starts_with("***") {
            let next = lines.next()?.trim();
            let mut f = next.split_whitespace();
            f.next()?;
            let url = f.next()?;
            if url == "/var/lib/dpkg/status" {
                return Some("local".into());
            }
            return Some(format!("{url} {}", f.next().unwrap_or("")));
        }
    }
    None
}

// -------------------------------------------------------------- rpm-ostree --

fn scan_rpm_ostree() -> Result<Inventory> {
    let mut inv = Inventory {
        manager: "rpm-ostree".into(),
        ..Default::default()
    };
    for line in run("rpm", &["-qa", "--qf", "%{NAME}\t%{VERSION}-%{RELEASE}\n"])?.lines() {
        if let Some((n, v)) = line.split_once('\t') {
            inv.installed.insert(n.into(), v.into());
        }
    }
    if let Some(js) = try_run("rpm-ostree", &["status", "--json", "--booted"]) {
        let v: serde_json::Value = serde_json::from_str(&js)?;
        if let Some(dep) = v.pointer("/deployments/0") {
            let arr = |k: &str| -> Vec<String> {
                dep.get(k)
                    .and_then(|x| x.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            for n in arr("requested-packages")
                .into_iter()
                .chain(arr("requested-local-packages"))
            {
                inv.user_installed.push(UserPkg {
                    name: n,
                    evidence: "layered".into(),
                });
            }
            inv.removed_from_base = arr("requested-base-removals");
        }
    }
    // On ostree, /usr/etc holds the image defaults: diff /etc against it.
    let usr_etc = Path::new("/usr/etc");
    if usr_etc.exists() {
        walk_etc_diff(Path::new("/etc"), usr_etc, &mut inv);
    }
    Ok(inv)
}

fn walk_etc_diff(etc: &Path, usr_etc: &Path, inv: &mut Inventory) {
    let mut stack = vec![etc.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(md) = fs::symlink_metadata(&p) else {
                continue;
            };
            let rel = p.strip_prefix(etc).unwrap();
            let def = usr_etc.join(rel);
            let abs = format!("/etc/{}", rel.display());
            if md.is_dir() {
                stack.push(p);
                continue;
            }
            if def.exists() || fs::symlink_metadata(&def).is_ok() {
                inv.owned_etc.insert(abs.clone());
                let same = if md.file_type().is_symlink() {
                    fs::read_link(&p).ok() == fs::read_link(&def).ok()
                } else {
                    fs::read(&p).ok() == fs::read(&def).ok()
                };
                if !same {
                    inv.config_changes.push(ConfigChange {
                        path: abs,
                        kind: ConfigKind::Modified,
                        package: None,
                    });
                }
            }
        }
    }
}

/// Files under /etc that no package owns. Called after the backend scan so the
/// owned set is populated.
pub fn find_unowned_etc(inv: &mut Inventory) {
    if inv.manager == "rpm-ostree" {
        // walk_etc_diff already compared against /usr/etc; anything not there is local.
        let mut stack = vec![Path::new("/etc").to_path_buf()];
        let mut found = vec![];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                let Ok(md) = fs::symlink_metadata(&p) else {
                    continue;
                };
                if md.is_dir() {
                    stack.push(p);
                    continue;
                }
                let abs = p.display().to_string();
                if !inv.owned_etc.contains(&abs) {
                    found.push(ConfigChange {
                        path: abs,
                        kind: ConfigKind::Unowned,
                        package: None,
                    });
                }
            }
        }
        inv.config_changes.extend(found);
        return;
    }
    let mut stack = vec![Path::new("/etc").to_path_buf()];
    let mut found = vec![];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(md) = fs::symlink_metadata(&p) else {
                continue;
            };
            let abs = p.display().to_string();
            if md.is_dir() {
                stack.push(p);
                continue;
            }
            if !inv.owned_etc.contains(&abs) {
                found.push(ConfigChange {
                    path: abs,
                    kind: ConfigKind::Unowned,
                    package: None,
                });
            }
        }
    }
    debug(format!("{} unowned files under /etc", found.len()));
    inv.config_changes.extend(found);
    inv.config_changes.sort_by(|a, b| a.path.cmp(&b.path));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let long = vec![b'a'; 1000];
        assert_eq!(
            sha256_hex(&long),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    #[test]
    fn apt_history_tracks_install_and_remove() {
        let log = "Start-Date: x\nCommandline: apt install -y htop vim=2:9.1 nginx:amd64\n\
                   Commandline: apt-get remove nginx\nCommandline: apt install ./local.deb tmux\n\
                   Commandline: /usr/bin/unattended-upgrade\n";
        let s = parse_apt_history(log);
        assert_eq!(
            s.into_iter().collect::<Vec<_>>(),
            vec!["htop", "tmux", "vim"]
        );
    }

    #[test]
    fn apt_policy_origin() {
        let p = "htop:\n  Installed: 3.3.0-4\n  Candidate: 3.3.0-4\n  Version table:\n \
                 *** 3.3.0-4 500\n        500 http://archive.ubuntu.com/ubuntu resolute/main amd64 Packages\n";
        assert_eq!(
            parse_apt_policy_origin(p).as_deref(),
            Some("http://archive.ubuntu.com/ubuntu resolute/main")
        );
    }

    #[test]
    fn user_sets_merge_prefers_history_and_drops_uninstalled() {
        let installed: BTreeMap<String, String> = [("htop", "1"), ("vim", "1"), ("bash", "1")]
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        let reason: BTreeSet<String> = ["vim", "bash", "gone"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let hist: BTreeSet<String> = ["htop", "vim"].iter().map(|s| s.to_string()).collect();
        let m = merge_user_sets(&installed, &reason, &hist, &BTreeSet::new());
        let got: Vec<(&str, &str)> = m
            .iter()
            .map(|p| (p.name.as_str(), p.evidence.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![("bash", "reason"), ("htop", "history"), ("vim", "history")]
        );
        // with bash part of the install-time baseline, the "reason" signal is ignored
        let base: BTreeSet<String> = ["bash", "vim"].iter().map(|s| s.to_string()).collect();
        let m = merge_user_sets(&installed, &reason, &hist, &base);
        let got: Vec<&str> = m.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(got, vec!["htop", "vim"]);
    }

    #[test]
    fn baseline_window() {
        let t: BTreeMap<String, u64> = [
            ("a", 1000),
            ("b", 1500),
            ("c", 1000 + 1800),
            ("d", 1000 + 1801),
            ("z", 0),
        ]
        .iter()
        .map(|(n, t)| (n.to_string(), *t))
        .collect();
        let b = install_baseline(&t, 1800);
        assert_eq!(b.into_iter().collect::<Vec<_>>(), vec!["a", "b", "c", "z"]);
    }
}
