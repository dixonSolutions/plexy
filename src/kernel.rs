//! Keep the source system's kernel (`--keep-kernel`).
//!
//! A traditional install can run a kernel the target does not ship
//! (linux-surface's `kernel-surface`, a COPR kernel, ...). On an image-based
//! target the kernel is part of the image, so Plexy carries it the way
//! rpm-ostree changes a base image's kernel: an override that removes the
//! target's stock kernel packages and installs the source kernel's packages
//! from the repository they came from. rpm-ostree records the override,
//! replays it at every upgrade and regenerates the initramfs on the machine.
//!
//! Nothing is pinned to versions, so the source kernel, the target's kernel
//! and the two distributions' releases do not have to match:
//! * packages are installed by name: every upgrade takes the newest build;
//! * the stock packages to remove are read from the target's own rpm
//!   database (whatever ships files for its kernel build or requires that
//!   exact build, and whatever else would lose a dependency the source kernel
//!   does not provide) — from the image at plan time, and again on the
//!   migrated system right before the override, so they match the image that
//!   was actually deployed;
//! * the kernel's repository gets fallback URLs for neighbouring releases:
//!   dnf/librepo try a repository's base URLs in order, so a target that is a
//!   release ahead of (or behind) the kernel's publisher keeps resolving the
//!   kernel from the nearest release that has it.

use crate::manifest::RepoFile;
use crate::os::Family;
use crate::pkg::Inventory;
use crate::util::{self, debug, run};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

/// A kernel build as an rpm database sees it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KernelFacts {
    /// Release as `uname -r` prints it.
    pub kver: String,
    /// Package that ships its vmlinuz (None: not from a package).
    pub owner: Option<String>,
    /// Packages tied to this exact build: they ship files in its module
    /// directory or require it by version (kernel, -core, -modules, kmods,
    /// -matched meta packages).
    pub tied: Vec<String>,
}

impl KernelFacts {
    /// Name stem of the kernel's own packages: `kernel-surface` for
    /// `kernel-surface-core`, `kernel` for `kernel-core`.
    pub fn base(&self) -> Option<String> {
        let o = self.owner.as_deref()?;
        Some(o.strip_suffix("-core").unwrap_or(o).to_string())
    }

    /// The tied packages that belong to the kernel itself, and the
    /// out-of-tree modules built for it (kmod-zfs, akmod output, ...).
    pub fn split(&self) -> (Vec<String>, Vec<String>) {
        let Some(base) = self.base() else {
            return (vec![], self.tied.clone());
        };
        self.tied
            .iter()
            .cloned()
            .partition(|p| *p == base || p.starts_with(&format!("{base}-")))
    }
}

/// Another installed package from the kernel's repository.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Companion {
    pub name: String,
    /// Source package name: packages built together are installed together.
    #[serde(default)]
    pub srpm: String,
    /// Ships services, udev rules, modules, firmware, ...: hardware support
    /// that has to run on the host next to the kernel.
    pub host_level: bool,
    /// Packages it replaces (unversioned Conflicts/Obsoletes).
    pub replaces: Vec<String>,
    /// Needs mokutil: it enrolls a Secure Boot key from a scriptlet.
    pub enrolls_key: bool,
}

/// The running kernel of the source system (`plexy scan`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SourceKernel {
    pub facts: KernelFacts,
    /// Repository the kernel's package came from (dnf's `from_repo`).
    pub repo: Option<String>,
    /// The repository is the distribution's own archive.
    pub distro_repo: bool,
    /// The kernel's installed packages, without development files: what
    /// gets installed on the target.
    pub packages: Vec<String>,
    /// Out-of-tree modules built for it: they cannot be carried.
    pub modules: Vec<String>,
    /// Capabilities `packages` provide (minus kernel symbols and
    /// modaliases): which dependents of the target's kernel stay satisfied.
    pub provides: Vec<String>,
    pub companions: Vec<Companion>,
    pub secure_boot: bool,
}

/// How the source kernel is carried to the target (part of the plan).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KernelPlan {
    /// Install the source kernel on the target (`--keep-kernel`, `k` in the review).
    pub keep: bool,
    /// Why it cannot be kept, when it cannot.
    #[serde(default)]
    pub blocked: Option<String>,
    pub source_kver: String,
    /// Package stem of the source kernel (`kernel-surface`).
    pub base: String,
    pub repo: String,
    /// Installed on the target, by name.
    pub packages: Vec<String>,
    pub provides: Vec<String>,
    /// The target's kernel and what the override removes from its base, as
    /// read from the image (recomputed on the system before the override).
    pub target_kver: String,
    pub target_base: String,
    pub remove: Vec<String>,
    /// Removed with the stock kernel without being part of it: modules built
    /// for it and software that needs them — (package, why).
    pub lost: Vec<(String, String)>,
    /// Installed with the kernel from its repository (in the plan as `image`).
    pub companions: Vec<String>,
    /// The companions by source package: what has to be installed together.
    #[serde(default)]
    pub companion_groups: Vec<Vec<String>>,
    /// Packages from the kernel's repository that are not carried — (package, why).
    pub skipped: Vec<(String, String)>,
    /// Out-of-tree modules built for the source kernel (not carried).
    pub source_modules: Vec<String>,
    /// The kernel's repository file and the content written to the target
    /// (with release fallbacks).
    pub repo_file: Option<String>,
    pub repo_content: Option<String>,
    /// What the repository serves for the target's release: name -> version.
    pub resolved: BTreeMap<String, String>,
    pub notes: Vec<String>,
}

/// Where an rpm database is read.
#[derive(Clone, Copy)]
pub enum Root<'a> {
    /// The running system: the source at scan time, the migrated system
    /// when the override is applied.
    Host,
    /// A target image, through podman (no network).
    Image(&'a str),
}

impl Root<'_> {
    fn sh(&self, script: &str, args: &[String]) -> Result<String> {
        let mut a: Vec<String> = vec![];
        let prog = match self {
            Root::Host => "sh",
            Root::Image(img) => {
                for x in ["run", "--rm", "--net=none", "--entrypoint", "/bin/sh", img] {
                    a.push(x.to_string());
                }
                "podman"
            }
        };
        a.extend(["-c".into(), script.into(), "plexy".into()]);
        a.extend(args.iter().cloned());
        run(prog, &a)
    }
}

/// POSIX sh. `$1`: kernel release to examine; default: the one in
/// /usr/lib/modules (images ship exactly one).
const PROBE: &str = r#"
k="$1"
if [ -z "$k" ]; then
  for v in /usr/lib/modules/*/vmlinuz; do [ -e "$v" ] && k=$(basename "$(dirname "$v")"); done
fi
[ -n "$k" ] && command -v rpm >/dev/null 2>&1 || exit 0
printf 'kver\t%s\n' "$k"
rpm -qf --qf 'owner\t%{NAME}\n' "/usr/lib/modules/$k/vmlinuz" 2>/dev/null | grep '^owner' || true
rpm -qa --qf '[%{=NAME}\t%{FILENAMES}\n]' | awk -F'\t' -v m="/modules/$k/" 'index($2, m) { print "mod\t" $1 }' | sort -u
case "$k" in
  *.x86_64|*.aarch64|*.ppc64le|*.s390x|*.riscv64|*.i686|*.armv7hl) vr=${k%.*} ;;
  *) vr=$k ;;
esac
rpm -qa --qf '[req\t%{=NAME}\t%{REQUIRENEVRS}\n]' | grep -F -e "$vr" || true
"#;

/// `rpm -e --test`: which installed packages lose a dependency. The lock
/// goes to /tmp: on an ostree system the rpm database is read-only.
const RPM_E_TEST: &str =
    "rpm -e --test --define '_rpmlock_path /tmp/plexy-rpm.lock' \"$@\" 2>&1; true";

pub fn facts(root: Root, kver: Option<&str>) -> Result<Option<KernelFacts>> {
    let args: Vec<String> = kver.map(|k| vec![k.to_string()]).unwrap_or_default();
    Ok(parse_probe(&root.sh(PROBE, &args)?))
}

pub fn strip_arch(kver: &str) -> &str {
    const ARCHES: &[&str] = &[
        "x86_64", "aarch64", "ppc64le", "s390x", "riscv64", "i686", "armv7hl",
    ];
    match kver.rsplit_once('.') {
        Some((vr, a)) if ARCHES.contains(&a) => vr,
        _ => kver,
    }
}

pub fn parse_probe(out: &str) -> Option<KernelFacts> {
    let mut f = KernelFacts::default();
    let mut tied = BTreeSet::new();
    let mut reqs = vec![];
    for line in out.lines() {
        let mut it = line.splitn(3, '\t');
        match (it.next(), it.next(), it.next()) {
            (Some("kver"), Some(k), _) => f.kver = k.trim().to_string(),
            (Some("owner"), Some(o), _) => f.owner = Some(o.trim().to_string()),
            (Some("mod"), Some(p), _) => {
                tied.insert(p.trim().to_string());
            }
            (Some("req"), Some(p), Some(r)) => reqs.push((p.to_string(), r.to_string())),
            _ => {}
        }
    }
    if f.kver.is_empty() {
        return None;
    }
    if let Some(o) = &f.owner {
        tied.insert(o.clone());
    }
    let vr = strip_arch(&f.kver).to_string();
    loop {
        let before = tied.len();
        for (p, r) in &reqs {
            if !tied.contains(p) && requires_build(r, &vr, &f.kver, &tied) {
                tied.insert(p.clone());
            }
        }
        if tied.len() == before {
            break;
        }
    }
    f.tied = tied.into_iter().collect();
    Some(f)
}

/// `req` (a REQUIRENEVRS entry, possibly a rich dependency) names this exact
/// kernel build: `*-uname-r = <kver>`, or `<tied package> = <version>`.
fn requires_build(req: &str, vr: &str, kver: &str, tied: &BTreeSet<String>) -> bool {
    let toks: Vec<&str> = req
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .filter(|t| !t.is_empty())
        .collect();
    toks.windows(3).any(|w| {
        let v = w[2].rsplit(':').next().unwrap_or(w[2]);
        w[1] == "=" && (v == vr || v == kver) && (w[0].ends_with("-uname-r") || tied.contains(w[0]))
    })
}

/// `rpm -e --test` output: (capability, package name) for every installed
/// package that would lose a dependency.
pub fn parse_needed_by(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|l| {
            let (cap, rest) = l.trim().split_once(" is needed by ")?;
            let nevra = rest.trim().trim_start_matches("(installed)").trim();
            Some((cap.trim().to_string(), nevra_name(nevra).to_string()))
        })
        .collect()
}

/// `name-version-release.src.rpm` -> `name`.
pub fn srpm_name(srpm: &str) -> &str {
    nevra_name(srpm.trim().trim_end_matches(".rpm"))
}

/// `name-version-release.arch` -> `name`.
pub fn nevra_name(nevra: &str) -> &str {
    let mut it = nevra.rsplitn(3, '-');
    let (_, _, n) = (it.next(), it.next(), it.next());
    n.unwrap_or(nevra)
}

/// What removing the target's kernel takes with it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Removal {
    pub remove: Vec<String>,
    /// Removed without being part of the kernel — (package, why).
    pub lost: Vec<(String, String)>,
}

/// Grow `stock` by every package that would lose a dependency the
/// replacement kernel does not provide, until nothing else breaks.
/// `test` runs `rpm -e --test` for a set of packages.
pub fn removal_closure(
    target: &KernelFacts,
    provides: &BTreeSet<String>,
    mut test: impl FnMut(&[String]) -> Result<String>,
) -> Result<Removal> {
    let (_, modules) = target.split();
    let mut lost: Vec<(String, String)> = modules
        .into_iter()
        .map(|m| {
            (
                m,
                format!("module built for the target's kernel {}", target.kver),
            )
        })
        .collect();
    let mut set: BTreeSet<String> = target.tied.iter().cloned().collect();
    for _ in 0..16 {
        let all: Vec<String> = set.iter().cloned().collect();
        let mut added = false;
        for (cap, pkg) in parse_needed_by(&test(&all)?) {
            let name = cap.split_whitespace().next().unwrap_or(&cap);
            if set.contains(&pkg) || provides.contains(name) {
                continue;
            }
            set.insert(pkg.clone());
            lost.push((pkg, format!("needs {cap}")));
            added = true;
        }
        if !added {
            break;
        }
    }
    lost.sort();
    lost.dedup_by(|a, b| a.0 == b.0);
    Ok(Removal {
        remove: set.into_iter().collect(),
        lost,
    })
}

/// The target's stock kernel set on `root`, and what removing it takes along
/// when a kernel providing `provides` replaces it.
pub fn stock_removal(
    root: Root,
    target: &KernelFacts,
    provides: &BTreeSet<String>,
) -> Result<Removal> {
    removal_closure(target, provides, |set| root.sh(RPM_E_TEST, set))
}

/// Installed packages among `names` on `root`.
pub fn installed(root: Root, names: &[String]) -> BTreeSet<String> {
    if names.is_empty() {
        return BTreeSet::new();
    }
    root.sh("rpm -q --qf '%{NAME}\\n' \"$@\" 2>/dev/null; true", names)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| names.iter().any(|n| n == l))
        .map(str::to_string)
        .collect()
}

fn secure_boot() -> bool {
    fs::read("/sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c")
        .map(|b| b.last() == Some(&1))
        .unwrap_or(false)
}

/// Development files and debug builds are not needed to boot the kernel.
fn development(name: &str) -> bool {
    name.ends_with("-devel")
        || name.ends_with("-devel-matched")
        || name.ends_with("-headers")
        || name.contains("-debuginfo")
        || name.ends_with("-debugsource")
}

pub fn repo_file_for<'a>(repos: &'a [RepoFile], id: &str) -> Option<&'a RepoFile> {
    let header = format!("[{id}]");
    repos
        .iter()
        .find(|r| r.content.lines().any(|l| l.trim() == header))
}

/// Package names `repo` publishes, with the system's own repository
/// configuration (empty when it cannot be read, e.g. offline: then only the
/// packages installed from it count).
fn repo_package_names(manager: &str, repo: &str) -> BTreeSet<String> {
    let dnf = if manager == "dnf5" { "dnf5" } else { "dnf" };
    let qf = "%{name}\n";
    util::try_run(
        dnf,
        &["repoquery", "-q", &format!("--repo={repo}"), "--qf", qf],
    )
    .unwrap_or_default()
    .lines()
    .map(|l| l.trim().to_string())
    .filter(|l| !l.is_empty())
    .collect()
}

/// The running kernel on a dnf-managed source (`plexy scan`).
pub fn scan_source(kver: &str, inv: &Inventory, repos: &[RepoFile]) -> Option<SourceKernel> {
    let facts = match facts(Root::Host, Some(kver)) {
        Ok(f) => f?,
        Err(e) => {
            debug(format!("kernel probe failed: {e}"));
            return None;
        }
    };
    let (own, modules) = facts.split();
    let packages: Vec<String> = own.into_iter().filter(|p| !development(p)).collect();
    let provides: Vec<String> = if packages.is_empty() {
        vec![]
    } else {
        let mut v: Vec<String> = Root::Host
            .sh(
                "rpm -q --qf '[%{PROVIDENAME}\\n]' \"$@\" 2>/dev/null; true",
                &packages,
            )
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|p| {
                !p.is_empty()
                    && !p.starts_with("kernel(")
                    && !p.starts_with("modalias(")
                    && !p.starts_with("config(")
                    && !p.starts_with("firmware(")
            })
            .map(str::to_string)
            .collect();
        v.sort();
        v.dedup();
        v
    };
    let repo = facts
        .owner
        .as_ref()
        .and_then(|o| inv.origin.get(o))
        .cloned();
    let distro_repo = match &repo {
        Some(r) => repo_file_for(repos, r)
            .map(|f| crate::plan::is_distro_repo(&f.content))
            .unwrap_or(true),
        None => true,
    };
    // A third-party kernel repository's other packages are its hardware
    // support (linux-surface: iptsd, libwacom-surface, surface-dtx-daemon):
    // the installed packages it publishes, wherever they were installed
    // from (Ultramarine's Terra ships iptsd and libwacom-surface too).
    let mut companions = vec![];
    if let (Some(r), false) = (&repo, distro_repo) {
        let published = repo_package_names(&inv.manager, r);
        let names: Vec<String> = inv
            .installed
            .keys()
            .filter(|n| !facts.tied.contains(*n))
            .filter(|n| inv.origin.get(*n) == Some(r) || published.contains(*n))
            .cloned()
            .collect();
        let replaces = crate::pkg::rpm_replaces(&names);
        let srpms: BTreeMap<String, String> = Root::Host
            .sh(
                "rpm -q --qf '%{NAME}\\t%{SOURCERPM}\\n' \"$@\" 2>/dev/null; true",
                &names,
            )
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(n, s)| (n.to_string(), srpm_name(s).to_string()))
            .collect();
        let key_tools: BTreeSet<String> = Root::Host
            .sh(
                "rpm -q --whatrequires mokutil --qf '%{NAME}\\n' 2>/dev/null; true",
                &[],
            )
            .unwrap_or_default()
            .lines()
            .map(|l| l.trim().to_string())
            .collect();
        for n in names {
            let files = util::try_run("rpm", &["-ql", &n]).unwrap_or_default();
            companions.push(Companion {
                srpm: srpms.get(&n).cloned().unwrap_or_default(),
                host_level: crate::plan::classify_files(&files).host_level,
                replaces: replaces.get(&n).cloned().unwrap_or_default(),
                enrolls_key: key_tools.contains(&n),
                name: n,
            });
        }
    }
    Some(SourceKernel {
        facts,
        repo,
        distro_repo,
        packages,
        modules,
        provides,
        companions,
        secure_boot: secure_boot(),
    })
}

/// Releases tried after the target's own (`$releasever`), newest first:
/// from two past the newer of the two systems down to one before the older.
pub fn fallback_releases(target: &str, source: &str) -> Vec<String> {
    let major = |v: &str| v.split('.').next().and_then(|x| x.parse::<u32>().ok());
    match (major(target), major(source)) {
        (Some(t), Some(s)) => {
            let hi = t.max(s) + 2;
            let lo = t.min(s).saturating_sub(1);
            (lo..=hi).rev().map(|v| v.to_string()).collect()
        }
        (None, Some(s)) => vec![s.to_string()],
        _ => vec![],
    }
}

/// Rewrite section `[repo]` so every `$releasever` base URL is followed by
/// the same URL for each of `releases`. None when the section has no such
/// URL (metalink/mirrorlist only, or a fixed release).
pub fn with_release_fallbacks(content: &str, repo: &str, releases: &[String]) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let mut out: Vec<String> = vec![];
    let mut in_section = false;
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim();
        if t.starts_with('[') && t.ends_with(']') {
            in_section = &t[1..t.len() - 1] == repo;
        } else if in_section
            && let Some(v) = t
                .strip_prefix("baseurl")
                .and_then(|r| r.trim_start().strip_prefix('='))
        {
            let mut urls = split_urls(v);
            i += 1;
            // continuation lines: indented values
            while i < lines.len()
                && lines[i].starts_with([' ', '\t'])
                && !lines[i].trim().is_empty()
                && !lines[i].trim_start().starts_with('#')
            {
                urls.extend(split_urls(lines[i]));
                i += 1;
            }
            let mut all = urls.clone();
            for u in urls.iter().filter(|u| u.contains("$releasever")) {
                for r in releases {
                    let c = u.replace("$releasever_major", r).replace("$releasever", r);
                    if !all.contains(&c) {
                        all.push(c);
                    }
                }
            }
            changed |= all.len() > urls.len();
            if let Some((first, rest)) = all.split_first() {
                out.push(format!("baseurl={first}"));
                out.extend(rest.iter().map(|u| format!("        {u}")));
            }
            continue;
        }
        out.push(lines[i].to_string());
        i += 1;
    }
    changed.then(|| out.join("\n") + "\n")
}

fn split_urls(v: &str) -> Vec<String> {
    v.split(|c: char| c.is_whitespace() || c == ',')
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .collect()
}

/// `dnf repoquery` against `repo` alone (defined by `content`) as a system
/// on `releasever` would see it. Uses the source's dnf.
fn repoquery(
    dnf: &str,
    repo: &str,
    content: &str,
    releasever: &str,
    query: &[&str],
    names: &[String],
) -> Result<String> {
    // disk-backed: /tmp is often a small tmpfs
    let dir =
        std::path::Path::new("/var/tmp").join(format!("plexy-kernel-repo-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    util::write_file(&dir.join("repos/kernel.repo"), content)?;
    let mut args: Vec<String> = vec![
        "repoquery".into(),
        "-q".into(),
        format!("--setopt=reposdir={}", dir.join("repos").display()),
        format!("--setopt=cachedir={}", dir.join("cache").display()),
        format!("--setopt={repo}.repo_gpgcheck=0"),
        format!("--releasever={releasever}"),
        format!("--repo={repo}"),
    ];
    args.extend(query.iter().map(|q| q.to_string()));
    args.extend(names.iter().cloned());
    let out = run(dnf, &args);
    let _ = fs::remove_dir_all(&dir);
    out
}

/// name -> newest version served, for `names`.
pub fn parse_versions(out: &str) -> BTreeMap<String, String> {
    out.lines()
        .filter_map(|l| l.trim().split_once('\t'))
        // repoquery lists versions oldest first: the last one wins
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect()
}

/// Fedora release a build is for (`7.2.11-1.surface.fc45` -> 45).
pub fn fedora_release(evr: &str) -> Option<&str> {
    let i = evr.rfind(".fc")?;
    let rest = &evr[i + 3..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

/// How the source kernel would be carried to `target`. None when the source
/// runs the same kind of kernel the target ships, or nothing can be
/// carried. With `keep`, anything that prevents it is an error.
pub fn plan(
    m: &crate::manifest::Manifest,
    target: &crate::target::ImageFacts,
    keep: bool,
) -> Result<Option<KernelPlan>> {
    let Some(sk) = &m.kernel else {
        if keep {
            bail!(
                "--keep-kernel: no kernel information for {} (a dnf-managed source is needed)",
                m.os.pretty_name
            );
        }
        return Ok(None);
    };
    let refuse = |why: String| -> Result<Option<KernelPlan>> {
        if keep {
            bail!("--keep-kernel: {why}");
        }
        Ok(None)
    };
    let Some(base) = sk.facts.base() else {
        return refuse(format!(
            "the running kernel {} is not from a package; Plexy can only carry packaged kernels",
            sk.facts.kver
        ));
    };
    if m.os.family != Family::Fedora || target.family != Some(Family::Fedora) {
        return refuse("carrying a kernel needs a Fedora-family source and target".into());
    }
    util::info(format!("examining the kernel of {}", target.image));
    let Some(tk) = facts(Root::Image(&target.image), None)? else {
        return refuse(format!("{} ships no packaged kernel", target.image));
    };
    let target_base = tk.base().unwrap_or_default();
    if target_base == base {
        if sk.distro_repo {
            if keep {
                util::info(format!(
                    "the source runs the distribution's {base}; the target's own {base} is kept"
                ));
            }
            return Ok(None);
        }
        return refuse(format!(
            "this system runs a {base} from {} under the same name as the target's kernel; \
             rpm-ostree cannot tell the two apart by name",
            sk.repo.as_deref().unwrap_or("an unknown repository")
        ));
    }

    let mut k = KernelPlan {
        keep,
        source_kver: sk.facts.kver.clone(),
        base: base.clone(),
        repo: sk.repo.clone().unwrap_or_default(),
        packages: sk.packages.clone(),
        provides: sk.provides.clone(),
        target_kver: tk.kver.clone(),
        target_base,
        source_modules: sk.modules.clone(),
        ..Default::default()
    };
    let mut groups: BTreeMap<&String, Vec<String>> = BTreeMap::new();
    for c in &sk.companions {
        let skip = if development(&c.name) {
            Some("development files".to_string())
        } else if c.name.starts_with(&format!("{base}-")) {
            Some(
                "built with the kernel for systems whose package manager writes boot entries; \
                 ostree writes its own"
                    .to_string(),
            )
        } else if c.enrolls_key {
            Some(format!(
                "enrolls a Secure Boot key with mokutil, which cannot run inside an rpm-ostree \
                 transaction; keys enrolled{} stay in the firmware",
                if sk.secure_boot {
                    " on this machine (Secure Boot is on)"
                } else {
                    ""
                }
            ))
        } else if crate::mapping::is_infrastructure(&c.name) {
            Some("repository / release package".to_string())
        } else if !c.host_level && c.replaces.is_empty() {
            None
        } else {
            k.companions.push(c.name.clone());
            groups
                .entry(if c.srpm.is_empty() { &c.name } else { &c.srpm })
                .or_default()
                .push(c.name.clone());
            continue;
        };
        if let Some(why) = skip {
            k.skipped.push((c.name.clone(), why));
        }
    }
    k.companion_groups = groups.into_values().collect();

    let repo_file = sk.repo.as_deref().and_then(|r| repo_file_for(&m.repos, r));
    match (&sk.repo, repo_file) {
        (None, _) => k.blocked = Some(format!("{base} was not installed from a repository")),
        (Some(r), _) if r.starts_with('@') => {
            k.blocked = Some(format!(
                "{base} was installed from a local file ({r}); rpm-ostree can only follow a repository"
            ))
        }
        (Some(r), None) => k.blocked = Some(format!("no repository file defines [{r}]")),
        (Some(r), Some(f)) => {
            k.repo_file = Some(f.path.clone());
            let releases = fallback_releases(&target.os_version, &m.os.version_id);
            let content = match with_release_fallbacks(&f.content, r, &releases) {
                Some(c) => c,
                None => {
                    k.notes.push(format!(
                        "[{r}] has no $releasever base URL; the kernel only resolves while the \
                         repository serves the target's release"
                    ));
                    f.content.clone()
                }
            };
            let dnf = match m.os.pkg_manager {
                crate::os::PkgManager::Dnf5 => "dnf5",
                _ => "dnf",
            };
            // The target's own kernel subpackages, under the source kernel's
            // name (kernel-modules-extra -> kernel-surface-modules-extra):
            // what the target's users rely on stays available.
            let (target_own, _) = tk.split();
            let mirrored: Vec<String> = target_own
                .iter()
                .filter(|p| !development(p))
                .filter_map(|p| p.strip_prefix(k.target_base.as_str()))
                .map(|suffix| format!("{base}{suffix}"))
                .filter(|p| !k.packages.contains(p))
                .collect();
            let mut names = k.packages.clone();
            names.extend(mirrored.iter().cloned());
            names.extend(k.companions.iter().cloned());
            let found = repoquery(
                dnf,
                r,
                &content,
                &target.os_version,
                &["--qf", "%{name}\t%{evr}\n"],
                &names,
            )
            .map(|o| parse_versions(&o));
            let added: Vec<String> = match &found {
                Ok(f) => mirrored.into_iter().filter(|p| f.contains_key(p)).collect(),
                Err(_) => vec![],
            };
            if !added.is_empty() {
                let more = repoquery(
                    dnf,
                    r,
                    &content,
                    &target.os_version,
                    &["--provides"],
                    &added,
                )
                .unwrap_or_default();
                k.provides.extend(
                    more.lines()
                        .filter_map(|l| l.split_whitespace().next())
                        .filter(|p| !p.starts_with("kernel(") && !p.starts_with("modalias("))
                        .map(str::to_string),
                );
                k.provides.sort();
                k.provides.dedup();
                k.notes.push(format!(
                    "also installed, as the target ships its counterpart: {}",
                    added.join(" ")
                ));
                k.packages.extend(added);
            }
            match found {
                Ok(found) => {
                    let missing: Vec<&String> = k
                        .packages
                        .iter()
                        .filter(|p| !found.contains_key(*p))
                        .collect();
                    if !missing.is_empty() {
                        k.blocked = Some(format!(
                            "[{r}] serves no {} for release {} or its fallbacks",
                            missing
                                .iter()
                                .map(|s| s.as_str())
                                .collect::<Vec<_>>()
                                .join(" "),
                            target.os_version
                        ));
                    }
                    for c in &k.companions {
                        if !found.contains_key(c) {
                            k.notes.push(format!(
                                "[{r}] serves no {c} for release {} yet; it is retried at every boot until it does",
                                target.os_version
                            ));
                        }
                    }
                    if let Some(rel) = found.get(&base).and_then(|v| fedora_release(v))
                        && rel != target.os_version.split('.').next().unwrap_or("")
                    {
                        k.notes.push(format!(
                            "[{r}] has no {base} for release {} yet: the release {rel} build is used, \
                             and the target's own release is picked up once it is published",
                            target.os_version
                        ));
                    }
                    k.resolved = found;
                }
                Err(e) => {
                    k.blocked = Some(format!(
                        "could not query [{r}] for release {}: {e}",
                        target.os_version
                    ))
                }
            }
            k.repo_content = Some(content);
        }
    }
    let provides: BTreeSet<String> = k.provides.iter().cloned().collect();
    let removal = stock_removal(Root::Image(&target.image), &tk, &provides)?;
    k.remove = removal.remove;
    k.lost = removal.lost;
    if let Some(b) = &k.blocked {
        if keep {
            bail!("--keep-kernel: {b}");
        }
        k.keep = false;
    }
    Ok(Some(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    // From a Bluefin 44 image (kmods built against its kernel included).
    const BLUEFIN: &str = "kver\t7.2.5-200.fc44.x86_64\nowner\tkernel-core\n\
mod\tkernel-core\nmod\tkernel-modules\nmod\tkernel-modules-core\nmod\tkernel-modules-extra\n\
mod\tkmod-v4l2loopback\nmod\tkmod-zfs\n\
req\tkernel\tkernel-core-uname-r = 7.2.5-200.fc44.x86_64\n\
req\tkernel\t((kernel-modules-extra-uname-r = 7.2.5-200.fc44.x86_64) if kernel-modules-extra-matched)\n\
req\tkernel-devel-matched\tkernel-core = 7.2.5-200.fc44\n\
req\tkernel-tools\tkernel-tools-libs = 7.2.5-200.fc44\n";

    #[test]
    fn tied_packages_from_probe() {
        let f = parse_probe(BLUEFIN).unwrap();
        assert_eq!(f.kver, "7.2.5-200.fc44.x86_64");
        assert_eq!(f.base().as_deref(), Some("kernel"));
        assert_eq!(
            f.tied,
            vec![
                "kernel",
                "kernel-core",
                "kernel-devel-matched",
                "kernel-modules",
                "kernel-modules-core",
                "kernel-modules-extra",
                "kmod-v4l2loopback",
                "kmod-zfs"
            ]
        );
        let (own, mods) = f.split();
        assert!(own.contains(&"kernel-devel-matched".to_string()));
        assert_eq!(mods, vec!["kmod-v4l2loopback", "kmod-zfs"]);
        assert!(parse_probe("").is_none());
    }

    #[test]
    fn surface_kernel_stem() {
        let f = parse_probe(
            "kver\t7.2.11-1.surface.fc44.x86_64\nowner\tkernel-surface-core\n\
             mod\tkernel-surface-core\nmod\tkernel-surface-modules\nmod\takmod-out-of-tree-7.2.11\n\
             req\tkernel-surface\tkernel-surface-core-uname-r = 7.2.11-1.surface.fc44.x86_64\n",
        )
        .unwrap();
        assert_eq!(f.base().as_deref(), Some("kernel-surface"));
        let (own, mods) = f.split();
        assert_eq!(
            own,
            vec![
                "kernel-surface",
                "kernel-surface-core",
                "kernel-surface-modules"
            ]
        );
        assert_eq!(mods, vec!["akmod-out-of-tree-7.2.11"]);
    }

    #[test]
    fn closure_keeps_what_the_new_kernel_provides() {
        let f = parse_probe(BLUEFIN).unwrap();
        let provides: BTreeSet<String> = ["kernel", "kernel-core", "kernel-modules-extra"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut rounds = 0;
        let r = removal_closure(&f, &provides, |set| {
            rounds += 1;
            let mut out = String::from("error: Failed dependencies:\n");
            out.push_str("\tkernel >= 5.6.14 is needed by (installed) virtualbox-guest-additions-7.2.16-1.fc44.x86_64\n");
            out.push_str("\tkernel-modules-extra is needed by (installed) usbip-5.7.9-14.fc44.x86_64\n");
            out.push_str("\tkmod(usbip-core.ko) is needed by (installed) usbip-5.7.9-14.fc44.x86_64\n");
            out.push_str("\tzfs-kmod = 2.4.4 is needed by (installed) zfs-2.4.4-1.fc44.x86_64\n");
            if set.iter().any(|p| p == "zfs") {
                out.push_str("\tzfs = 2.4.4 is needed by (installed) zfs-dracut-2.4.4-1.fc44.noarch\n");
            }
            Ok(out)
        })
        .unwrap();
        assert_eq!(rounds, 3);
        assert!(!r.remove.contains(&"virtualbox-guest-additions".to_string()));
        for p in ["kernel", "kmod-zfs", "zfs", "zfs-dracut", "usbip"] {
            assert!(r.remove.contains(&p.to_string()), "{p}");
        }
        let lost: Vec<&str> = r.lost.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            lost,
            vec![
                "kmod-v4l2loopback",
                "kmod-zfs",
                "usbip",
                "zfs",
                "zfs-dracut"
            ]
        );
        assert!(
            r.lost
                .iter()
                .any(|(p, w)| p == "usbip" && w == "needs kmod(usbip-core.ko)")
        );
    }

    #[test]
    fn nevra_names() {
        assert_eq!(
            nevra_name("virtualbox-guest-additions-7.2.16-1.fc44.x86_64"),
            "virtualbox-guest-additions"
        );
        assert_eq!(nevra_name("zfs-2.4.4-1.fc44.x86_64"), "zfs");
        assert_eq!(strip_arch("7.2.5-200.fc44.x86_64"), "7.2.5-200.fc44");
        assert_eq!(strip_arch("6.1.0-13-amd64"), "6.1.0-13-amd64");
        assert_eq!(fedora_release("7.2.11-1.surface.fc45"), Some("45"));
        assert_eq!(fedora_release("2.20.0-1.fc44"), Some("44"));
        assert_eq!(fedora_release("1.0-1.el10"), None);
    }

    #[test]
    fn release_fallbacks() {
        assert_eq!(fallback_releases("44", "44"), vec!["46", "45", "44", "43"]);
        assert_eq!(
            fallback_releases("43", "44"),
            vec!["46", "45", "44", "43", "42"]
        );
        let repo = "[linux-surface]\nname=linux-surface\nbaseurl=https://pkg.surfacelinux.com/fedora/f$releasever/\n\
                    enabled=1\nskip_if_unavailable=1\n\n[other]\nbaseurl=https://x/$releasever/\n";
        let out =
            with_release_fallbacks(repo, "linux-surface", &["45".into(), "44".into()]).unwrap();
        assert_eq!(
            out,
            "[linux-surface]\nname=linux-surface\nbaseurl=https://pkg.surfacelinux.com/fedora/f$releasever/\n\
             \x20       https://pkg.surfacelinux.com/fedora/f45/\n\
             \x20       https://pkg.surfacelinux.com/fedora/f44/\n\
             enabled=1\nskip_if_unavailable=1\n\n[other]\nbaseurl=https://x/$releasever/\n"
        );
        // already-listed continuation lines are kept, nothing duplicated
        assert_eq!(
            with_release_fallbacks(&out, "linux-surface", &["45".into(), "44".into()]),
            None
        );
        let copr = "[copr:copr.fedorainfracloud.org:bieszczaders:kernel-cachyos]\n\
                    baseurl = https://download.copr.fedorainfracloud.org/results/bieszczaders/kernel-cachyos/fedora-$releasever-$basearch/\n";
        let c = with_release_fallbacks(
            copr,
            "copr:copr.fedorainfracloud.org:bieszczaders:kernel-cachyos",
            &["43".into()],
        )
        .unwrap();
        assert!(c.contains("        https://download.copr.fedorainfracloud.org/results/bieszczaders/kernel-cachyos/fedora-43-$basearch/"));
        let metalink = "[rpmfusion]\nmetalink=https://mirrors.rpmfusion.org/metalink?repo=free-fedora-$releasever\n";
        assert_eq!(
            with_release_fallbacks(metalink, "rpmfusion", &["43".into()]),
            None
        );
    }

    /// Against a real target image: `PLEXY_TEST_IMAGE` (default Bluefin).
    #[test]
    #[ignore]
    fn live_target_removal() {
        let img = std::env::var("PLEXY_TEST_IMAGE")
            .unwrap_or_else(|_| "ghcr.io/ublue-os/bluefin:stable".into());
        let f = facts(Root::Image(&img), None).unwrap().unwrap();
        eprintln!("{f:#?}");
        let provides: BTreeSet<String> = ["kernel", "kernel-core", "kernel-modules-extra"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let r = stock_removal(Root::Image(&img), &f, &provides).unwrap();
        eprintln!("{r:#?}");
        assert!(r.remove.contains(&f.owner.clone().unwrap()));
    }

    /// On a source with a custom kernel installed: `PLEXY_TEST_KVER` names it
    /// (in a container, `uname -r` is the host's).
    #[test]
    #[ignore]
    fn live_source_scan() {
        let os = crate::os::detect_os(std::path::Path::new("/"));
        let inv = crate::pkg::scan(&os).unwrap();
        let kver = std::env::var("PLEXY_TEST_KVER").unwrap();
        let repos = crate::manifest::scan_repos(os.family);
        let sk = scan_source(&kver, &inv, &repos).unwrap();
        println!("{}", serde_json::to_string_pretty(&sk).unwrap());
        assert!(!sk.packages.is_empty());
    }

    /// The whole kernel plan for a scanned source (`PLEXY_TEST_SOURCE`: the
    /// JSON `live_source_scan` prints) against a real image and repository.
    #[test]
    #[ignore]
    fn live_plan() {
        let img = std::env::var("PLEXY_TEST_IMAGE")
            .unwrap_or_else(|_| "ghcr.io/ublue-os/bluefin:stable".into());
        let text = std::fs::read_to_string(std::env::var("PLEXY_TEST_SOURCE").unwrap()).unwrap();
        let json = &text[text.find('{').unwrap()..text.rfind('}').unwrap() + 1];
        let mut m: crate::manifest::Manifest =
            serde_json::from_str(include_str!("../tests/data/manifest-min.json")).unwrap();
        m.kernel = Some(serde_json::from_str(json).unwrap());
        m.os.family = Family::Fedora;
        m.os.version_id =
            std::env::var("PLEXY_TEST_SOURCE_RELEASE").unwrap_or_else(|_| "44".into());
        m.os.pkg_manager = crate::os::PkgManager::Dnf5;
        m.repos = vec![RepoFile {
            path: "/etc/yum.repos.d/linux-surface.repo".into(),
            content: "[linux-surface]\nname=linux-surface\nbaseurl=https://pkg.surfacelinux.com/fedora/f$releasever/\n\
                      enabled=1\nskip_if_unavailable=1\ngpgkey=https://raw.githubusercontent.com/linux-surface/linux-surface/master/pkg/keys/surface.asc\n\
                      gpgcheck=1\nenabled_metadata=1\ntype=rpm-md\nrepo_gpgcheck=0\n"
                .into(),
            keys: vec![],
            owner: None,
        }];
        let mut t = crate::target::ImageFacts {
            image: img,
            family: Some(Family::Fedora),
            os_version: std::env::var("PLEXY_TEST_TARGET_RELEASE").unwrap_or_else(|_| "44".into()),
            ..Default::default()
        };
        t.pretty_name = "test target".into();
        let k = plan(&m, &t, true).unwrap().unwrap();
        let mut shown = k.clone();
        shown.provides = vec![format!("({} capabilities)", k.provides.len())];
        println!("{}", serde_json::to_string_pretty(&shown).unwrap());
        assert!(k.keep && k.blocked.is_none());
    }
}
