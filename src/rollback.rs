//! `plexy rollback`: return to the previous operating system.
//!
//! Runs on the migrated (bootc) system. The old root is still on disk (it is
//! the physical root, mounted at /sysroot), /boot and the ESP were archived
//! before bootc rewrote them, and every data move was journaled. Rollback
//! reverses the moves, restores /boot and the ESP and recreates the firmware
//! boot entry; after a reboot the old system starts as it was.

use crate::apply::{MoveJournal, REMAP_SKIP, chown_tree, move_path};
use crate::util::{self, info, read_opt, run, step, try_run, warn};
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const STATE: &str = "/var/lib/plexy";

/// Map a path recorded on the old system to where it is visible now.
/// Old /var (when kept as a mount) is still /var; everything else lived on the
/// old root filesystem, which is now /sysroot.
pub fn translate(path: &str, var_mounted: bool) -> PathBuf {
    if var_mounted && (path == "/var" || path.starts_with("/var/")) {
        return PathBuf::from(path);
    }
    PathBuf::from(format!("/sysroot{path}"))
}

fn esp_device() -> Option<String> {
    let fstab = read_opt("/sysroot/etc/fstab")?;
    let line = fstab.lines().find(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        f.len() > 2 && (f[1] == "/boot/efi" || f[1] == "/efi") && !l.trim_start().starts_with('#')
    })?;
    let src = line.split_whitespace().next()?;
    let dev = if let Some(u) = src.strip_prefix("UUID=") {
        format!("/dev/disk/by-uuid/{u}")
    } else if let Some(l) = src.strip_prefix("LABEL=") {
        format!("/dev/disk/by-label/{l}")
    } else if let Some(u) = src.strip_prefix("PARTUUID=") {
        format!("/dev/disk/by-partuuid/{u}")
    } else {
        src.to_string()
    };
    Some(dev)
}

/// The old distribution's shim (or GRUB) in the restored ESP.
fn old_loader(esp: &Path) -> Option<(String, String)> {
    let efi = esp.join("EFI");
    for e in fs::read_dir(&efi).ok()?.flatten() {
        let vendor = e.file_name().to_string_lossy().into_owned();
        if vendor.eq_ignore_ascii_case("BOOT") {
            continue;
        }
        for f in ["shimx64.efi", "grubx64.efi"] {
            if e.path().join(f).exists() {
                return Some((vendor.clone(), format!("\\EFI\\{vendor}\\{f}")));
            }
        }
    }
    None
}

fn invert(m: &BTreeMap<u32, u32>) -> BTreeMap<u32, u32> {
    m.iter().map(|(a, b)| (*b, *a)).collect()
}

pub fn rollback(yes: bool, reboot: bool) -> Result<()> {
    if !util::is_root() {
        bail!("plexy rollback must run as root");
    }
    if !Path::new("/run/ostree-booted").exists() {
        bail!("rollback runs on the migrated system (this one is not image-based)");
    }
    let journal: MoveJournal = serde_json::from_str(
        &fs::read_to_string(Path::new(STATE).join("moves.json"))
            .context("no move journal in /var/lib/plexy")?,
    )?;
    let backup = Path::new(STATE).join("backup");
    if !backup.join("boot.tar").exists() {
        bail!("no /boot backup in {}", backup.display());
    }
    let old = read_opt("/sysroot/etc/os-release")
        .map(|t| {
            util::parse_kv(&t)
                .get("PRETTY_NAME")
                .cloned()
                .unwrap_or_default()
        })
        .unwrap_or_else(|| journal.old_os.clone());
    println!("\n  This restores {old}: data moved during the migration goes back,");
    println!("  /boot and the EFI partition are restored from the backup taken before it.");
    println!("  Changes made on the new system to files in /etc are not carried back.\n");
    if !yes {
        print!("  Type 'rollback' to continue: ");
        use std::io::Write;
        std::io::stdout().flush()?;
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
        if s.trim() != "rollback" {
            bail!("aborted");
        }
    }
    util::run_visible("mount", &["-o", "remount,rw", "/sysroot"])?;

    step(format!("moving {} entries back", journal.moves.len()));
    for (src, dst) in journal.moves.iter().rev() {
        let from = translate(dst, journal.var_mounted);
        let to = translate(src, journal.var_mounted);
        if fs::symlink_metadata(&from).is_err() {
            warn(format!("{} is gone; skipping", from.display()));
            continue;
        }
        if let Some(p) = to.parent() {
            fs::create_dir_all(p)?;
        }
        if fs::symlink_metadata(&to).is_ok() {
            // something new appeared at the old location: keep both
            let _ = fs::rename(&to, to.with_extension("plexy-new-system"));
        }
        move_path(&from, &to)?;
    }

    if !journal.uid_remap.is_empty() || !journal.gid_remap.is_empty() {
        step("restoring the old ownership of system accounts' data");
        let root = translate("/var", journal.var_mounted);
        let n = chown_tree(
            &root,
            &invert(&journal.uid_remap),
            &invert(&journal.gid_remap),
            REMAP_SKIP,
        );
        info(format!("  {n} files re-owned"));
    }

    step("restoring /boot");
    let _ = run("mount", &["-o", "remount,rw", "/boot"]);
    for e in fs::read_dir("/boot")?.flatten() {
        let p = e.path();
        if p.file_name().map(|n| n == "efi").unwrap_or(false) {
            continue;
        }
        let _ = run(
            "rm",
            &["-rf", "--one-file-system", &p.display().to_string()],
        );
    }
    util::run_visible(
        "tar",
        &[
            "-xpf",
            &backup.join("boot.tar").display().to_string(),
            "-C",
            "/boot",
        ],
    )?;

    if let (Some(dev), true) = (esp_device(), backup.join("esp.tar").exists()) {
        step("restoring the EFI system partition");
        let mnt = Path::new("/run/plexy-esp");
        fs::create_dir_all(mnt)?;
        util::run_visible("mount", &[&dev, &mnt.display().to_string()])?;
        for d in ["EFI", "loader"] {
            let _ = run("rm", &["-rf", &mnt.join(d).display().to_string()]);
        }
        util::run_visible(
            "tar",
            &[
                "-xf",
                &backup.join("esp.tar").display().to_string(),
                "-C",
                &mnt.display().to_string(),
            ],
        )?;
        if let Some((vendor, loader)) = old_loader(mnt) {
            let entries = try_run("efibootmgr", &["-v"]).unwrap_or_default();
            if !entries.to_lowercase().contains(&loader.to_lowercase()) {
                let part = run(
                    "findmnt",
                    &["-n", "-o", "SOURCE", &mnt.display().to_string()],
                )?
                .trim()
                .to_string();
                let disk = run("lsblk", &["-n", "-o", "PKNAME", &part])?
                    .trim()
                    .to_string();
                let num = fs::read_to_string(format!(
                    "/sys/class/block/{}/partition",
                    part.trim_start_matches("/dev/")
                ))?;
                util::run_visible(
                    "efibootmgr",
                    &[
                        "--create",
                        "--disk",
                        &format!("/dev/{disk}"),
                        "--part",
                        num.trim(),
                        "--label",
                        &vendor,
                        "--loader",
                        &loader,
                    ],
                )?;
            } else {
                info(format!("firmware entry for {loader} present"));
            }
        }
        let _ = run("umount", &[mnt.display().to_string().as_str()]);
    } else {
        warn("could not find the old ESP; the firmware may need its boot entry fixed");
    }
    let _ = run("sync", &[] as &[&str]);
    println!("\n  Done. Reboot to start {old}. The image-based system remains in /ostree");
    println!("  on the old root; delete /ostree from there once you no longer need it.\n");
    if reboot {
        run("systemctl", &["reboot"])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remap_inversion() {
        let m: BTreeMap<u32, u32> = [(991, 985), (992, 990)].into_iter().collect();
        let i = invert(&m);
        assert_eq!(i.get(&985), Some(&991));
        assert_eq!(i.get(&990), Some(&992));
    }

    #[test]
    fn path_translation() {
        assert_eq!(
            translate("/root/.ssh", true),
            PathBuf::from("/sysroot/root/.ssh")
        );
        assert_eq!(
            translate("/var/roothome/.ssh", true),
            PathBuf::from("/var/roothome/.ssh")
        );
        assert_eq!(
            translate("/ostree/deploy/default/var/home/bob", false),
            PathBuf::from("/sysroot/ostree/deploy/default/var/home/bob")
        );
        assert_eq!(
            translate("/var/lib/nginx", false),
            PathBuf::from("/sysroot/var/lib/nginx")
        );
    }
}
