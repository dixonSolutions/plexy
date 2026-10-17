//! Small process / filesystem helpers shared by every module.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

static VERBOSE: AtomicBool = AtomicBool::new(false);
// Thread-local so tests can exercise dry-run paths without racing each other
// (the program itself is single-threaded).
thread_local! {
    static DRY_RUN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn set_verbose(v: bool) {
    VERBOSE.store(v, Ordering::Relaxed);
}
pub fn set_dry_run(v: bool) {
    DRY_RUN.with(|d| d.set(v));
}
pub fn dry_run() -> bool {
    DRY_RUN.with(|d| d.get())
}

pub fn info(msg: impl AsRef<str>) {
    eprintln!("\x1b[1;34m::\x1b[0m {}", msg.as_ref());
}
pub fn warn(msg: impl AsRef<str>) {
    eprintln!("\x1b[1;33mwarning:\x1b[0m {}", msg.as_ref());
}
pub fn step(msg: impl AsRef<str>) {
    eprintln!("\x1b[1;32m==>\x1b[0m \x1b[1m{}\x1b[0m", msg.as_ref());
}
pub fn debug(msg: impl AsRef<str>) {
    if VERBOSE.load(Ordering::Relaxed) {
        eprintln!("\x1b[2m   {}\x1b[0m", msg.as_ref());
    }
}

fn render<S: AsRef<OsStr>>(prog: &str, args: &[S]) -> String {
    let mut s = prog.to_string();
    for a in args {
        s.push(' ');
        s.push_str(&a.as_ref().to_string_lossy());
    }
    s
}

/// Run a command, capture stdout. Fails on non-zero exit.
pub fn run<S: AsRef<OsStr>>(prog: &str, args: &[S]) -> Result<String> {
    debug(format!("$ {}", render(prog, args)));
    let out = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to spawn {prog}"))?;
    if !out.status.success() {
        bail!(
            "`{}` exited with {}: {}",
            render(prog, args),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run a command and return stdout, or None if it is missing or fails.
pub fn try_run<S: AsRef<OsStr>>(prog: &str, args: &[S]) -> Option<String> {
    run(prog, args).ok()
}

/// Run a command with inherited stdio (for long, user-visible operations).
/// Honours --dry-run.
pub fn run_visible<S: AsRef<OsStr>>(prog: &str, args: &[S]) -> Result<()> {
    let line = render(prog, args);
    if dry_run() {
        info(format!("[dry-run] {line}"));
        return Ok(());
    }
    info(format!("$ {line}"));
    let st = Command::new(prog)
        .args(args)
        .status()
        .with_context(|| format!("failed to spawn {prog}"))?;
    if !st.success() {
        bail!("`{line}` exited with {st}");
    }
    Ok(())
}

/// Run with a string piped to stdin; returns stdout.
pub fn run_stdin<S: AsRef<OsStr>>(prog: &str, args: &[S], input: &str) -> Result<String> {
    debug(format!("$ {} <<stdin", render(prog, args)));
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {prog}"))?;
    child.stdin.take().unwrap().write_all(input.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "`{}` exited with {}: {}",
            render(prog, args),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn have(prog: &str) -> bool {
    if prog.contains('/') {
        return Path::new(prog).exists();
    }
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(prog).is_file()))
        .unwrap_or(false)
}

/// Make sure the given commands exist, installing their packages with the
/// system package manager when they are missing. `pairs` = (command, package).
pub fn ensure_tools(pairs: &[(&str, &str)]) -> Result<()> {
    let missing: Vec<&str> = pairs
        .iter()
        .filter(|(c, _)| {
            !have(c)
                && !Path::new("/usr/sbin").join(c).exists()
                && !Path::new("/sbin").join(c).exists()
        })
        .map(|(_, p)| *p)
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    info(format!("installing required tools: {}", missing.join(" ")));
    if have("dnf5") || have("dnf") {
        let dnf = if have("dnf5") { "dnf5" } else { "dnf" };
        let mut a = vec!["install", "-y"];
        a.extend(missing.iter().copied());
        run_visible(dnf, &a)
    } else if have("apt-get") {
        let _ = run_visible("apt-get", &["update", "-q"]);
        let mut a = vec!["install", "-y", "-q"];
        a.extend(missing.iter().copied());
        run_visible("apt-get", &a)
    } else {
        bail!(
            "missing tools ({}) and no supported package manager to install them",
            missing.join(" ")
        )
    }
}

pub fn read_opt(p: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(p).ok()
}

/// Parse a KEY=VALUE file (os-release, /etc/default/locale, ...).
pub fn parse_kv(text: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            m.insert(k.trim().to_string(), v.to_string());
        }
    }
    m
}

pub fn is_root() -> bool {
    // SAFETY-free: read effective uid from /proc to avoid a libc dependency.
    read_opt("/proc/self/status")
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(2).map(|u| u == "0"))
        })
        .unwrap_or(false)
}

/// Join an absolute path under a root: root_join("/sysroot", "/etc/x") -> /sysroot/etc/x
pub fn root_join(root: &Path, abs: impl AsRef<Path>) -> PathBuf {
    let abs = abs.as_ref();
    root.join(abs.strip_prefix("/").unwrap_or(abs))
}

pub fn write_file(p: &Path, content: impl AsRef<[u8]>) -> Result<()> {
    if let Some(d) = p.parent() {
        fs::create_dir_all(d).with_context(|| format!("mkdir {}", d.display()))?;
    }
    fs::write(p, content).with_context(|| format!("write {}", p.display()))
}

/// Shell-quote a single word for generated scripts / Containerfiles.
pub fn shq(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:+=@,%".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reboot into a system plexy has just staged, as `--reboot` asked. A desktop
/// session's block inhibitors (gsd-power, gsd-media-keys) make a plain
/// `systemctl reboot` fail with "Operation denied due to active block
/// inhibitor", which would leave the machine running a half-replaced system.
/// Say who held them, then reboot past them; delay inhibitors still run.
pub fn reboot_staged() -> Result<()> {
    if let Some(list) = try_run("systemd-inhibit", &["--list", "--mode=block", "--no-pager"]) {
        let who: Vec<&str> = list
            .lines()
            .skip(1)
            .filter(|l| !l.trim().is_empty() && !l.contains("inhibitors listed"))
            .collect();
        if !who.is_empty() {
            warn(format!(
                "rebooting past {} block inhibitor(s) (the migration is staged):\n{}",
                who.len(),
                who.join("\n")
            ));
        }
    }
    run("systemctl", &["reboot", "--check-inhibitors=no"]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_parsing_strips_quotes_and_comments() {
        let m = parse_kv("# c\nID=fedora\nNAME=\"Fedora Linux\"\nVERSION_ID='44'\n\nBAD\n");
        assert_eq!(m["ID"], "fedora");
        assert_eq!(m["NAME"], "Fedora Linux");
        assert_eq!(m["VERSION_ID"], "44");
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(shq("vim-enhanced"), "vim-enhanced");
        assert_eq!(shq("a b"), "'a b'");
        assert_eq!(shq("it's"), "'it'\\''s'");
        assert_eq!(shq(""), "''");
    }

    #[test]
    fn root_join_strips_leading_slash() {
        assert_eq!(
            root_join(Path::new("/sysroot"), "/etc/passwd"),
            PathBuf::from("/sysroot/etc/passwd")
        );
    }
}
