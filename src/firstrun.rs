//! Stages that can only run on the *new* system:
//!
//! * `plexy firstboot`  (system service, root): regenerate caches derived from
//!   migrated config, install Flatpak replacements, relabel when needed.
//! * `plexy firstlogin` (user service, every user): load and create the
//!   toolbox, export its apps and commands, install Homebrew formulae, and
//!   tell the user what happened.

use crate::plan::{Dest, Plan};
use crate::util::{self, have, info, run, try_run, warn, write_file};
use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const STATE: &str = "/var/lib/plexy";

fn load_plan() -> Result<Plan> {
    crate::plan::load(&Path::new(STATE).join("plan.json"))
        .context("no plan at /var/lib/plexy/plan.json")
}

pub fn firstboot() -> Result<()> {
    let plan = load_plan()?;
    let mut report = vec![format!(
        "Plexy migration to {} completed first boot.",
        plan.target_image
    )];

    if have("dconf") && Path::new("/etc/dconf/db").exists() {
        let _ = run("dconf", &["update"]);
        report.push("system dconf databases rebuilt".into());
    }
    if have("update-ca-trust")
        && Path::new("/etc/pki/ca-trust/source/anchors")
            .read_dir()
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
    {
        let _ = run("update-ca-trust", &["extract"]);
        report.push("custom CA certificates re-trusted".into());
    }
    if have("update-ca-certificates") && Path::new("/usr/local/share/ca-certificates").exists() {
        let _ = run("update-ca-certificates", &[] as &[&str]);
    }
    if have("systemd-sysusers") {
        let _ = run("systemd-sysusers", &[] as &[&str]);
    }
    if Path::new("/sys/fs/selinux/enforce").exists() && Path::new(STATE).join("relabel").exists() {
        for p in fs::read_to_string(Path::new(STATE).join("relabel"))
            .unwrap_or_default()
            .lines()
        {
            let _ = run("restorecon", &["-RF", p]);
        }
        report.push("SELinux labels restored on migrated data".into());
    }
    if !plan.flatpaks_to_install.is_empty() && have("flatpak") {
        let _ = run(
            "flatpak",
            &[
                "remote-add",
                "--system",
                "--if-not-exists",
                "flathub",
                "https://dl.flathub.org/repo/flathub.flatpakrepo",
            ],
        );
        let mut args = vec!["install", "--system", "--noninteractive", "-y", "flathub"];
        args.extend(plan.flatpaks_to_install.iter().map(String::as_str));
        match run("flatpak", &args) {
            Ok(_) => report.push(format!(
                "installed Flatpaks: {}",
                plan.flatpaks_to_install.join(", ")
            )),
            Err(e) => report.push(format!(
                "Flatpak install failed (will retry next boot): {e}"
            )),
        }
    }
    // The toolbox archive is loaded by each user's rootless podman.
    let archive = Path::new(STATE).join("staging/toolbox.tar");
    if archive.exists() {
        let _ = fs::set_permissions(&archive, fs::Permissions::from_mode(0o644));
        let _ = fs::set_permissions(
            Path::new(STATE).join("staging"),
            fs::Permissions::from_mode(0o755),
        );
        let _ = fs::set_permissions(STATE, fs::Permissions::from_mode(0o755));
    }
    write_file(
        &Path::new(STATE).join("report.txt"),
        report.join("\n") + "\n",
    )?;
    write_file(
        &Path::new(STATE).join("firstboot.done"),
        util::now_unix().to_string(),
    )?;
    info(report.join("; "));
    Ok(())
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
}

fn notify(summary: &str, body: &str) {
    if have("notify-send") {
        let _ = try_run(
            "notify-send",
            &[
                "--app-name=Plexy",
                "-i",
                "system-software-update",
                summary,
                body,
            ],
        );
    }
}

pub fn firstlogin() -> Result<()> {
    // The unit has ConditionUser=!@system; this also covers units written by
    // older plexy versions. Root's toolbox would only take space users need.
    if util::is_root() {
        info("first login setup is for regular users; skipped for root");
        return Ok(());
    }
    let plan = load_plan()?;
    let state = home().join(".local/state/plexy");
    fs::create_dir_all(&state)?;
    let mut done = vec![];
    let mut retry = false;

    if let Some(tb) = &plan.toolbox {
        match setup_toolbox(&plan, tb) {
            Ok(n) => done.push(format!("toolbox '{}' ready ({n} exports)", tb.name)),
            Err(e) => {
                warn(format!("toolbox setup failed: {e:#}"));
                retry = true;
                if format!("{e:#}").contains("no space left on device") {
                    done.push(
                        "toolbox setup failed: the disk is full. Once the new system works, run \
                         `sudo plexy cleanup` to remove the old one, then log out and back in."
                            .into(),
                    );
                } else {
                    done.push(format!(
                        "toolbox setup failed (will retry at next login): {e}"
                    ));
                }
            }
        }
    }
    let formulas = plan.names_for(Dest::Brew);
    if !formulas.is_empty() {
        match setup_brew(&formulas) {
            Ok(Some(n)) => done.push(format!("{n} Homebrew formulae installed")),
            Ok(None) => {}
            Err(e) => done.push(format!("Homebrew install failed: {e}")),
        }
    }
    write_file(&state.join("firstlogin.log"), done.join("\n") + "\n")?;
    // Without the done marker the user service runs again at the next login.
    if !retry {
        write_file(&state.join("firstlogin.done"), util::now_unix().to_string())?;
    }
    if !done.is_empty() {
        notify("Your software has been migrated", &done.join("\n"));
    }
    info(done.join("; "));
    Ok(())
}

/// Load the prebuilt toolbox image into this user's container storage, create
/// the container and export apps/commands to the host.
fn setup_toolbox(plan: &Plan, tb: &crate::plan::ToolboxPlan) -> Result<usize> {
    let archive = Path::new(STATE).join("staging/toolbox.tar");
    if try_run("podman", &["image", "exists", &tb.image_tag]).is_none() {
        if archive.exists() {
            info("loading toolbox image");
            run("podman", &["load", "-i", &archive.display().to_string()])?;
            let _ = run("podman", &["tag", &tb.image_tag.to_string(), &tb.image_tag]);
        } else {
            // No archive (e.g. built elsewhere): build from the saved Containerfile.
            let ctx = Path::new(STATE).join("toolbox");
            run(
                "podman",
                &["build", "-t", &tb.image_tag, &ctx.display().to_string()],
            )?;
        }
    }
    let exists = try_run("podman", &["container", "exists", &tb.name]).is_some();
    if !exists {
        info(format!("creating {} container {}", tb.engine, tb.name));
        if tb.engine == "distrobox" && have("distrobox") {
            run(
                "distrobox",
                &[
                    "create",
                    "--yes",
                    "--name",
                    &tb.name,
                    "--image",
                    &tb.image_tag,
                ],
            )?;
            // first enter performs distrobox's in-container setup
            run("distrobox", &["enter", "--name", &tb.name, "--", "true"])?;
        } else {
            run(
                "toolbox",
                &["create", "-y", "-c", &tb.name, "--image", &tb.image_tag],
            )?;
        }
    }
    export_toolbox(plan, tb)
}

/// Commands that must not shadow the host's own tools.
fn host_has(bin: &str) -> bool {
    ["/usr/bin", "/usr/sbin", "/usr/local/bin"]
        .iter()
        .any(|d| Path::new(d).join(bin).exists())
}

fn wrapper(tb: &crate::plan::ToolboxPlan, cmd: &str) -> String {
    if tb.engine == "distrobox" && have("distrobox") {
        format!("distrobox enter --name {} -- {}", tb.name, cmd)
    } else {
        format!("toolbox run -c {} {}", tb.name, cmd)
    }
}

pub fn export_toolbox(plan: &Plan, tb: &crate::plan::ToolboxPlan) -> Result<usize> {
    let bin_dir = home().join(".local/bin");
    let app_dir = home().join(".local/share/applications");
    fs::create_dir_all(&bin_dir)?;
    fs::create_dir_all(&app_dir)?;
    let mut n = 0;
    for p in plan.by_dest(Dest::Toolbox) {
        // Preinstalled packages' commands are only exported when the host lacks them,
        // user-installed ones always (the user explicitly wanted them).
        for b in &p.traits.binaries {
            let name = b.rsplit('/').next().unwrap_or(b);
            if host_has(name) {
                continue;
            }
            if p.origin != "user" && !p.traits.gui {
                continue;
            }
            let path = bin_dir.join(name);
            let script = format!(
                "#!/bin/sh\n# exported by plexy from toolbox {}\nexec {} \"$@\"\n",
                tb.name,
                wrapper(tb, b)
            );
            write_file(&path, script)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
            n += 1;
        }
        for d in &p.traits.desktop_files {
            // The host already provides this app (as a package or a Flatpak).
            let flatpak_user = home().join(".local/share/flatpak/exports/share/applications");
            if [
                Path::new("/usr/share/applications"),
                Path::new("/var/lib/flatpak/exports/share/applications"),
                flatpak_user.as_path(),
            ]
            .iter()
            .any(|dir| dir.join(d).exists())
            {
                continue;
            }
            let inner = run(
                "podman",
                &[
                    "run",
                    "--rm",
                    "--net=none",
                    &tb.image_tag,
                    "cat",
                    &format!("/usr/share/applications/{d}"),
                ],
            );
            let Ok(text) = inner else { continue };
            let mut out = String::new();
            for line in text.lines() {
                if let Some(cmd) = line.strip_prefix("Exec=") {
                    out.push_str(&format!("Exec={}\n", wrapper(tb, cmd)));
                } else if let Some(nm) = line.strip_prefix("Name=") {
                    out.push_str(&format!("Name={nm} ({})\n", tb.name));
                } else if line.starts_with("TryExec=") || line.starts_with("DBusActivatable=") {
                    continue;
                } else {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            write_file(&app_dir.join(format!("plexy-{}-{d}", tb.name)), out)?;
            if let Some(icon) = text.lines().find_map(|l| l.strip_prefix("Icon=")) {
                export_icon(tb, icon.trim());
            }
            n += 1;
        }
    }
    let _ = try_run("update-desktop-database", &[app_dir.display().to_string()]);
    Ok(n)
}

/// Copy an icon theme entry (all sizes) out of the toolbox image so exported
/// launchers show their real icon on the host.
fn export_icon(tb: &crate::plan::ToolboxPlan, icon: &str) {
    if icon.is_empty() || icon.starts_with('/') || icon.contains(['\'', '"', ' ', '$']) {
        return;
    }
    let dest = home().join(".local/share");
    let script = format!(
        "cd /usr/share && find icons/hicolor pixmaps -name '{icon}.*' 2>/dev/null | tar -cf - -T - 2>/dev/null"
    );
    let cmd = format!(
        "podman run --rm --entrypoint /bin/sh {} -c \"{}\" | tar -xf - -C {}",
        tb.image_tag,
        script,
        dest.display()
    );
    let _ = run("/bin/sh", &["-c", &cmd]);
    let _ = try_run(
        "gtk-update-icon-cache",
        &[
            "-f",
            "-t",
            &dest.join("icons/hicolor").display().to_string(),
        ],
    );
}

fn brew_bin() -> Option<PathBuf> {
    [
        "/home/linuxbrew/.linuxbrew/bin/brew",
        "/var/home/linuxbrew/.linuxbrew/bin/brew",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
}

fn setup_brew(formulas: &[String]) -> Result<Option<usize>> {
    let brew = match brew_bin() {
        Some(b) => b,
        None => {
            info("installing Homebrew");
            run(
                "/bin/bash",
                &[
                    "-c",
                    "NONINTERACTIVE=1 /bin/bash -c \"$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\"",
                ],
            )?;
            brew_bin().context("Homebrew did not install")?
        }
    };
    // Only the prefix owner can install into it.
    let owner = std::os::unix::fs::MetadataExt::uid(&fs::metadata(
        brew.parent().unwrap().parent().unwrap(),
    )?);
    let me = std::os::unix::fs::MetadataExt::uid(&fs::metadata(home())?);
    if owner != me {
        return Ok(None);
    }
    let b = brew.display().to_string();
    let mut args = vec!["install".to_string()];
    args.extend(formulas.iter().cloned());
    run(&b, &args)?;
    // Config of tools moved to Homebrew: brew-built tools read $HOMEBREW_PREFIX/etc.
    let staged = Path::new(STATE).join("staging/brew-etc");
    if staged.exists() {
        let prefix = brew.parent().unwrap().parent().unwrap().join("etc");
        copy_tree(&staged, &prefix)?;
    }
    Ok(Some(formulas.len()))
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    for e in fs::read_dir(src)?.flatten() {
        let s = e.path();
        let d = dst.join(e.file_name());
        if s.is_dir() {
            fs::create_dir_all(&d)?;
            copy_tree(&s, &d)?;
        } else if !d.exists() {
            fs::create_dir_all(dst)?;
            fs::copy(&s, &d)?;
        }
    }
    Ok(())
}
