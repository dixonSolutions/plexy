//! `plexy apply` for the bootc backend: install the image next to the running
//! system, then carry every piece of machine state into the new deployment.
//!
//! Order matters. Everything that can fail is done *before* the destructive
//! data moves, and the data moves (renames on the same filesystem, so they are
//! instant and never copy user data) are recorded in a journal so
//! `plexy rollback-data` can put them back.

use crate::manifest::Manifest;
use crate::os::Family;
use crate::plan::{ConfigAction, Plan, etc_translate};
use crate::state::{GroupEntry, PasswdEntry, parse_group, parse_passwd};
use crate::util::{self, info, root_join, run, step, try_run, warn, write_file};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub struct ApplyOpts {
    pub image: String,
    /// Image reference bootc should follow for updates.
    pub target_imgref: String,
    pub target_transport: Option<String>,
    pub reboot: bool,
    pub allow_esp_wipe: bool,
    pub stateroot: String,
}

/// Everything `plexy rollback` needs to undo the data moves, recorded at apply
/// time with paths as the *old* system saw them.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MoveJournal {
    pub moves: Vec<(String, String)>,
    /// Old /var was kept as a mount and is mounted at /var on the new system.
    #[serde(default)]
    pub var_mounted: bool,
    #[serde(default)]
    pub home_mounted: bool,
    /// Pretty name of the system that was replaced.
    #[serde(default)]
    pub old_os: String,
    /// Ownership changes applied to migrated data (old id -> new id), so a
    /// rollback can restore the old ids.
    #[serde(default)]
    pub uid_remap: BTreeMap<u32, u32>,
    #[serde(default)]
    pub gid_remap: BTreeMap<u32, u32>,
}

/// Where the new system's /var lives, and how the old data reaches it.
#[derive(Debug)]
struct DataLayout {
    /// Directory (seen from the old system) that becomes /var after reboot.
    new_var: PathBuf,
    /// fstab lines to append to the new deployment.
    fstab: Vec<String>,
    /// True when old /var is kept as a mount (nothing to move inside it).
    var_mounted: bool,
    home_mounted: bool,
}

/// `flatpak_apps`: Flatpaks first boot will install.
pub fn preflight(m: &Manifest, opts: &ApplyOpts, flatpak_apps: usize) -> Result<()> {
    if !util::is_root() {
        bail!("plexy apply must run as root");
    }
    if !util::have("podman") {
        bail!(
            "podman is required (install it with your package manager first: dnf install podman / apt install podman)"
        );
    }
    if m.os.image_based {
        bail!("this system is already image-based; use `plexy switch` instead");
    }
    if !m.boot.uefi {
        warn("legacy BIOS boot detected: bootc supports it via bootupd, but UEFI is better tested");
    }
    // bootc removes EFI/ and loader/ from the ESP. Refuse if another OS lives there.
    for esp in ["/boot/efi/EFI", "/efi/EFI", "/boot/EFI"] {
        if let Ok(rd) = fs::read_dir(esp) {
            let others: Vec<String> = rd
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| {
                    matches!(
                        n.as_str(),
                        "Microsoft" | "Windows" | "refind" | "opensuse" | "arch"
                    )
                })
                .collect();
            if !others.is_empty() && !opts.allow_esp_wipe {
                bail!(
                    "the EFI system partition holds other boot loaders ({}). bootc rewrites EFI/ on the ESP, which \
                     would break them. Back them up and re-run with --allow-esp-wipe",
                    others.join(", ")
                );
            }
        }
    }
    if m.boot.root_on_luks && m.os.family == Family::Debian {
        warn(
            "root is on LUKS: Debian-family systems unlock it from crypttab in the initramfs; plexy adds rd.luks.uuid= so the new initramfs can unlock it",
        );
    }
    let archive = fs::metadata("/var/lib/plexy/staging/toolbox.tar").map_or(0, |m| m.len());
    let unpacked = fs::read_to_string(TOOLBOX_SIZE_FILE)
        .ok()
        .and_then(|s| s.trim().parse().ok());
    let toolbox = if archive > 0 {
        toolbox_unpack_room(unpacked, archive)
    } else {
        0
    };
    require_free_root(
        deploy_room(image_size(&opts.image))
            + toolbox
            + flatpak_apps as u64 * FLATPAK_PER_APP
            + FIRST_BOOT_MARGIN,
        "the new system image needs room next to the old one, first boot installs Flatpaks and first login unpacks the toolbox",
    )
}

const GIB: u64 = 1024 * 1024 * 1024;
/// Room `bootc install to-existing-root` needs on / for the new deployment.
pub const MIN_FREE_FOR_IMAGE: u64 = 12 * GIB;
/// Room a toolbox needs per package routed to it: the compressed archive
/// plus what a user's first login unpacks. Measured: 506 Ultramarine
/// packages, 2.0 GB archive, 5.4 GB unpacked: ~15 MB each; 20 covers what
/// the build leaves in staging.
pub const TOOLBOX_PER_PACKAGE: u64 = 20 * 1024 * 1024;
pub const TOOLBOX_MIN_ROOM: u64 = 4 * GIB;
/// Room per Flatpak first boot installs (snap and package replacements):
/// a new app usually brings a runtime. VLC with its KDE runtime ~1.4 GB.
pub const FLATPAK_PER_APP: u64 = 3 * GIB / 2;
/// What the new system itself writes at first boot that plexy does not
/// control: Bluefin's flatpak-preinstall.service put 2.6 GB of apps and
/// runtimes on the Ultramarine guest, journals and caches come on top.
pub const FIRST_BOOT_MARGIN: u64 = 2 * GIB;

/// Room the ostree deployment of an image needs on /. Measured: a 7.4 GB
/// Bluefin image became a 6.8 GB ostree repo, so 1.1x its size leaves some
/// margin. [`MIN_FREE_FOR_IMAGE`] when the image size is not known yet.
pub fn deploy_room(image_bytes: Option<u64>) -> u64 {
    image_bytes.map_or(MIN_FREE_FOR_IMAGE, |b| b * 11 / 10)
}

/// Unpacked size of the toolbox image, recorded by `plexy build` before the
/// image is removed from container storage (the archive is compressed).
pub const TOOLBOX_SIZE_FILE: &str = "/var/lib/plexy/staging/toolbox.size";

/// Room each user's first login needs: the unpacked toolbox image plus 10 %
/// for the container distrobox creates. Without a recorded size, assume 3x
/// the gzip archive (measured 2.7x). Measured: a 5.4 GB image used 7.3 GB,
/// 1.9 GB of which was the /var/tmp copy an oci-archive load makes and a
/// docker-archive load does not.
pub fn toolbox_unpack_room(unpacked: Option<u64>, archive_bytes: u64) -> u64 {
    let u = unpacked.unwrap_or(archive_bytes * 3);
    u + u / 10
}

/// Free space on / needed before building, so a migration that cannot fit
/// fails in seconds instead of after a long image build. `target_bytes` is
/// the size of the target image when it is already in container storage;
/// `toolbox_pkgs` the number of packages going into a toolbox (0: none).
pub fn required_free_before_build(
    target_bytes: Option<u64>,
    toolbox_pkgs: usize,
    flatpak_apps: usize,
) -> u64 {
    let toolbox = match toolbox_pkgs {
        0 => 0,
        n => (n as u64 * TOOLBOX_PER_PACKAGE).max(TOOLBOX_MIN_ROOM),
    };
    deploy_room(target_bytes) + toolbox + flatpak_apps as u64 * FLATPAK_PER_APP + FIRST_BOOT_MARGIN
}

/// Unpacked size of an image in local container storage, if it is there.
pub fn image_size(image: &str) -> Option<u64> {
    try_run(
        "podman",
        &["image", "inspect", "--format", "{{.Size}}", image],
    )
    .and_then(|o| o.trim().parse().ok())
}

pub fn free_on_root() -> u64 {
    try_run("df", &["--output=avail", "-B1", "/"])
        .and_then(|o| o.lines().nth(1).and_then(|l| l.trim().parse::<u64>().ok()))
        .unwrap_or(u64::MAX)
}

pub fn require_free_root(need: u64, why: &str) -> Result<()> {
    let free = free_on_root();
    if free < need {
        bail!(
            "{:.1} GiB free on /, {:.1} GiB needed: {why}. Free space (e.g. `podman image prune`, \
             `dnf clean all` / `apt clean`) and re-run",
            free as f64 / GIB as f64,
            need as f64 / GIB as f64
        );
    }
    Ok(())
}

pub fn run_bootc_install(m: &Manifest, plan: &Plan, opts: &ApplyOpts) -> Result<()> {
    step(format!(
        "installing {} next to the running system (bootc install to-existing-root)",
        opts.image
    ));
    let mut args: Vec<String> = vec![
        "run",
        "--rm",
        // The image is local; no network means no netavark/nftables either
        // (Debian's podman without recommends has no nft binary).
        "--net=none",
        "--privileged",
        "--pid=host",
        "--user=root:root",
        "-v",
        "/dev:/dev",
        "-v",
        "/var/lib/containers:/var/lib/containers",
        "-v",
        "/:/target",
        "--security-opt",
        "label=type:unconfined_t",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    args.push(opts.image.clone());
    args.extend(
        [
            "bootc",
            "install",
            "to-existing-root",
            "--acknowledge-destructive",
            "--skip-fetch-check",
        ]
        .into_iter()
        .map(String::from),
    );
    args.push(format!("--stateroot={}", opts.stateroot));
    args.push(format!("--target-imgref={}", opts.target_imgref));
    if let Some(t) = &opts.target_transport {
        args.push(format!("--target-transport={t}"));
    }
    for k in &plan.kargs {
        args.push(format!("--karg={k}"));
    }
    // Debian-family initramfs-tools unlock root via crypttab; dracut needs a karg.
    if m.boot.root_on_luks
        && m.os.family == Family::Debian
        && let Some(u) = luks_uuid_of_root(&m.boot.root_source)
    {
        args.push(format!("--karg=rd.luks.uuid={u}"));
    }
    util::run_visible("podman", &args)
}

fn luks_uuid_of_root(src: &str) -> Option<String> {
    let out = try_run("lsblk", &["-s", "-n", "-o", "TYPE,UUID", src])?;
    let mut lines = out.lines().peekable();
    while let Some(l) = lines.next() {
        if l.trim_start().starts_with("crypt") {
            // the device below the crypt mapping carries the LUKS UUID
            return lines
                .next()
                .and_then(|n| n.split_whitespace().nth(1))
                .map(str::to_string);
        }
    }
    None
}

pub fn find_deployment(stateroot: &str) -> Result<(PathBuf, PathBuf)> {
    let base = PathBuf::from(format!("/ostree/deploy/{stateroot}"));
    let mut deps: Vec<PathBuf> = fs::read_dir(base.join("deploy"))
        .with_context(|| format!("no deployments under {}", base.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    deps.sort_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok());
    let d = deps.pop().context("no deployment found")?;
    Ok((d, base.join("var")))
}

// ----------------------------------------------------------------- fstab ---

/// Translate an fstab mount target from a traditional layout to the
/// image-based one (/home -> /var/home ...).
pub fn translate_target(t: &str) -> Option<String> {
    let map: &[(&str, &str)] = &[
        ("/home", "/var/home"),
        ("/root", "/var/roothome"),
        ("/opt", "/var/opt"),
        ("/srv", "/var/srv"),
        ("/usr/local", "/var/usrlocal"),
        ("/mnt", "/var/mnt"),
    ];
    for (from, to) in map {
        if t == *from {
            return Some(to.to_string());
        }
        if let Some(rest) = t.strip_prefix(&format!("{from}/")) {
            return Some(format!("{to}/{rest}"));
        }
    }
    match t {
        "/" | "/boot" | "/boot/efi" | "/efi" | "/usr" => None,
        _ => Some(t.to_string()),
    }
}

/// Carry every non-system fstab entry over, with translated targets.
pub fn translate_fstab(old: &str) -> Vec<String> {
    let mut out = vec![];
    for line in old.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 3 {
            continue;
        }
        let (src, tgt, fstype) = (f[0], f[1], f[2]);
        if fstype == "swap" {
            // A swap *file* lived in the old root, which is /sysroot afterwards.
            let src = if src.starts_with('/') && !src.starts_with("/dev/") {
                format!("/sysroot{src}")
            } else {
                src.to_string()
            };
            let mut nf: Vec<String> = f.iter().map(|s| s.to_string()).collect();
            nf[0] = src;
            out.push(nf.join(" "));
            continue;
        }
        let Some(nt) = translate_target(tgt) else {
            continue;
        };
        let mut nf: Vec<String> = f.iter().map(|s| s.to_string()).collect();
        nf[1] = nt;
        // the new root is read-only composefs: never fsck these at boot (pass 0)
        while nf.len() < 6 {
            nf.push("0".into());
        }
        out.push(nf.join(" "));
    }
    out
}

fn plan_data_layout(m: &Manifest, stateroot_var: &Path) -> DataLayout {
    let old_fstab = util::read_opt("/etc/fstab").unwrap_or_default();
    let fstab = translate_fstab(&old_fstab);
    let var_mounted = m.boot.data_mounts.iter().any(|d| d.target == "/var")
        && fstab
            .iter()
            .any(|l| l.split_whitespace().nth(1) == Some("/var"));
    let home_mounted = m.boot.data_mounts.iter().any(|d| d.target == "/home")
        && fstab
            .iter()
            .any(|l| l.split_whitespace().nth(1) == Some("/var/home"));
    DataLayout {
        // When the old /var is a separate filesystem it simply gets mounted at
        // /var again; everything else moves into the stateroot's var.
        new_var: if var_mounted {
            PathBuf::from("/var")
        } else {
            stateroot_var.to_path_buf()
        },
        fstab,
        var_mounted,
        home_mounted,
    }
}

// ------------------------------------------------------------------ moves ---

/// /var entries that belong to the old distribution's package management or
/// are runtime-only; they stay behind in /sysroot.
pub fn var_entry_left_behind(rel: &str) -> bool {
    const DENY: &[&str] = &[
        "lib/dpkg",
        "lib/apt",
        "cache/apt",
        "lib/snapd",
        "snap",
        "cache/snapd",
        "lib/rpm",
        "lib/dnf",
        "cache/dnf",
        "cache/libdnf5",
        "lib/yum",
        "cache/yum",
        "run",
        "lock",
        "lib/ucf",
        "lib/update-notifier",
        "lib/ubuntu-advantage",
        "lib/ubuntu-release-upgrader",
        "lib/cloud",
        "lib/systemd/deb-systemd-helper-enabled",
        "lib/systemd/deb-systemd-user-helper-enabled",
        "lib/os-prober",
        "lib/selinux",
        "lib/sss/mc",
        "tmp",
        "lib/plexy/staging",
        "cache/man",
        "lib/apparmor",
        "cache/apparmor",
        "cache/debconf",
        "lib/abroot",
    ];
    DENY.contains(&rel)
}

/// rename(2), falling back to a reflink copy + delete when source and
/// destination are different mounts (e.g. btrfs subvolumes root vs var).
pub fn move_path(src: &Path, dst: &Path) -> Result<()> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(18) => {
            run(
                "cp",
                &[
                    "-a",
                    "--reflink=auto",
                    &src.display().to_string(),
                    &dst.display().to_string(),
                ],
            )?;
            run(
                "rm",
                &["-rf", "--one-file-system", &src.display().to_string()],
            )?;
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("rename {} -> {}", src.display(), dst.display())),
    }
}

/// Rename `src` into `dst`, merging directories when `dst` already exists.
/// Renames when possible, so user data is normally never copied.
fn merge_move(src: &Path, dst: &Path, journal: &mut MoveJournal) -> Result<()> {
    if fs::symlink_metadata(dst).is_err() {
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }
        if util::dry_run() {
            info(format!("[dry-run] mv {} {}", src.display(), dst.display()));
            return Ok(());
        }
        move_path(src, dst)?;
        journal
            .moves
            .push((src.display().to_string(), dst.display().to_string()));
        return Ok(());
    }
    let sm = fs::symlink_metadata(src)?;
    let dm = fs::symlink_metadata(dst)?;
    if sm.is_dir() && dm.is_dir() && !sm.file_type().is_symlink() {
        for e in fs::read_dir(src)?.flatten() {
            merge_move(&e.path(), &dst.join(e.file_name()), journal)?;
        }
        return Ok(());
    }
    // Conflict: the image created something the user also had. Keep the
    // user's version, park the image default next to it.
    let parked = PathBuf::from(format!("{}.plexy-image-default", dst.display()));
    if !util::dry_run() {
        let _ = fs::rename(dst, &parked);
        move_path(src, dst)?;
        journal
            .moves
            .push((src.display().to_string(), dst.display().to_string()));
    }
    Ok(())
}

/// Where a /var entry goes on a target of another family: Debian keeps mail
/// in /var/mail (with spool/mail a symlink), Fedora the other way round.
pub fn var_rel_for_target(rel: &str, from: Family, to: Family) -> String {
    match (from, to) {
        (Family::Debian, Family::Fedora) => match rel.strip_prefix("mail/") {
            Some(r) => format!("spool/mail/{r}"),
            None => rel.to_string(),
        },
        (Family::Fedora, Family::Debian) => match rel.strip_prefix("spool/mail/") {
            Some(r) => format!("mail/{r}"),
            None => rel.to_string(),
        },
        _ => rel.to_string(),
    }
}

fn move_data(
    layout: &DataLayout,
    journal: &mut MoveJournal,
    from: Family,
    to: Family,
) -> Result<()> {
    let nv = &layout.new_var;
    if !layout.var_mounted {
        step(format!("moving /var contents into {}", nv.display()));
        for top in fs::read_dir("/var")?.flatten() {
            let name = top.file_name().to_string_lossy().into_owned();
            if name == "home" && layout.home_mounted {
                continue;
            }
            let p = top.path();
            if fs::symlink_metadata(&p)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
            {
                continue;
            }
            // Caches belong to the old distribution's tools (ownership, layout);
            // across families they are regenerated rather than carried.
            if name == "cache" && from != to {
                continue;
            }
            if p.is_dir() {
                for sub in fs::read_dir(&p)?.flatten() {
                    let rel = format!("{name}/{}", sub.file_name().to_string_lossy());
                    if var_entry_left_behind(&rel) || var_entry_left_behind(&name) {
                        continue;
                    }
                    // Symlinks in /var are distribution layout (spool/mail ->
                    // ../mail on Debian), not data; the target creates its own.
                    if fs::symlink_metadata(sub.path())
                        .map(|m| m.file_type().is_symlink())
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    if rel == "mail" || rel == "spool/mail" {
                        // move the mailboxes, not the directory, so they land in
                        // the target's real mail spool
                        for mb in fs::read_dir(sub.path())?.flatten() {
                            let mrel = format!("{rel}/{}", mb.file_name().to_string_lossy());
                            merge_move(
                                &mb.path(),
                                &nv.join(var_rel_for_target(&mrel, from, to)),
                                journal,
                            )?;
                        }
                        continue;
                    }
                    merge_move(&sub.path(), &nv.join(&rel), journal)?;
                }
            }
        }
    }
    if !layout.home_mounted {
        let home = Path::new("/home");
        if home.is_dir() && !fs::symlink_metadata(home)?.file_type().is_symlink() {
            step(format!("moving /home into {}/home", nv.display()));
            for e in fs::read_dir(home)?.flatten() {
                merge_move(&e.path(), &nv.join("home").join(e.file_name()), journal)?;
            }
        }
    }
    for (old, new) in [
        ("/root", "roothome"),
        ("/opt", "opt"),
        ("/srv", "srv"),
        ("/usr/local", "usrlocal"),
    ] {
        let p = Path::new(old);
        let Ok(md) = fs::symlink_metadata(p) else {
            continue;
        };
        if md.file_type().is_symlink() || !md.is_dir() {
            continue;
        }
        if layout
            .fstab
            .iter()
            .any(|l| l.split_whitespace().nth(1) == Some(&format!("/var/{new}")))
        {
            continue; // separately mounted, handled by fstab
        }
        for e in fs::read_dir(p)?.flatten() {
            // /usr/local's skeleton dirs belong to the distro; only move content.
            if old == "/usr/local" && dir_is_empty_tree(&e.path()) {
                continue;
            }
            merge_move(&e.path(), &nv.join(new).join(e.file_name()), journal)?;
        }
    }
    Ok(())
}

fn dir_is_empty_tree(p: &Path) -> bool {
    match fs::read_dir(p) {
        Ok(rd) => rd.flatten().all(|e| dir_is_empty_tree(&e.path())),
        Err(_) => false,
    }
}

// --------------------------------------------------------------- accounts ---

/// Group renames between families (Debian admin group "sudo" is Fedora's "wheel").
fn translate_group(g: &str, to: Family) -> String {
    match (g, to) {
        ("sudo" | "admin", Family::Fedora) => "wheel".into(),
        ("wheel", Family::Debian) => "sudo".into(),
        _ => g.into(),
    }
}

pub struct AccountMerge {
    pub passwd: String,
    pub shadow: String,
    pub group: String,
    pub gshadow: String,
    pub subuid: String,
    pub subgid: String,
}

/// Merge the old system's human accounts into the new deployment's /etc
/// account files. `image_*` are the deployment's current files; `lib_group`
/// is /usr/lib/group (nss-altfiles) from the image.
#[allow(clippy::too_many_arguments)]
pub fn merge_accounts(
    m: &Manifest,
    to: Family,
    image_passwd: &str,
    image_shadow: &str,
    image_group: &str,
    image_gshadow: &str,
    lib_passwd: &str,
    lib_group: &str,
    old_shadow: &str,
    old_gshadow: &str,
) -> AccountMerge {
    let mut passwd = image_passwd.trim_end().to_string();
    let mut shadow = image_shadow.trim_end().to_string();
    let existing_users: BTreeSet<String> = parse_passwd(image_passwd)
        .into_iter()
        .chain(parse_passwd(lib_passwd))
        .map(|u| u.name)
        .collect();
    let mut groups: Vec<GroupEntry> = parse_group(image_group);
    let lib_groups = parse_group(lib_group);
    let mut gshadow = image_gshadow.trim_end().to_string();

    let shadow_line = |name: &str, text: &str| {
        text.lines()
            .find(|l| l.split(':').next() == Some(name))
            .map(str::to_string)
    };

    // root's password hash carries over so the admin can still log in.
    if let Some(root) = shadow_line("root", old_shadow) {
        let mut lines: Vec<String> = shadow
            .lines()
            .filter(|l| !l.starts_with("root:"))
            .map(str::to_string)
            .collect();
        lines.insert(0, root);
        shadow = lines.join("\n");
    }

    for h in &m.accounts.humans {
        if existing_users.contains(&h.name) {
            continue;
        }
        let mut e: PasswdEntry = h.clone();
        if e.home.starts_with("/home/") {
            // /home is a symlink to /var/home on every image-based target; keep
            // the familiar path so scripts and configs that hard-code it work.
        }
        if !e.shell.is_empty() && !Path::new(&e.shell).exists() && e.shell.contains("zsh") {
            // shell from the old system; Plexy layers it if it was user-installed.
        }
        passwd.push('\n');
        passwd.push_str(&e.to_line());
        e.shell.clear();
        if let Some(s) = shadow_line(&h.name, old_shadow) {
            shadow.push('\n');
            shadow.push_str(&s);
        }
        // primary group
        if let Some(g) = m.accounts.all_groups.iter().find(|g| g.gid == h.gid)
            && !groups.iter().any(|x| x.name == g.name)
            && !lib_groups.iter().any(|x| x.name == g.name)
        {
            groups.push(GroupEntry {
                name: g.name.clone(),
                gid: g.gid,
                members: vec![],
            });
            if let Some(gs) = shadow_line(&g.name, old_gshadow) {
                gshadow.push('\n');
                gshadow.push_str(&gs);
            } else {
                gshadow.push_str(&format!("\n{}:!::", g.name));
            }
        }
    }

    // supplementary memberships
    for (user, gs) in &m.accounts.memberships {
        for g in gs {
            let name = translate_group(g, to);
            if let Some(entry) = groups.iter_mut().find(|x| x.name == name) {
                if !entry.members.contains(user) {
                    entry.members.push(user.clone());
                }
                continue;
            }
            // Group only in /usr/lib/group (nss-altfiles): copy it to /etc/group,
            // otherwise membership changes are invisible.
            if let Some(lg) = lib_groups.iter().find(|x| x.name == name) {
                groups.push(GroupEntry {
                    name: lg.name.clone(),
                    gid: lg.gid,
                    members: vec![user.clone()],
                });
                continue;
            }
            // Group unknown to the target (docker, lxd, ...): recreate it with the
            // old gid if that gid is free, so file ownership stays meaningful.
            if let Some(old) = m.accounts.all_groups.iter().find(|x| &x.name == g) {
                let taken = groups
                    .iter()
                    .chain(lib_groups.iter())
                    .any(|x| x.gid == old.gid);
                if !taken {
                    groups.push(GroupEntry {
                        name: name.clone(),
                        gid: old.gid,
                        members: vec![user.clone()],
                    });
                    gshadow.push_str(&format!("\n{name}:!::{user}"));
                }
            }
        }
    }
    let group = groups
        .iter()
        .map(|g| g.to_line())
        .collect::<Vec<_>>()
        .join("\n");

    let ids = |file: &str| -> String {
        let t = util::read_opt(file).unwrap_or_default();
        t.lines()
            .filter(|l| {
                m.accounts
                    .humans
                    .iter()
                    .any(|h| l.split(':').next() == Some(h.name.as_str()))
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    AccountMerge {
        passwd: passwd + "\n",
        shadow: shadow + "\n",
        group: group + "\n",
        gshadow: gshadow + "\n",
        subuid: ids("/etc/subuid"),
        subgid: ids("/etc/subgid"),
    }
}

fn write_accounts(m: &Manifest, plan: &Plan, deploy: &Path) -> Result<()> {
    step("merging user accounts");
    let r = |p: &str| util::read_opt(root_join(deploy, p)).unwrap_or_default();
    let merged = merge_accounts(
        m,
        plan.target_family,
        &r("/etc/passwd"),
        &r("/etc/shadow"),
        &r("/etc/group"),
        &r("/etc/gshadow"),
        &r("/usr/lib/passwd"),
        &r("/usr/lib/group"),
        &util::read_opt("/etc/shadow").unwrap_or_default(),
        &util::read_opt("/etc/gshadow").unwrap_or_default(),
    );
    let put = |p: &str, content: &str, mode: u32| -> Result<()> {
        let path = root_join(deploy, p);
        if util::dry_run() {
            info(format!("[dry-run] write {}", path.display()));
            return Ok(());
        }
        write_file(&path, content)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
        Ok(())
    };
    put("/etc/passwd", &merged.passwd, 0o644)?;
    put("/etc/shadow", &merged.shadow, 0o000)?;
    put("/etc/group", &merged.group, 0o644)?;
    put("/etc/gshadow", &merged.gshadow, 0o000)?;
    if !merged.subuid.is_empty() {
        put("/etc/subuid", &(merged.subuid + "\n"), 0o644)?;
        put("/etc/subgid", &(merged.subgid + "\n"), 0o644)?;
    }
    for h in &m.accounts.humans {
        let gs = m
            .accounts
            .memberships
            .get(&h.name)
            .map(|g| g.join(", "))
            .unwrap_or_default();
        info(format!("  user {} (uid {}), groups: {}", h.name, h.uid, gs));
    }
    Ok(())
}

// ----------------------------------------------------------------- config ---

/// A config that is a symlink whose target does not exist in the new root
/// (e.g. Ubuntu's /etc/modprobe.d/blacklist-oss.conf pointing into a package
/// the target lacks). Copying it would leave a dangling link that tools trip on.
pub fn dangling_in_new_root(src: &Path, dst: &Path, root: &Path) -> bool {
    let Ok(target) = fs::read_link(src) else {
        return false;
    };
    let resolved = if target.is_absolute() {
        root_join(root, target.display().to_string())
    } else {
        dst.parent().unwrap_or(root).join(&target)
    };
    fs::symlink_metadata(&resolved).is_err()
}

pub fn copy_preserving(src: &Path, dst: &Path) -> Result<()> {
    let md = fs::symlink_metadata(src)?;
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p)?;
    }
    let _ = fs::remove_file(dst);
    if md.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
    } else {
        fs::copy(src, dst)?;
        fs::set_permissions(dst, fs::Permissions::from_mode(md.mode() & 0o7777))?;
    }
    std::os::unix::fs::lchown(dst, Some(md.uid()), Some(md.gid()))?;
    Ok(())
}

fn write_configs(plan: &Plan, deploy: &Path, staging: &Path) -> Result<Vec<String>> {
    step("carrying configuration into the new /etc");
    let mut written = vec![];
    for c in &plan.configs {
        if !matches!(c.action, ConfigAction::Etc | ConfigAction::Brew) {
            continue;
        }
        let src = Path::new(&c.path);
        if fs::symlink_metadata(src).is_err() {
            continue;
        }
        let dst_path = etc_translate(&c.path, plan.source_family, plan.target_family);
        let dst = root_join(deploy, &dst_path);
        if dangling_in_new_root(src, &dst, deploy) {
            info(format!(
                "  skipping {}: its link target is not in the new system",
                c.path
            ));
            continue;
        }
        if util::dry_run() {
            info(format!("[dry-run] {} → {}", c.path, dst_path));
            continue;
        }
        copy_preserving(src, &dst).with_context(|| format!("copy {}", c.path))?;
        written.push(dst_path.clone());
        if c.action == ConfigAction::Brew {
            let b = staging
                .join("brew-etc")
                .join(c.path.trim_start_matches("/etc/"));
            copy_preserving(src, &b)?;
        }
    }
    // Machine identity and console/locale settings are always carried.
    for p in [
        "/etc/hostname",
        "/etc/machine-id",
        "/etc/vconsole.conf",
        "/etc/locale.conf",
    ] {
        let src = Path::new(p);
        if src.exists() && !written.iter().any(|w| w == p) && !util::dry_run() {
            copy_preserving(src, &root_join(deploy, p))?;
            written.push(p.into());
        }
    }
    if let Ok(tz) = fs::read_link("/etc/localtime") {
        let dst = root_join(deploy, "/etc/localtime");
        let _ = fs::remove_file(&dst);
        if !util::dry_run() {
            std::os::unix::fs::symlink(normalize_zoneinfo(&tz), &dst)?;
        }
    }
    // /etc/shells: union, so both the target's shells and the user's stay valid.
    if let Some(old) = util::read_opt("/etc/shells") {
        let p = root_join(deploy, "/etc/shells");
        let mut cur = util::read_opt(&p).unwrap_or_default();
        for l in old.lines().filter(|l| l.starts_with('/')) {
            if !cur.lines().any(|c| c == l) {
                cur.push_str(l);
                cur.push('\n');
            }
        }
        if !util::dry_run() {
            write_file(&p, cur)?;
        }
    }
    // Debian keeps the locale in /etc/default/locale; Fedora in /etc/locale.conf.
    if plan.source_family == Family::Debian
        && plan.target_family == Family::Fedora
        && let Some(t) = util::read_opt("/etc/default/locale")
        && !util::dry_run()
    {
        write_file(&root_join(deploy, "/etc/locale.conf"), t)?;
    }
    info(format!("  {} config files written", written.len()));
    Ok(written)
}

fn normalize_zoneinfo(p: &Path) -> PathBuf {
    let s = p.display().to_string();
    match s.find("zoneinfo/") {
        Some(i) => PathBuf::from(format!("../usr/share/zoneinfo/{}", &s[i + 9..])),
        None => p.to_path_buf(),
    }
}

// -------------------------------------------------------------- services ---

fn enable_units(plan: &Plan, deploy: &Path) -> Result<Vec<String>> {
    step("re-enabling services");
    let mut enabled = vec![];
    let mut missing = vec![];
    for u in &plan.units_to_enable {
        let exists = ["/usr/lib/systemd/system", "/etc/systemd/system"]
            .iter()
            .any(|d| root_join(deploy, format!("{d}/{u}")).exists());
        if !exists {
            missing.push(u.clone());
            continue;
        }
        let d = deploy.display().to_string();
        if util::dry_run() || try_run("systemctl", &["--root", &d, "enable", u]).is_some() {
            enabled.push(u.clone());
        } else {
            missing.push(u.clone());
        }
    }
    let d = deploy.display().to_string();
    for u in &plan.units_to_disable {
        if !util::dry_run() && try_run("systemctl", &["--root", &d, "disable", u]).is_some() {
            info(format!(
                "  disabled {u} (it was disabled on the old system)"
            ));
        }
    }
    info(format!(
        "  {} units enabled as on the old system",
        enabled.len()
    ));
    if !missing.is_empty() {
        info(format!(
            "  {} units have no counterpart on the target: {}",
            missing.len(),
            missing.join(" ")
        ));
    }
    Ok(enabled)
}

// --------------------------------------------------------------- ownership --

/// Remap file ownership of system accounts whose numeric ids differ between
/// the old and new system (e.g. nginx was 991, is now 985).
pub fn id_remap(old: &[PasswdEntry], new: &[PasswdEntry]) -> BTreeMap<u32, u32> {
    let mut m = BTreeMap::new();
    for o in old.iter().filter(|o| o.uid < 1000 && o.uid != 0) {
        if let Some(n) = new.iter().find(|n| n.name == o.name)
            && n.uid != o.uid
        {
            m.insert(o.uid, n.uid);
        }
    }
    m
}

pub fn gid_remap(old: &[GroupEntry], new: &[GroupEntry]) -> BTreeMap<u32, u32> {
    let mut m = BTreeMap::new();
    for o in old.iter().filter(|o| o.gid < 1000 && o.gid != 0) {
        if let Some(n) = new.iter().find(|n| n.name == o.name)
            && n.gid != o.gid
        {
            m.insert(o.gid, n.gid);
        }
    }
    m
}

pub fn chown_tree(
    root: &Path,
    uids: &BTreeMap<u32, u32>,
    gids: &BTreeMap<u32, u32>,
    skip: &[&str],
) -> usize {
    let mut n = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        if skip.iter().any(|s| p.ends_with(s)) {
            continue;
        }
        let Ok(md) = fs::symlink_metadata(&p) else {
            continue;
        };
        let nu = uids.get(&md.uid()).copied();
        let ng = gids.get(&md.gid()).copied();
        if (nu.is_some() || ng.is_some())
            && !util::dry_run()
            && std::os::unix::fs::lchown(&p, nu, ng).is_ok()
        {
            n += 1;
        }
        if md.is_dir()
            && let Ok(rd) = fs::read_dir(&p)
        {
            stack.extend(rd.flatten().map(|e| e.path()));
        }
    }
    n
}

/// Ids of migrated data are remapped under these trees; container layer
/// stores carry their own id spaces and are never touched.
pub const REMAP_SKIP: &[&str] = &[
    "lib/containers",
    "lib/docker",
    "lib/lxc",
    "lib/lxd",
    "lib/machines",
    "home",
    "lib/flatpak",
    "lib/libvirt/images",
];

type IdMaps = (BTreeMap<u32, u32>, BTreeMap<u32, u32>);

fn remap_ownership(
    m: &Manifest,
    deploy: &Path,
    var_root: &Path,
    extra: &[PathBuf],
) -> Result<IdMaps> {
    let r = |p: &str| util::read_opt(root_join(deploy, p)).unwrap_or_default();
    let new_users: Vec<PasswdEntry> = parse_passwd(&r("/usr/lib/passwd"))
        .into_iter()
        .chain(parse_passwd(&r("/etc/passwd")))
        .collect();
    let new_groups: Vec<GroupEntry> = parse_group(&r("/usr/lib/group"))
        .into_iter()
        .chain(parse_group(&r("/etc/group")))
        .collect();
    let uids = id_remap(&m.accounts.all_users, &new_users);
    let gids = gid_remap(&m.accounts.all_groups, &new_groups);
    if uids.is_empty() && gids.is_empty() {
        return Ok((uids, gids));
    }
    step(format!(
        "remapping ownership of system accounts ({} uids, {} gids differ)",
        uids.len(),
        gids.len()
    ));
    let mut n = chown_tree(var_root, &uids, &gids, REMAP_SKIP);
    n += chown_tree(&deploy.join("etc"), &uids, &gids, &[]);
    for e in extra {
        n += chown_tree(e, &uids, &gids, REMAP_SKIP);
    }
    info(format!("  {n} files re-owned"));
    Ok((uids, gids))
}

// ----------------------------------------------------------------- SELinux --

/// Label what we wrote with the *target's* policy. Runs `setfiles` from the
/// target image with the deployment and var mounted at /target.
fn relabel(plan: &Plan, deploy: &Path, var_dirs: &[(PathBuf, &str)]) -> Result<()> {
    step("labelling migrated files with the target's SELinux policy");
    let mut args: Vec<String> = vec![
        "run",
        "--rm",
        "--privileged",
        "--net=none",
        "--security-opt",
        "label=disable",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    args.push("-v".into());
    args.push(format!("{}/etc:/target/etc", deploy.display()));
    let mut paths = vec!["/target/etc".to_string()];
    for (host, inner) in var_dirs {
        // non-recursive: never descend into nested mounts (vfat ESP in /boot)
        args.push("--mount".into());
        args.push(format!(
            "type=bind,source={},destination=/target{},bind-nonrecursive",
            host.display(),
            inner
        ));
        paths.push(format!("/target{inner}"));
    }
    // Run setfiles from the new deployment's own tree: the image may no longer
    // be in container storage (/var/lib/containers just moved with /var).
    let _ = &plan.target_image;
    args.push("--rootfs".into());
    // :O = overlay, so mount points are not created inside the deployment
    args.push(format!("{}:O", deploy.display()));
    args.push("/usr/sbin/setfiles".into());
    args.extend(
        [
            "-F",
            "-r",
            "/target",
            "/etc/selinux/targeted/contexts/files/file_contexts",
        ]
        .map(String::from),
    );
    args.extend(paths);
    match util::run_visible("podman", &args) {
        Ok(()) => Ok(()),
        Err(e) => {
            warn(format!(
                "relabel failed ({e}); the first boot will relabel instead"
            ));
            Ok(())
        }
    }
}

// ----------------------------------------------------- first boot / login ---

pub const FIRSTBOOT_UNIT: &str = "[Unit]
Description=Plexy: finish migration on first boot
ConditionPathExists=!/var/lib/plexy/firstboot.done
After=local-fs.target network-online.target
Wants=network-online.target
Before=systemd-user-sessions.service

[Service]
Type=oneshot
ExecStart=/usr/local/bin/plexy firstboot
RemainAfterExit=yes
TimeoutStartSec=0

[Install]
WantedBy=multi-user.target
";

pub const FIRSTLOGIN_UNIT: &str = "[Unit]
Description=Plexy: restore your toolbox, Homebrew tools and apps
ConditionPathExists=!%h/.local/state/plexy/firstlogin.done
ConditionUser=!@system
After=graphical-session.target

[Service]
Type=oneshot
ExecStart=/usr/local/bin/plexy firstlogin
RemainAfterExit=yes
TimeoutStartSec=0

[Install]
WantedBy=default.target
";

fn install_hooks(plan: &Plan, deploy: &Path, new_var: &Path, staging: &Path) -> Result<()> {
    step("installing first-boot and first-login hooks");
    if util::dry_run() {
        return Ok(());
    }
    // /proc/self/exe stays valid even after /usr/local (and this binary) moved.
    install_self(&new_var.join("usrlocal/bin/plexy"))?;
    write_file(
        &root_join(deploy, "/etc/systemd/system/plexy-firstboot.service"),
        FIRSTBOOT_UNIT,
    )?;
    write_file(
        &root_join(deploy, "/etc/systemd/user/plexy-firstlogin.service"),
        FIRSTLOGIN_UNIT,
    )?;
    let wants = root_join(
        deploy,
        "/etc/systemd/system/multi-user.target.wants/plexy-firstboot.service",
    );
    fs::create_dir_all(wants.parent().unwrap())?;
    let _ = fs::remove_file(&wants);
    std::os::unix::fs::symlink("/etc/systemd/system/plexy-firstboot.service", &wants)?;
    let uwants = root_join(
        deploy,
        "/etc/systemd/user/default.target.wants/plexy-firstlogin.service",
    );
    fs::create_dir_all(uwants.parent().unwrap())?;
    let _ = fs::remove_file(&uwants);
    std::os::unix::fs::symlink("/etc/systemd/user/plexy-firstlogin.service", &uwants)?;
    // The plan travels with the system (and is what firstboot/firstlogin read).
    let state = new_var.join("lib/plexy");
    fs::create_dir_all(&state)?;
    write_file(
        &state.join("plan.json"),
        serde_json::to_string_pretty(plan)?,
    )?;
    // Files staged during apply (toolbox archive, brew etc) move with /var.
    if staging.exists() && staging != state.join("staging") {
        let _ = fs::rename(staging, state.join("staging"));
    }
    Ok(())
}

// -------------------------------------------------------------------- run ---

pub fn apply_bootc(m: &Manifest, plan: &Plan, opts: &ApplyOpts, staging: &Path) -> Result<()> {
    preflight(m, opts, plan.flatpaks_to_install.len())?;
    backup_boot()?;
    run_bootc_install(m, plan, opts)?;
    let (deploy, stateroot_var) = if util::dry_run() {
        (
            PathBuf::from("/ostree/deploy/default/deploy/DRYRUN.0"),
            PathBuf::from("/ostree/deploy/default/var"),
        )
    } else {
        find_deployment(&opts.stateroot)?
    };
    info(format!("new deployment: {}", deploy.display()));
    let layout = plan_data_layout(m, &stateroot_var);

    // fstab: bootc writes /boot only when the old entry was UUID-based
    // (Ubuntu uses LABEL=BOOT); without it `bootc upgrade` cannot write /boot.
    if !util::dry_run() {
        let p = root_join(&deploy, "/etc/fstab");
        let mut cur = util::read_opt(&p).unwrap_or_default();
        let has_boot = cur
            .lines()
            .any(|l| l.split_whitespace().nth(1) == Some("/boot"));
        if let (true, false, Some(line)) = (m.boot.boot_separate, has_boot, boot_fstab_line()) {
            info(format!("adding {line}"));
            cur.push_str(&line);
            cur.push('\n');
        }
        write_file(&p, &cur)?;
    }
    if !layout.fstab.is_empty() && !util::dry_run() {
        let p = root_join(&deploy, "/etc/fstab");
        let mut cur = util::read_opt(&p).unwrap_or_default();
        for l in &layout.fstab {
            if !cur
                .lines()
                .any(|c| c.split_whitespace().nth(1) == l.split_whitespace().nth(1))
            {
                cur.push_str(l);
                cur.push('\n');
            }
        }
        write_file(&p, cur)?;
    }
    info(format!("fstab entries carried: {}", layout.fstab.len()));

    write_accounts(m, plan, &deploy)?;
    write_configs(plan, &deploy, staging)?;
    enable_units(plan, &deploy)?;

    // --- point of no return for the old system: move data ------------------
    let mut journal = MoveJournal {
        var_mounted: layout.var_mounted,
        home_mounted: layout.home_mounted,
        old_os: m.os.pretty_name.clone(),
        ..Default::default()
    };
    move_data(
        &layout,
        &mut journal,
        plan.source_family,
        plan.target_family,
    )?;
    install_hooks(plan, &deploy, &layout.new_var, staging)?;

    let mut extra = vec![];
    if layout.var_mounted {
        extra.push(PathBuf::from("/var"));
    }
    let (uids, gids) = remap_ownership(m, &deploy, &stateroot_var, &extra)?;
    journal.uid_remap = uids;
    journal.gid_remap = gids;
    if !util::dry_run() {
        write_file(
            &layout.new_var.join("lib/plexy/moves.json"),
            serde_json::to_string_pretty(&journal)?,
        )?;
    }

    // SELinux: label /etc always; label /var + home when the source did not
    // use the same policy (cross-family, or SELinux disabled on the source).
    let mut var_dirs: Vec<(PathBuf, &str)> = vec![];
    if !m.os.selinux || plan.source_family != plan.target_family {
        var_dirs.push((layout.new_var.clone(), "/var"));
        // bootc/bootupd wrote /boot from a host without SELinux: unlabelled.
        if m.boot.boot_separate {
            var_dirs.push((PathBuf::from("/boot"), "/boot"));
        }
        if layout.home_mounted {
            var_dirs.push((PathBuf::from("/home"), "/var/home"));
        }
    } else {
        // even on Fedora→Fedora, roothome/usrlocal moved from the root fs need labels
        for d in ["roothome", "usrlocal", "lib/plexy"] {
            let p = layout.new_var.join(d);
            if p.exists() {
                var_dirs.push((
                    p,
                    match d {
                        "roothome" => "/var/roothome",
                        "usrlocal" => "/var/usrlocal",
                        _ => "/var/lib/plexy",
                    },
                ));
            }
        }
    }
    if !util::dry_run() {
        relabel(plan, &deploy, &var_dirs)?;
    }

    step("migration staged");
    println!("\n  The new system is installed and your data has been placed. Reboot to start it.");
    println!("  The old system's files remain under /sysroot until you run `plexy cleanup`.\n");
    if opts.reboot && !util::dry_run() {
        util::reboot_staged()?;
    }
    Ok(())
}

/// Install the running plexy binary at `dst`. Copies from /proc/self/exe
/// (valid even after /usr/local was moved) to a temporary file and renames it
/// into place, because `dst` may be this very binary, still executing.
pub fn install_self(dst: &Path) -> Result<()> {
    fs::create_dir_all(dst.parent().context("bad path")?)?;
    let tmp = dst.with_extension("plexy-new");
    fs::copy("/proc/self/exe", &tmp)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    fs::rename(&tmp, dst)?;
    Ok(())
}

/// fstab line for the separate /boot filesystem, by UUID, read-only like
/// the entries bootc writes itself.
fn boot_fstab_line() -> Option<String> {
    let out = try_run("findmnt", &["-n", "-o", "UUID,FSTYPE", "/boot"])?;
    let mut f = out.split_whitespace();
    let (uuid, fstype) = (f.next()?, f.next()?);
    Some(format!("UUID={uuid} /boot {fstype} defaults,ro 0 0"))
}

fn backup_boot() -> Result<()> {
    let dir = Path::new("/var/lib/plexy/backup");
    if util::dry_run() {
        return Ok(());
    }
    fs::create_dir_all(dir)?;
    step("backing up /boot and the EFI system partition to /var/lib/plexy/backup");
    for (src, name) in [
        ("/boot", "boot.tar"),
        ("/boot/efi", "esp.tar"),
        ("/efi", "efi.tar"),
    ] {
        if Path::new(src).is_dir() {
            let _ = run(
                "tar",
                &[
                    "--one-file-system",
                    "-cf",
                    &dir.join(name).display().to_string(),
                    "-C",
                    src,
                    ".",
                ],
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn free_space_budget() {
        let g = 1024 * 1024 * 1024u64;
        assert_eq!(deploy_room(None), MIN_FREE_FOR_IMAGE);
        assert_eq!(deploy_room(Some(10 * g)), 11 * g);
        assert_eq!(deploy_room(Some(20 * g)), 22 * g);
        assert_eq!(required_free_before_build(None, 10, 0), 18 * g);
        assert_eq!(required_free_before_build(None, 512, 0), 24 * g);
        assert_eq!(required_free_before_build(Some(20 * g), 0, 0), 24 * g);
        assert_eq!(required_free_before_build(Some(20 * g), 0, 2), 27 * g);
        assert_eq!(toolbox_unpack_room(None, 10 * g), 33 * g);
        assert_eq!(toolbox_unpack_room(Some(10 * g), 2 * g), 11 * g);
    }

    use super::*;

    #[test]
    fn merge_move_merges_and_parks_conflicts() {
        let t = std::env::temp_dir().join(format!("plexy-mv-{}", std::process::id()));
        let _ = fs::remove_dir_all(&t);
        let (src, dst) = (t.join("old/var"), t.join("new/var"));
        fs::create_dir_all(src.join("lib/nginx")).unwrap();
        fs::create_dir_all(src.join("log")).unwrap();
        fs::write(src.join("lib/nginx/state"), "user").unwrap();
        fs::write(src.join("log/app.log"), "old log").unwrap();
        fs::create_dir_all(dst.join("log")).unwrap();
        fs::write(dst.join("log/app.log"), "image default").unwrap();
        fs::write(dst.join("log/other"), "keep").unwrap();
        let mut j = MoveJournal::default();
        for e in fs::read_dir(&src).unwrap().flatten() {
            merge_move(&e.path(), &dst.join(e.file_name()), &mut j).unwrap();
        }
        assert_eq!(
            fs::read_to_string(dst.join("lib/nginx/state")).unwrap(),
            "user"
        );
        // user's version wins, image default is parked next to it
        assert_eq!(
            fs::read_to_string(dst.join("log/app.log")).unwrap(),
            "old log"
        );
        assert_eq!(
            fs::read_to_string(dst.join("log/app.log.plexy-image-default")).unwrap(),
            "image default"
        );
        assert_eq!(fs::read_to_string(dst.join("log/other")).unwrap(), "keep");
        assert!(!src.join("lib/nginx").exists());
        assert_eq!(j.moves.len(), 2);
        fs::remove_dir_all(&t).unwrap();
    }

    #[test]
    fn mail_spool_mapping() {
        assert_eq!(
            var_rel_for_target("mail/bob", Family::Debian, Family::Fedora),
            "spool/mail/bob"
        );
        assert_eq!(
            var_rel_for_target("spool/mail/bob", Family::Fedora, Family::Debian),
            "mail/bob"
        );
        assert_eq!(
            var_rel_for_target("lib/nginx", Family::Debian, Family::Fedora),
            "lib/nginx"
        );
    }

    #[test]
    fn var_left_behind_policy() {
        assert!(var_entry_left_behind("lib/dpkg"));
        assert!(var_entry_left_behind("lib/rpm"));
        assert!(var_entry_left_behind("lib/snapd"));
        assert!(!var_entry_left_behind("lib/flatpak"));
        assert!(!var_entry_left_behind("lib/postgresql"));
    }

    #[test]
    fn fstab_translation() {
        let old = "UUID=a / btrfs subvol=root 0 1\nUUID=a /boot btrfs subvol=boot 0 0\n\
                   UUID=a /home btrfs subvol=home,compress=zstd:1 0 0\nUUID=a /var btrfs subvol=var 0 0\n\
                   UUID=e /boot/efi vfat umask=0077 0 2\n/swap.img none swap sw 0 0\n\
                   UUID=d /data ext4 defaults 0 2\nnas:/share /mnt/nas nfs defaults\n# comment\n";
        let t = translate_fstab(old);
        assert_eq!(
            t,
            vec![
                "UUID=a /var/home btrfs subvol=home,compress=zstd:1 0 0",
                "UUID=a /var btrfs subvol=var 0 0",
                "/sysroot/swap.img none swap sw 0 0",
                "UUID=d /data ext4 defaults 0 2",
                "nas:/share /var/mnt/nas nfs defaults 0 0",
            ]
        );
    }

    #[test]
    fn target_translation() {
        assert_eq!(translate_target("/home").as_deref(), Some("/var/home"));
        assert_eq!(
            translate_target("/home/bob/media").as_deref(),
            Some("/var/home/bob/media")
        );
        assert_eq!(translate_target("/homework").as_deref(), Some("/homework"));
        assert_eq!(translate_target("/"), None);
        assert_eq!(translate_target("/boot/efi"), None);
    }

    #[test]
    fn id_remapping_only_for_differing_system_accounts() {
        let old = vec![
            PasswdEntry::parse("nginx:x:991:991::/:/sbin/nologin").unwrap(),
            PasswdEntry::parse("chrony:x:992:992::/:/sbin/nologin").unwrap(),
            PasswdEntry::parse("alice:x:1000:1000::/home/alice:/bin/bash").unwrap(),
        ];
        let new = vec![
            PasswdEntry::parse("nginx:x:985:985::/:/sbin/nologin").unwrap(),
            PasswdEntry::parse("chrony:x:992:992::/:/sbin/nologin").unwrap(),
        ];
        let m = id_remap(&old, &new);
        assert_eq!(m.into_iter().collect::<Vec<_>>(), vec![(991, 985)]);
    }

    fn manifest_with(humans: &str, groups: &str) -> Manifest {
        let mut m: Manifest =
            serde_json::from_str(include_str!("../tests/data/manifest-min.json")).unwrap();
        m.accounts = crate::state::Accounts {
            all_users: parse_passwd(humans),
            all_groups: parse_group(groups),
            humans: parse_passwd(humans)
                .into_iter()
                .filter(|u| u.uid >= 1000)
                .collect(),
            memberships: Default::default(),
        };
        for h in m.accounts.humans.clone() {
            let gs = m
                .accounts
                .all_groups
                .iter()
                .filter(|g| g.members.contains(&h.name))
                .map(|g| g.name.clone())
                .collect();
            m.accounts.memberships.insert(h.name.clone(), gs);
        }
        m
    }

    #[test]
    fn account_merge_debian_to_fedora() {
        let m = manifest_with(
            "root:x:0:0::/root:/bin/bash\nbob:x:1000:1000:Bob:/home/bob:/bin/bash\n",
            "sudo:x:27:bob\nbob:x:1000:\ndocker:x:998:bob\nlxd:x:110:bob\n",
        );
        let r = merge_accounts(
            &m,
            Family::Fedora,
            "root:x:0:0:root:/root:/bin/bash\n",
            "root:!locked::0:99999:7:::\n",
            "",
            "",
            "bin:x:1:1::/:/sbin/nologin\n",
            "wheel:x:10:\ndocker:x:990:\n",
            "root:$6$abc:19000:0:99999:7:::\nbob:$6$xyz:19000:0:99999:7:::\n",
            "",
        );
        assert!(r.passwd.contains("bob:x:1000:1000:Bob:/home/bob:/bin/bash"));
        assert!(r.shadow.starts_with("root:$6$abc"));
        assert!(r.shadow.contains("bob:$6$xyz"));
        assert!(r.group.contains("wheel:x:10:bob"), "{}", r.group);
        assert!(r.group.contains("bob:x:1000:"));
        // docker exists in the image under another gid: membership uses the image's group
        assert!(r.group.contains("docker:x:990:bob"));
        // lxd is unknown to the target: recreated with its old gid
        assert!(r.group.contains("lxd:x:110:bob"));
    }
}
