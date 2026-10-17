//! plexy — migrate a traditional ("mutable") Linux installation in place to an
//! image-based ("immutable") one, keeping packages, configuration, users,
//! services, desktop settings, Flatpaks, Homebrew and data.

mod abroot;
mod apply;
mod build;
mod desktop;
mod firstrun;
mod manifest;
mod mapping;
mod os;
mod pkg;
mod plan;
mod registry;
mod rollback;
mod state;
mod target;
mod util;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use plan::{Dest, Mode};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use util::{info, step, warn};

#[derive(Parser)]
#[command(
    name = "plexy",
    version,
    about = "Migrate a traditional Linux install in place to an image-based OS \
(Bluefin, Silverblue, Aurora, Vanilla OS, ...) keeping your packages, config, users and data."
)]
struct Cli {
    #[arg(short, long, global = true)]
    verbose: bool,
    /// Show what would be done without changing anything.
    #[arg(long, global = true)]
    dry_run: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
struct TargetArgs {
    /// Catalog name (see `plexy targets`) or any image reference. `auto`
    /// picks a catalog image whose desktop matches this system.
    #[arg(short, long, default_value = "auto")]
    target: String,
    /// Where packages go by default (each one can be changed in the review).
    #[arg(short, long, value_enum, default_value_t = Mode::Image)]
    mode: Mode,
    /// Allow a target whose desktop differs from the current one.
    #[arg(long)]
    allow_desktop_change: bool,
    /// Also migrate software that came with the old OS but is missing from
    /// the target (toolbox mode always does).
    #[arg(long)]
    include_preinstalled: bool,
    /// Containerfile snippet appended to the derived system image (extra
    /// packages, files, RUN steps) — your own adjustments to the target.
    #[arg(long, value_name = "FILE")]
    customize: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Inventory this system (packages, config, users, desktop, wrappers) as JSON.
    Scan {
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// List the target registry: OSes, how they update, and which
    /// conversions are available (from this system with `--from auto`).
    Targets {
        /// Show conversions available from a source: an os-release id
        /// (ubuntu, fedora, ultramarine, silverblue, ...) or `auto` for this system.
        #[arg(long)]
        from: Option<String>,
        /// With --from <id>: the source is already image-based (plexy switch).
        #[arg(long)]
        image_based: bool,
        /// Show which registry files were loaded.
        #[arg(long)]
        files: bool,
    },
    /// Read a target image's facts: desktop, package count, tooling.
    Inspect { image: String },
    /// Compare this system against a target: what is already there, what is
    /// layered, boxed, brewed or skipped — and why.
    Diff {
        #[command(flatten)]
        t: TargetArgs,
    },
    /// Produce a reviewable migration plan (JSON) and the build contexts.
    Plan {
        #[command(flatten)]
        t: TargetArgs,
        #[arg(short, long, default_value = "/var/lib/plexy/plan")]
        out: PathBuf,
        /// Do not ask questions; accept the computed plan.
        #[arg(short, long)]
        yes: bool,
    },
    /// Build the derived system image and the toolbox image from a plan.
    Build {
        #[arg(short, long, default_value = "/var/lib/plexy/plan")]
        plan: PathBuf,
    },
    /// Apply a built plan: install the image and migrate all state. Destructive.
    Apply {
        #[arg(short, long, default_value = "/var/lib/plexy/plan")]
        plan: PathBuf,
        #[arg(long)]
        reboot: bool,
        #[arg(short, long)]
        yes: bool,
        /// Proceed even if other boot loaders live on the ESP (bootc rewrites EFI/).
        #[arg(long)]
        allow_esp_wipe: bool,
        /// ABRoot backend: block device that receives the A/B volumes
        /// (a second disk, or unpartitioned space on this one).
        #[arg(long)]
        abroot_disk: Option<String>,
    },
    /// scan + plan + build + apply in one go (asks before anything destructive).
    Migrate {
        #[command(flatten)]
        t: TargetArgs,
        #[arg(long)]
        reboot: bool,
        #[arg(short, long)]
        yes: bool,
        #[arg(long)]
        allow_esp_wipe: bool,
        #[arg(long)]
        abroot_disk: Option<String>,
    },
    /// Already on an image-based OS: switch to another image keeping layered packages.
    Switch {
        #[command(flatten)]
        t: TargetArgs,
        #[arg(short, long)]
        yes: bool,
    },
    /// After a successful migration: delete the old system's files from /sysroot.
    Cleanup {
        #[arg(short, long)]
        yes: bool,
    },
    /// On the migrated system: go back to the previous OS (reverses data moves,
    /// restores /boot and the EFI partition from the pre-migration backup).
    Rollback {
        #[arg(short, long)]
        yes: bool,
        #[arg(long)]
        reboot: bool,
    },
    /// Re-export toolbox apps and commands to the host.
    Export,
    /// (internal) first-boot stage, run by plexy-firstboot.service.
    #[command(hide = true)]
    Firstboot,
    /// Follow the upstream remote: rebuild a derived system image when its
    /// remote changed (run weekly by plexy-update.timer), or explain how this
    /// system updates.
    Update {
        /// Rebuild even if the remote is unchanged.
        #[arg(long)]
        force: bool,
    },
    /// (internal) layer packages client-side, run by plexy-layer.service.
    #[command(hide = true)]
    Layer,
    /// (internal) first-login stage, run by the plexy-firstlogin user service.
    #[command(hide = true)]
    Firstlogin,
}

fn main() {
    let cli = Cli::parse();
    util::set_verbose(cli.verbose);
    util::set_dry_run(cli.dry_run);
    if let Err(e) = real_main(cli) {
        eprintln!("\x1b[1;31merror:\x1b[0m {e:#}");
        std::process::exit(1);
    }
}

fn real_main(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Scan { output } => {
            let m = manifest::scan()?;
            let js = serde_json::to_string_pretty(&m)?;
            match output {
                Some(p) => {
                    util::write_file(&p, js)?;
                    info(format!("manifest written to {}", p.display()));
                    print_scan_summary(&m);
                }
                None => println!("{js}"),
            }
        }
        Cmd::Targets {
            from,
            image_based,
            files,
        } => print_targets(from.as_deref(), image_based, files)?,
        Cmd::Inspect { image } => {
            let reg = registry::Registry::load()?;
            let (img, entry) = reg.resolve(&image);
            let f = target::inspect(&img)?;
            println!("image     : {}", f.image);
            println!("os        : {} ({:?})", f.pretty_name, f.family);
            println!("backend   : {:?}", f.backend);
            println!(
                "desktop   : {} (sessions: {})",
                f.desktop
                    .primary
                    .map(desktop::Desktop::label)
                    .unwrap_or("?"),
                f.desktop.sessions.join(", ")
            );
            println!("packages  : {}", f.packages.len());
            println!(
                "tools     : distrobox={} toolbox={} flatpak={} brew={} rpm-ostree={} selinux={}",
                f.has_distrobox,
                f.has_toolbox,
                f.has_flatpak,
                f.has_brew,
                f.has_rpm_ostree,
                f.selinux
            );
            if let Some(t) = entry {
                println!(
                    "registry  : {} — updates from {}{}, layering: {:?} ({})",
                    t.id,
                    t.remote(),
                    if t.update.signed { " (signed)" } else { "" },
                    t.update.layering,
                    t.origin
                );
            }
        }
        Cmd::Diff { t } => {
            let m = manifest::scan()?;
            let (name, facts) = choose_target(&m, &t.target)?;
            let p = make_plan(&m, &facts, name, &t)?;
            plan::print_summary(&p);
            println!("  per-package detail:");
            for x in &p.packages {
                if x.dest != Dest::Base || cli.verbose {
                    println!(
                        "    {:<36} {:<8} {}",
                        x.name,
                        format!("{:?}", x.dest).to_lowercase(),
                        x.why
                    );
                }
            }
        }
        Cmd::Plan { t, out, yes } => {
            let m = manifest::scan()?;
            let (name, facts) = choose_target(&m, &t.target)?;
            let mut p = make_plan(&m, &facts, name, &t)?;
            review_or_print(&mut p, yes)?;
            write_plan_dir(&m, &p, &out, t.customize.as_deref())?;
        }
        Cmd::Build { plan: dir } => {
            let p = plan::load(&dir.join("plan.json"))?;
            build_all(&p, &dir)?;
        }
        Cmd::Apply {
            plan: dir,
            reboot,
            yes,
            allow_esp_wipe,
            abroot_disk,
        } => {
            let p = plan::load(&dir.join("plan.json"))?;
            let m: manifest::Manifest =
                serde_json::from_str(&fs::read_to_string(dir.join("manifest.json"))?)?;
            confirm_destructive(&p, yes)?;
            do_apply(&m, &p, &dir, reboot, allow_esp_wipe, abroot_disk)?;
        }
        Cmd::Migrate {
            t,
            reboot,
            yes,
            allow_esp_wipe,
            abroot_disk,
        } => {
            let m = manifest::scan()?;
            print_scan_summary(&m);
            if m.os.image_based {
                bail!("this system is already image-based; use `plexy switch`");
            }
            let (name, facts) = choose_target(&m, &t.target)?;
            let mut p = make_plan(&m, &facts, name, &t)?;
            review_or_print(&mut p, yes)?;
            let dir = PathBuf::from("/var/lib/plexy/plan");
            write_plan_dir(&m, &p, &dir, t.customize.as_deref())?;
            if p.backend == target::Backend::Bootc {
                // The new deployment lands on the same filesystem as the old one.
                apply::require_free_root(
                    apply::required_free_before_build(
                        apply::image_size(&p.target_image),
                        if p.toolbox.is_some() {
                            p.names_for(plan::Dest::Toolbox).len()
                        } else {
                            0
                        },
                        p.flatpaks_to_install.len(),
                    ),
                    "building the images and installing the new system next to the old one",
                )?;
            }
            build_all(&p, &dir)?;
            confirm_destructive(&p, yes)?;
            do_apply(&m, &p, &dir, reboot, allow_esp_wipe, abroot_disk)?;
        }
        Cmd::Switch { t, yes } => switch(&t, yes)?,
        Cmd::Cleanup { yes } => cleanup(yes)?,
        Cmd::Rollback { yes, reboot } => rollback::rollback(yes, reboot)?,
        Cmd::Export => {
            let p = plan::load(&Path::new(firstrun::STATE).join("plan.json"))?;
            let tb = p.toolbox.clone().context("plan has no toolbox")?;
            let n = firstrun::export_toolbox(&p, &tb)?;
            info(format!("{n} apps/commands exported"));
        }
        Cmd::Firstboot => firstrun::firstboot()?,
        Cmd::Firstlogin => firstrun::firstlogin()?,
        Cmd::Layer => firstrun::layer()?,
        Cmd::Update { force } => firstrun::update(force)?,
    }
    Ok(())
}

fn review_or_print(p: &mut plan::Plan, yes: bool) -> Result<()> {
    if !yes && std::io::stdin().is_terminal() {
        let stdin = std::io::stdin();
        if !plan::review(p, &mut stdin.lock(), &mut std::io::stdout())? {
            bail!("aborted");
        }
    } else {
        plan::print_summary(p);
    }
    Ok(())
}

fn print_scan_summary(m: &manifest::Manifest) {
    println!();
    println!(
        "  system   : {} ({:?}, {:?})",
        m.os.pretty_name, m.os.family, m.os.pkg_manager
    );
    println!(
        "  desktop  : {} (display manager: {})",
        m.desktop
            .primary
            .map(desktop::Desktop::label)
            .unwrap_or("?"),
        m.desktop.display_manager.as_deref().unwrap_or("-")
    );
    println!(
        "  boot     : {} root={} ({}){}",
        if m.boot.uefi { "UEFI" } else { "BIOS" },
        m.boot.root_source,
        m.boot.root_fstype,
        m.boot
            .root_subvol
            .as_ref()
            .map(|s| format!(" subvol={s}"))
            .unwrap_or_default()
    );
    println!(
        "  packages : {} installed, {} user-installed",
        m.packages.installed.len(),
        m.packages.user_installed.len()
    );
    let changed = m
        .packages
        .config_changes
        .iter()
        .filter(|c| c.kind == pkg::ConfigKind::Modified)
        .count();
    let unowned = m
        .packages
        .config_changes
        .iter()
        .filter(|c| c.kind == pkg::ConfigKind::Unowned)
        .count();
    println!("  config   : {changed} modified package configs, {unowned} unowned files in /etc");
    println!("  repos    : {}", m.repos.len());
    println!(
        "  users    : {}",
        m.accounts
            .humans
            .iter()
            .map(|h| h.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("  services : {} enabled", m.system.enabled_units.len());
    if let Some(f) = &m.wrappers.flatpak {
        println!(
            "  flatpak  : {} system apps, remotes: {}",
            f.system_apps.len(),
            f.system_remotes.join(",")
        );
    }
    if let Some(b) = &m.wrappers.homebrew {
        println!("  homebrew : {} formulae at {}", b.formulae.len(), b.prefix);
    }
    if !m.wrappers.snaps.is_empty() {
        println!(
            "  snaps    : {}",
            m.wrappers
                .snaps
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!();
}

/// Resolve `--target`. "auto" walks catalog candidates for this desktop and
/// keeps the first whose *inspected* desktop matches.
/// Resolve `--target`. "auto" walks the registry's candidates for this
/// desktop and source (verified conversions first) and keeps the first whose
/// *inspected* desktop matches.
fn choose_target(m: &manifest::Manifest, t: &str) -> Result<(String, target::ImageFacts)> {
    util::ensure_tools(&[("podman", "podman")])?;
    let reg = registry::Registry::load()?;
    if t != "auto" {
        let (img, _) = reg.resolve(t);
        return Ok((t.to_string(), target::inspect(&img)?));
    }
    let want = m.desktop.primary.unwrap_or(desktop::Desktop::Headless);
    for cand in reg.candidates(&m.os, want) {
        let f = target::inspect(&cand.image)?;
        if matches!(
            desktop::compare(&m.desktop, &f.desktop),
            desktop::Compat::Same | desktop::Compat::SourceHeadless
        ) {
            info(format!(
                "auto-selected target {} ({}): desktop matches",
                cand.id, cand.image
            ));
            return Ok((cand.id.clone(), f));
        }
    }
    bail!(
        "no registry target ships {} for {}. Pass --target <image> with an image that does (see `plexy targets --from auto`)",
        want.label(),
        m.os.pretty_name
    )
}

/// `plexy targets`: the registry, or the conversions available from a source.
fn print_targets(from: Option<&str>, image_based: bool, files: bool) -> Result<()> {
    let reg = registry::Registry::load()?;
    if files {
        for f in &reg.files {
            println!("  {f}");
        }
        println!(
            "  (add *.toml files to {})\n",
            registry::override_dirs()
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(" or ")
        );
    }
    let Some(src_name) = from else {
        println!(
            "{:<15} {:<9} {:<7} {:<8} {:<47} UPDATES",
            "NAME", "DESKTOP", "BACKEND", "LAYERS", "IMAGE"
        );
        for t in &reg.targets {
            println!(
                "{:<15} {:<9} {:<7} {:<8} {:<47} {}{}",
                t.id,
                format!("{:?}", t.desktop).to_lowercase(),
                format!("{:?}", t.backend).to_lowercase(),
                format!("{:?}", t.update.layering).to_lowercase(),
                t.image,
                t.remote(),
                if t.update.signed { " (signed)" } else { "" }
            );
        }
        println!(
            "\nConversions: `plexy targets --from auto` (this system) or `--from ubuntu` / `--from fedora` / ..."
        );
        println!("Any bootc image reference works too: --target ghcr.io/you/custom:latest");
        return Ok(());
    };
    let src = if src_name == "auto" {
        os::detect_os(Path::new("/"))
    } else {
        registry::source_from_name(src_name, image_based)
    };
    println!(
        "Conversions from {} ({}{}):\n",
        src.pretty_name,
        format!("{:?}", src.family).to_lowercase(),
        if src.image_based {
            ", image-based: plexy switch"
        } else {
            ", plexy migrate"
        }
    );
    println!(
        "{:<12} {:<15} {:<9} {:<27} NOTES",
        "STATUS", "TARGET", "DESKTOP", "MODES"
    );
    for (t, c) in reg.conversions_from(&src) {
        let modes = if c.modes.is_empty() {
            "-".to_string()
        } else {
            c.modes
                .iter()
                .map(|m| format!("{m:?}").to_lowercase())
                .collect::<Vec<_>>()
                .join(",")
        };
        let mut notes = c.notes.clone().unwrap_or_default();
        if let Some(e) = &c.evidence {
            if !notes.is_empty() {
                notes.push_str("; ");
            }
            notes.push_str(e);
        }
        println!(
            "{:<12} {:<15} {:<9} {:<27} {}",
            registry::status_label(c.status),
            t.id,
            format!("{:?}", t.desktop).to_lowercase(),
            modes,
            notes
        );
    }
    Ok(())
}

fn make_plan(
    m: &manifest::Manifest,
    facts: &target::ImageFacts,
    name: String,
    t: &TargetArgs,
) -> Result<plan::Plan> {
    step("classifying packages");
    let mode = t.mode;
    let mut p = make_plan_inner(m, facts, name, t, mode)?;
    apply_registry(&mut p, m, facts, t)?;
    if let Some(tb) = p.toolbox.as_mut() {
        // Toolbox images exist for the main distributions only; derivatives
        // fall back to the plain distribution image (distrobox initialises any image).
        let id_like =
            m.os.id_like
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
        let candidates = [
            tb.base_image.clone(),
            format!("docker.io/library/{}:{}", m.os.id, m.os.version_id),
            format!("docker.io/library/{id_like}:latest"),
        ];
        let found = candidates
            .iter()
            .find(|c| {
                util::try_run("podman", &["manifest", "inspect", c.as_str()]).is_some()
                    || util::try_run("podman", &["image", "exists", c.as_str()]).is_some()
            })
            .cloned();
        match found {
            Some(c) if c != tb.base_image => {
                warn(format!(
                    "{} not found; using {c} for the toolbox",
                    tb.base_image
                ));
                tb.base_image = c;
            }
            Some(_) => {}
            None => warn(format!(
                "no container image found for {}; the toolbox build will fail",
                m.os.pretty_name
            )),
        }
    }
    Ok(p)
}

/// Fill in how the migrated system is updated, and check the conversion
/// against the registry (refuse unsupported, warn on untested).
fn apply_registry(
    p: &mut plan::Plan,
    m: &manifest::Manifest,
    facts: &target::ImageFacts,
    t: &TargetArgs,
) -> Result<()> {
    let reg = registry::Registry::load()?;
    let (_, entry) = reg.resolve(&p.target_name);
    let entry = entry.or_else(|| reg.targets.iter().find(|e| e.image == p.target_image));
    match entry {
        Some(e) => {
            p.update_remote = e.remote().to_string();
            p.update_signed = e.update.signed;
            p.layering = e.update.layering;
            match e.conversion_for(&m.os) {
                Some(c) if c.status == registry::Status::Unsupported => bail!(
                    "{} -> {} is not supported: {}",
                    m.os.pretty_name,
                    e.name,
                    c.notes
                        .as_deref()
                        .unwrap_or("see `plexy targets --from auto`")
                ),
                Some(c) => {
                    p.conversion_status = Some(registry::status_label(c.status).into());
                    if c.status == registry::Status::Untested {
                        p.warnings.push(format!(
                            "{} -> {} has not been verified end to end yet (registry status: untested)",
                            m.os.pretty_name, e.name
                        ));
                    } else if !c.modes.is_empty() && !c.modes.contains(&p.mode) {
                        p.warnings.push(format!(
                            "{} -> {} is verified in mode(s) {:?}, not {:?}",
                            m.os.pretty_name, e.name, c.modes, p.mode
                        ));
                    }
                }
                None => p.warnings.push(format!(
                    "the registry lists no conversion from {} to {}; proceeding untested",
                    m.os.pretty_name, e.name
                )),
            }
        }
        None => {
            // An image outside the registry: follow it directly, layer
            // client-side when it ships rpm-ostree.
            p.update_remote = p.target_image.clone();
            p.update_signed = false;
            p.layering = match p.backend {
                target::Backend::Abroot => registry::Layering::Abroot,
                _ if facts.has_rpm_ostree => registry::Layering::Client,
                _ => registry::Layering::Image,
            };
        }
    }
    if t.customize.is_some() && p.layering == registry::Layering::Client {
        info(
            "--customize needs a derived image: layering into a local image instead of client-side",
        );
        p.layering = registry::Layering::Image;
    }
    Ok(())
}

fn make_plan_inner(
    m: &manifest::Manifest,
    facts: &target::ImageFacts,
    name: String,
    t: &TargetArgs,
    mode: Mode,
) -> Result<plan::Plan> {
    let files = package_files(
        m,
        mode == Mode::Toolbox || t.include_preinstalled,
        &facts.packages,
    )?;
    let brew_index = if matches!(mode, Mode::Brew | Mode::Hybrid) {
        load_brew_index()
    } else {
        BTreeMap::new()
    };
    plan::build(plan::PlanInputs {
        manifest: m,
        target: facts,
        target_name: name,
        mode,
        files,
        brew_index,
        allow_desktop_change: t.allow_desktop_change,
        include_preinstalled: t.include_preinstalled,
    })
}

/// File listings for the packages the planner will look at, in one pass.
fn package_files(
    m: &manifest::Manifest,
    all: bool,
    target_pkgs: &BTreeSet<String>,
) -> Result<BTreeMap<String, String>> {
    let want: BTreeSet<&str> = if all {
        m.packages.installed.keys().map(String::as_str).collect()
    } else {
        m.packages
            .user_installed
            .iter()
            .map(|p| p.name.as_str())
            .collect()
    };
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    match m.packages.manager.as_str() {
        "apt" => {
            for n in &want {
                for cand in [
                    format!("/var/lib/dpkg/info/{n}.list"),
                    format!("/var/lib/dpkg/info/{n}:amd64.list"),
                ] {
                    if let Ok(t) = fs::read_to_string(&cand) {
                        out.insert(n.to_string(), t);
                        break;
                    }
                }
            }
        }
        "dnf" | "dnf5" | "rpm-ostree" => {
            let all = util::run("rpm", &["-qa", "--qf", "[%{=NAME}\t%{FILENAMES}\n]"])?;
            for line in all.lines() {
                if let Some((n, f)) = line.split_once('\t')
                    && want.contains(n)
                {
                    let e = out.entry(n.to_string()).or_default();
                    e.push_str(f);
                    e.push('\n');
                }
            }
        }
        _ => {}
    }
    // Meta-packages (cockpit, ubuntu-restricted-addons, ...) own no files of
    // their own: classify them by what their direct dependencies ship.
    // Meta-packages (cockpit, ubuntu-restricted-addons, ...) ship nothing that
    // says what they are (only docs, icons, metadata): classify them by what
    // their direct dependencies ship.
    // Only user-installed packages: preinstalled trait-less packages are
    // libraries and data, and expanding thousands of them costs minutes.
    let user: BTreeSet<&str> = m
        .packages
        .user_installed
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    let meta: Vec<String> = want
        .iter()
        .filter(|n| user.contains(**n))
        .filter(|n| {
            let t = plan::classify_files(out.get(**n).map(String::as_str).unwrap_or(""));
            !t.cli && !t.gui && !t.host_level && t.desktop_files.is_empty()
        })
        .map(|n| n.to_string())
        .collect();
    for n in meta {
        let mut files = String::new();
        for dep in direct_deps(&m.packages.manager, &n) {
            if let Some(f) = dep_files(&m.packages.manager, &dep) {
                files.push_str(&f);
            }
        }
        if !files.is_empty() {
            out.entry(n).or_default().push_str(&files);
        }
    }
    // Services are often split from their binaries (Debian's nginx ships the
    // binary, nginx-common the unit). A user package whose direct dependencies
    // (absent from the target) ship service units is host-level software.
    for p in &m.packages.user_installed {
        for dep in direct_deps(&m.packages.manager, &p.name) {
            if target_pkgs.contains(&dep) {
                continue;
            }
            let Some(f) = dep_files(&m.packages.manager, &dep) else {
                continue;
            };
            let units: String = f
                .lines()
                .filter(|l| {
                    (l.starts_with("/usr/lib/systemd/system/")
                        || l.starts_with("/lib/systemd/system/"))
                        && (l.ends_with(".service") || l.ends_with(".socket"))
                })
                .map(|l| format!("{l}\n"))
                .collect();
            if !units.is_empty() {
                out.entry(p.name.clone()).or_default().push_str(&units);
            }
        }
    }
    Ok(out)
}

fn direct_deps(manager: &str, pkg: &str) -> Vec<String> {
    match manager {
        "apt" => {
            let st = fs::read_to_string("/var/lib/dpkg/status").unwrap_or_default();
            let mut in_pkg = false;
            for l in st.lines() {
                if let Some(p) = l.strip_prefix("Package: ") {
                    in_pkg = p == pkg;
                } else if in_pkg && let Some(d) = l.strip_prefix("Depends: ") {
                    return d
                        .split(',')
                        .filter_map(|x| x.split_whitespace().next())
                        .map(|x| x.split(':').next().unwrap_or(x).to_string())
                        .collect();
                }
            }
            vec![]
        }
        _ => {
            let reqs = util::try_run("rpm", &["-q", "--qf", "[%{REQUIRENAME}\n]", pkg])
                .unwrap_or_default();
            let reqs: Vec<&str> = reqs
                .lines()
                .filter(|r| !r.starts_with("rpmlib(") && !r.starts_with('/'))
                .collect();
            if reqs.is_empty() {
                return vec![];
            }
            let mut args = vec!["-q", "--qf", "%{NAME}\n", "--whatprovides"];
            args.extend(reqs);
            let mut v: Vec<String> = util::try_run("rpm", &args)
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.contains(' '))
                .map(str::to_string)
                .collect();
            v.sort();
            v.dedup();
            v
        }
    }
}

fn dep_files(manager: &str, pkg: &str) -> Option<String> {
    match manager {
        "apt" => fs::read_to_string(format!("/var/lib/dpkg/info/{pkg}.list"))
            .or_else(|_| fs::read_to_string(format!("/var/lib/dpkg/info/{pkg}:amd64.list")))
            .ok(),
        _ => util::try_run("rpm", &["-ql", pkg]),
    }
}

fn load_brew_index() -> BTreeMap<String, String> {
    let cache = PathBuf::from("/var/cache/plexy/formula.json");
    let fresh = fs::metadata(&cache)
        .and_then(|m| m.modified())
        .map(|t| {
            t.elapsed()
                .map(|e| e.as_secs() < 86400 * 7)
                .unwrap_or(false)
        })
        .unwrap_or(false);
    if !fresh {
        let _ = fs::create_dir_all(cache.parent().unwrap());
        info("downloading the Homebrew formula index");
        if util::run(
            "curl",
            &[
                "-fsSL",
                "-o",
                &cache.display().to_string(),
                "https://formulae.brew.sh/api/formula.json",
            ],
        )
        .is_err()
        {
            warn(
                "could not download the Homebrew formula index; brew routing limited to known names",
            );
            return BTreeMap::new();
        }
    }
    mapping::parse_brew_index(&fs::read_to_string(&cache).unwrap_or_default())
}

fn write_plan_dir(
    m: &manifest::Manifest,
    p: &plan::Plan,
    dir: &Path,
    customize: Option<&Path>,
) -> Result<()> {
    fs::create_dir_all(dir)?;
    util::write_file(&dir.join("manifest.json"), serde_json::to_string_pretty(m)?)?;
    plan::save(p, &dir.join("plan.json"))?;
    let cf = build::system_containerfile(m, p, &dir.join("system"))?;
    if let Some(extra) = customize {
        let snippet =
            fs::read_to_string(extra).with_context(|| format!("reading {}", extra.display()))?;
        let mut text = fs::read_to_string(&cf)?;
        text.push_str(&format!(
            "\n# --- customisations from {} ---\n{}\n",
            extra.display(),
            snippet.trim_end()
        ));
        util::write_file(&cf, text)?;
        info(format!("appended {} to the derived image", extra.display()));
    }
    build::toolbox_containerfile(m, p, &dir.join("toolbox"))?;
    info(format!(
        "review or edit {}/{{plan.json,system/Containerfile,toolbox/Containerfile}}",
        dir.display()
    ));
    Ok(())
}

/// ostree container reference for an image, signature-verified when asked
/// (the policy for it ships in the image, e.g. Universal Blue's cosign key).
pub fn ostree_ref(image: &str, signed: bool) -> String {
    if signed {
        format!("ostree-image-signed:docker://{image}")
    } else {
        format!("ostree-unverified-registry:{image}")
    }
}

/// Whether the system boots a locally built derived image. With client-side
/// layering the upstream image is installed as-is and packages are layered
/// by rpm-ostree on first boot, so the system follows the upstream remote.
fn uses_derived(p: &plan::Plan, dir: &Path) -> bool {
    p.layering != registry::Layering::Client && derived_needed(dir)
}

/// A derived image is only needed when the generated Containerfile does more
/// than FROM the target.
fn derived_needed(dir: &Path) -> bool {
    let cf = fs::read_to_string(dir.join("system/Containerfile")).unwrap_or_default();
    cf.lines().any(|l| {
        l.starts_with("RUN ") && !l.contains("bootc container lint") || l.starts_with("COPY ")
    })
}

fn build_all(p: &plan::Plan, dir: &Path) -> Result<()> {
    util::ensure_tools(&[("podman", "podman")])?;
    {
        if uses_derived(p, dir) {
            step(format!("building derived system image {}", p.derived_image));
            build::podman_build(&dir.join("system"), &p.derived_image)?;
        } else {
            if p.layering == registry::Layering::Client && !p.names_for(Dest::Image).is_empty() {
                info(format!(
                    "the system will follow {} directly; {} package(s) are layered with rpm-ostree on first boot",
                    p.remote(),
                    p.names_for(Dest::Image).len()
                ));
            } else {
                info("no packages to layer: the target image is used as-is");
            }
            target::pull(&p.target_image)?;
        }
    }
    if let Some(tb) = &p.toolbox {
        step(format!(
            "building toolbox image {} from {}",
            tb.image_tag, tb.base_image
        ));
        let base_was_present =
            util::try_run("podman", &["image", "exists", &tb.base_image]).is_some();
        build::podman_build(&dir.join("toolbox"), &tb.image_tag)?;
        let staging = Path::new("/var/lib/plexy/staging");
        fs::create_dir_all(staging)?;
        let archive = staging.join("toolbox.tar");
        let _ = fs::remove_file(&archive);
        util::run_visible(
            "podman",
            &[
                "save",
                "--format",
                // docker-archive loads by streaming; oci-archive is first
                // unpacked to /var/tmp, and podman leaves that copy behind
                // when a load fails.
                "docker-archive",
                "-o",
                &archive.display().to_string(),
                &tb.image_tag,
            ],
        )?;
        // First login loads the toolbox from the archive. Keeping the image in
        // container storage as well doubles its footprint on /, which is the
        // room the new deployment needs.
        if let Some(size) = apply::image_size(&tb.image_tag) {
            fs::write(apply::TOOLBOX_SIZE_FILE, size.to_string())?;
        }
        let _ = util::try_run("podman", &["rmi", &tb.image_tag]);
        if !base_was_present {
            let _ = util::try_run("podman", &["rmi", &tb.base_image]);
        }
    }
    Ok(())
}

fn confirm_destructive(p: &plan::Plan, yes: bool) -> Result<()> {
    println!();
    println!(
        "  About to replace the operating system with {} ({:?} backend).",
        p.target_image, p.backend
    );
    match p.backend {
        target::Backend::Bootc => {
            println!(
                "  /boot and the EFI boot entries will be rewritten; data, accounts and settings are carried over."
            );
            println!(
                "  A backup of /boot and the ESP is written to /var/lib/plexy/backup (`plexy rollback` uses it)."
            );
        }
        target::Backend::Abroot => {
            println!(
                "  New ABRoot volumes are created on the disk you named (its contents are erased);"
            );
            println!("  this system is left untouched and stays bootable from the firmware menu.");
        }
    }
    if yes {
        return Ok(());
    }
    print!("  Type 'migrate' to continue: ");
    use std::io::Write;
    std::io::stdout().flush()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    if s.trim() != "migrate" {
        bail!("aborted by user");
    }
    Ok(())
}

fn do_apply(
    m: &manifest::Manifest,
    p: &plan::Plan,
    dir: &Path,
    reboot: bool,
    allow_esp_wipe: bool,
    abroot_disk: Option<String>,
) -> Result<()> {
    let staging = PathBuf::from("/var/lib/plexy/staging");
    // The build contexts travel with the system so updates can rebuild them.
    let _ = fs::create_dir_all(&staging);
    match p.backend {
        target::Backend::Bootc => {
            let derived = uses_derived(p, dir);
            if derived {
                let _ = util::run(
                    "cp",
                    &[
                        "-a",
                        &dir.join("system").display().to_string(),
                        &staging.join("system").display().to_string(),
                    ],
                );
            }
            let (image, transport) = if derived {
                (
                    p.derived_image.clone(),
                    Some("containers-storage".to_string()),
                )
            } else {
                (p.target_image.clone(), None)
            };
            // Follow the real upstream remote unless the system boots a local
            // derived image (which `plexy update` rebuilds from that remote).
            let imgref = if derived {
                image.clone()
            } else {
                p.remote().to_string()
            };
            let opts = apply::ApplyOpts {
                target_imgref: imgref,
                image,
                target_transport: transport,
                reboot,
                allow_esp_wipe,
                stateroot: "default".into(),
            };
            apply::apply_bootc(m, p, &opts, &staging)
        }
        target::Backend::Abroot => abroot::apply(m, p, abroot_disk.as_deref(), reboot),
    }
}

fn switch(t: &TargetArgs, yes: bool) -> Result<()> {
    let m = manifest::scan()?;
    if !m.os.image_based {
        bail!("this system is not image-based; use `plexy migrate`");
    }
    let (name, facts) = choose_target(&m, &t.target)?;
    let mut p = make_plan(&m, &facts, name, t)?;
    review_or_print(&mut p, yes)?;
    let dir = PathBuf::from("/var/lib/plexy/plan");
    write_plan_dir(&m, &p, &dir, t.customize.as_deref())?;
    build_all(&p, &dir)?;
    confirm_destructive(&p, yes)?;
    // On an image-based host /etc and /var already survive a switch (3-way merge).
    if p.layering == registry::Layering::Client && util::have("rpm-ostree") {
        // Follow the upstream remote directly; layered packages ride along.
        let mut args = vec![
            "rebase".to_string(),
            ostree_ref(p.remote(), p.update_signed),
        ];
        for n in p.names_for(Dest::Image) {
            args.push("--install".into());
            args.push(n);
        }
        util::run_visible("rpm-ostree", &args)?;
    } else if uses_derived(&p, &dir) {
        util::run_visible(
            "bootc",
            &[
                "switch",
                "--transport",
                "containers-storage",
                &p.derived_image,
            ],
        )?;
    } else {
        util::run_visible("bootc", &["switch", p.remote()])?;
    }
    // Record what this image layers, so a later switch keeps it too.
    let state = Path::new(firstrun::STATE);
    let current = state.join("plan.json");
    if current.exists() {
        let _ = fs::copy(&current, state.join("plan.prev.json"));
    }
    if !util::dry_run() {
        fs::create_dir_all(state)?;
        fs::write(&current, serde_json::to_string_pretty(&p)?)?;
    }
    Ok(())
}

/// Remove the previous OS from the physical root once the new one works.
fn cleanup(yes: bool) -> Result<()> {
    if !Path::new("/run/ostree-booted").exists() {
        bail!("cleanup runs on the migrated (image-based) system");
    }
    let sysroot = Path::new("/sysroot");
    let old: Vec<PathBuf> = [
        "bin", "sbin", "lib", "lib32", "lib64", "libx32", "usr", "etc", "opt", "srv", "snap",
        "root", "home", "media", "mnt", "var", "cdrom", "swap.img",
    ]
    .iter()
    .map(|d| sysroot.join(d))
    .filter(|p| fs::symlink_metadata(p).is_ok())
    .collect();
    if old.is_empty() {
        info("nothing left to clean");
        return Ok(());
    }
    println!("  These entries of the previous system remain in /sysroot:");
    for p in &old {
        println!("    {}", p.display());
    }
    if !yes {
        print!("  Delete them? [y/N] ");
        use std::io::Write;
        std::io::stdout().flush()?;
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
        if !s.trim().eq_ignore_ascii_case("y") {
            return Ok(());
        }
    }
    util::run_visible("mount", &["-o", "remount,rw", "/sysroot"])?;
    for p in &old {
        // never cross into filesystems still mounted as the new /var or /var/home
        util::run_visible(
            "rm",
            &["-rf", "--one-file-system", &p.display().to_string()],
        )?;
    }
    let _ = util::run("mount", &["-o", "remount,ro", "/sysroot"]);
    Ok(())
}
