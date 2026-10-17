//! Machine state that is not a package: accounts, identity, services, and the
//! user-space package managers ("wrappers") that live in /var and $HOME.

use crate::util::{parse_kv, read_opt, try_run};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PasswdEntry {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub gecos: String,
    pub home: String,
    pub shell: String,
}

impl PasswdEntry {
    pub fn parse(line: &str) -> Option<PasswdEntry> {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 7 || line.starts_with('#') {
            return None;
        }
        Some(PasswdEntry {
            name: f[0].into(),
            uid: f[2].parse().ok()?,
            gid: f[3].parse().ok()?,
            gecos: f[4].into(),
            home: f[5].into(),
            shell: f[6].into(),
        })
    }
    pub fn to_line(&self) -> String {
        format!("{}:x:{}:{}:{}:{}:{}", self.name, self.uid, self.gid, self.gecos, self.home, self.shell)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GroupEntry {
    pub name: String,
    pub gid: u32,
    pub members: Vec<String>,
}

impl GroupEntry {
    pub fn parse(line: &str) -> Option<GroupEntry> {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 4 || line.starts_with('#') {
            return None;
        }
        Some(GroupEntry {
            name: f[0].into(),
            gid: f[2].parse().ok()?,
            members: f[3].split(',').filter(|m| !m.is_empty()).map(str::to_string).collect(),
        })
    }
    pub fn to_line(&self) -> String {
        format!("{}:x:{}:{}", self.name, self.gid, self.members.join(","))
    }
}

pub fn parse_passwd(text: &str) -> Vec<PasswdEntry> {
    text.lines().filter_map(PasswdEntry::parse).collect()
}
pub fn parse_group(text: &str) -> Vec<GroupEntry> {
    text.lines().filter_map(GroupEntry::parse).collect()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Accounts {
    /// Every account (system + human) with its uid, for ownership remapping.
    pub all_users: Vec<PasswdEntry>,
    pub all_groups: Vec<GroupEntry>,
    /// Human accounts (UID_MIN..=UID_MAX) that will be recreated.
    pub humans: Vec<PasswdEntry>,
    /// Groups each human belongs to (supplementary).
    pub memberships: BTreeMap<String, Vec<String>>,
}

pub fn login_defs_range(root: &Path) -> (u32, u32) {
    let t = read_opt(root.join("etc/login.defs")).unwrap_or_default();
    let mut min = 1000;
    let mut max = 60000;
    for l in t.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() == 2 {
            match f[0] {
                "UID_MIN" => min = f[1].parse().unwrap_or(min),
                "UID_MAX" => max = f[1].parse().unwrap_or(max),
                _ => {}
            }
        }
    }
    (min, max)
}

pub fn scan_accounts(root: &Path) -> Accounts {
    let users = parse_passwd(&read_opt(root.join("etc/passwd")).unwrap_or_default());
    let groups = parse_group(&read_opt(root.join("etc/group")).unwrap_or_default());
    let (min, max) = login_defs_range(root);
    let humans: Vec<PasswdEntry> = users
        .iter()
        .filter(|u| u.uid >= min && u.uid <= max && u.name != "nobody")
        .cloned()
        .collect();
    let mut memberships = BTreeMap::new();
    for h in &humans {
        let gs: Vec<String> =
            groups.iter().filter(|g| g.members.contains(&h.name)).map(|g| g.name.clone()).collect();
        memberships.insert(h.name.clone(), gs);
    }
    Accounts { all_users: users, all_groups: groups, humans, memberships }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemSettings {
    pub hostname: Option<String>,
    pub timezone: Option<String>,
    pub locale: BTreeMap<String, String>,
    pub keymap: Option<String>,
    /// Every enabled unit. The target's presets differ from the source's
    /// (Silverblue ships sshd disabled, Fedora Cloud enabled), so the source's
    /// *state* is replicated, not its deviation from presets.
    pub enabled_units: Vec<String>,
    /// Units the admin disabled although the vendor preset enables them.
    #[serde(default)]
    pub disabled_units: Vec<String>,
    pub default_target: Option<String>,
}

pub fn scan_system(root: &Path) -> SystemSettings {
    let mut s = SystemSettings {
        hostname: read_opt(root.join("etc/hostname")).map(|h| h.trim().to_string()).filter(|h| !h.is_empty()),
        ..Default::default()
    };
    if let Ok(t) = fs::read_link(root.join("etc/localtime")) {
        let t = t.display().to_string();
        if let Some(i) = t.find("zoneinfo/") {
            s.timezone = Some(t[i + 9..].to_string());
        }
    }
    // Fedora: /etc/locale.conf, Debian: /etc/default/locale
    for p in ["etc/locale.conf", "etc/default/locale"] {
        if let Some(t) = read_opt(root.join(p)) {
            s.locale.extend(parse_kv(&t));
        }
    }
    if let Some(t) = read_opt(root.join("etc/vconsole.conf")) {
        s.keymap = parse_kv(&t).get("KEYMAP").cloned();
    }
    if s.keymap.is_none()
        && let Some(t) = read_opt(root.join("etc/default/keyboard")) {
            s.keymap = parse_kv(&t).get("XKBLAYOUT").cloned();
        }
    if root == Path::new("/") {
        if let Some(out) =
            try_run("systemctl", &["list-unit-files", "--state=enabled,disabled", "--no-legend", "--no-pager"])
        {
            let (en, dis) = parse_unit_files(&out);
            s.enabled_units = en;
            s.disabled_units = dis;
        }
        s.default_target = try_run("systemctl", &["get-default"]).map(|t| t.trim().to_string());
    }
    s
}

/// Parse `systemctl list-unit-files` rows (UNIT STATE PRESET): every enabled
/// unit, and units disabled against an "enabled" preset. Template instances
/// and units that only exist for other units' sake are skipped.
pub fn parse_unit_files(out: &str) -> (Vec<String>, Vec<String>) {
    let mut en = vec![];
    let mut dis = vec![];
    for l in out.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 2 || f[0].contains("@.") {
            continue;
        }
        match (f[1], f.get(2).copied()) {
            ("enabled", _) => en.push(f[0].to_string()),
            ("disabled", Some("enabled")) => dis.push(f[0].to_string()),
            _ => {}
        }
    }
    (en, dis)
}

// ------------------------------------------------------------ wrappers ------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Wrappers {
    pub flatpak: Option<FlatpakState>,
    pub homebrew: Option<BrewState>,
    pub snaps: Vec<SnapApp>,
    pub nix: bool,
    /// distrobox / toolbox containers per user (they live in $HOME and move with it).
    pub containers: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FlatpakState {
    pub system_remotes: Vec<String>,
    pub system_apps: Vec<String>,
    /// user -> app ids installed per-user (in ~/.local/share/flatpak).
    pub user_apps: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BrewState {
    pub prefix: String,
    pub owner: Option<String>,
    pub formulae: Vec<String>,
    pub casks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapApp {
    pub name: String,
    /// Suggested Flatpak replacement (snaps do not run on image-based Fedora).
    pub flatpak: Option<String>,
}

pub fn scan_wrappers(accounts: &Accounts) -> Wrappers {
    let mut w = Wrappers::default();

    if Path::new("/var/lib/flatpak").exists() || crate::util::have("flatpak") {
        let mut f = FlatpakState::default();
        if let Some(o) = try_run("flatpak", &["remotes", "--system", "--columns=name"]) {
            f.system_remotes = o.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
        }
        if let Some(o) = try_run("flatpak", &["list", "--system", "--app", "--columns=application"]) {
            f.system_apps = o.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
        }
        for h in &accounts.humans {
            let dir = Path::new(&h.home).join(".local/share/flatpak/app");
            if let Ok(rd) = fs::read_dir(&dir) {
                let apps: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
                if !apps.is_empty() {
                    f.user_apps.insert(h.name.clone(), apps);
                }
            }
        }
        w.flatpak = Some(f);
    }

    for prefix in ["/home/linuxbrew/.linuxbrew", "/var/home/linuxbrew/.linuxbrew"] {
        let p = Path::new(prefix);
        if p.join("bin/brew").exists() {
            let cellar = |d: &str| -> Vec<String> {
                fs::read_dir(p.join(d))
                    .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
                    .unwrap_or_default()
            };
            let owner = std::os::unix::fs::MetadataExt::uid(&fs::metadata(p).unwrap());
            w.homebrew = Some(BrewState {
                prefix: prefix.into(),
                owner: accounts.all_users.iter().find(|u| u.uid == owner).map(|u| u.name.clone()),
                formulae: cellar("Cellar"),
                casks: cellar("Caskroom"),
            });
            break;
        }
    }

    if let Some(o) = try_run("snap", &["list"]) {
        for l in o.lines().skip(1) {
            let name = l.split_whitespace().next().unwrap_or("").to_string();
            if name.is_empty() || is_snap_infrastructure(&name) {
                continue;
            }
            let flatpak = snap_to_flatpak(&name).map(str::to_string);
            w.snaps.push(SnapApp { name, flatpak });
        }
    }

    w.nix = Path::new("/nix/store").exists();

    for h in &accounts.humans {
        let mut names = vec![];
        let containers = Path::new(&h.home).join(".local/share/containers/storage/overlay-containers/containers.json");
        if let Some(t) = read_opt(&containers)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                for c in v.as_array().into_iter().flatten() {
                    if let Some(n) = c.pointer("/names/0").and_then(|x| x.as_str()) {
                        names.push(n.to_string());
                    }
                }
            }
        if !names.is_empty() {
            w.containers.insert(h.name.clone(), names);
        }
    }
    w
}

fn is_snap_infrastructure(n: &str) -> bool {
    n == "snapd" || n == "bare" || n.starts_with("core") || n.starts_with("gnome-") && n.contains("-2")
        || n.starts_with("gtk-common-themes") || n.starts_with("mesa-") || n.starts_with("kf5-") || n.starts_with("kf6-")
        || n == "snapd-desktop-integration" || n == "firmware-updater" || n == "desktop-security-center"
        || n == "prompting-client"
}

/// Curated snap -> Flathub mapping for common desktop apps.
pub fn snap_to_flatpak(snap: &str) -> Option<&'static str> {
    Some(match snap {
        "firefox" => "org.mozilla.firefox",
        "thunderbird" => "org.mozilla.Thunderbird",
        "chromium" => "org.chromium.Chromium",
        "code" => "com.visualstudio.code",
        "codium" => "com.vscodium.codium",
        "spotify" => "com.spotify.Client",
        "slack" => "com.slack.Slack",
        "discord" => "com.discordapp.Discord",
        "telegram-desktop" => "org.telegram.desktop",
        "signal-desktop" => "org.signal.Signal",
        "vlc" => "org.videolan.VLC",
        "gimp" => "org.gimp.GIMP",
        "inkscape" => "org.inkscape.Inkscape",
        "libreoffice" => "org.libreoffice.LibreOffice",
        "obs-studio" => "com.obsproject.Studio",
        "zoom-client" => "us.zoom.Zoom",
        "postman" => "com.getpostman.Postman",
        "intellij-idea-community" => "com.jetbrains.IntelliJ-IDEA-Community",
        "pycharm-community" => "com.jetbrains.PyCharm-Community",
        "android-studio" => "com.google.AndroidStudio",
        "snap-store" => "org.gnome.Software",
        "steam" => "com.valvesoftware.Steam",
        "blender" => "org.blender.Blender",
        "kdenlive" => "org.kde.kdenlive",
        "audacity" => "org.audacityteam.Audacity",
        "bitwarden" => "com.bitwarden.desktop",
        "keepassxc" => "org.keepassxc.KeePassXC",
        "obsidian" => "md.obsidian.Obsidian",
        "element-desktop" => "im.riot.Riot",
        "teams-for-linux" => "com.github.IsmaelMartinez.teams_for_linux",
        "gnome-calculator" => "org.gnome.Calculator",
        "gnome-text-editor" => "org.gnome.TextEditor",
        "loupe" => "org.gnome.Loupe",
        "mpv" => "io.mpv.Mpv",
        "krita" => "org.kde.krita",
        "foliate" => "com.github.johnfactotum.Foliate",
        "drawio" => "com.jgraph.drawio.desktop",
        "dbeaver-ce" => "io.dbeaver.DBeaverCommunity",
        "brave" => "com.brave.Browser",
        "opera" => "com.opera.Opera",
        "skype" => "com.skype.Client",
        "mattermost-desktop" => "com.mattermost.Desktop",
        "notepad-plus-plus" => return None,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwd_roundtrip() {
        let l = "alice:x:1000:1000:Alice A:/home/alice:/bin/bash";
        let e = PasswdEntry::parse(l).unwrap();
        assert_eq!(e.uid, 1000);
        assert_eq!(e.to_line(), l);
        assert!(PasswdEntry::parse("# comment").is_none());
        assert!(PasswdEntry::parse("broken:x").is_none());
    }

    #[test]
    fn group_roundtrip() {
        let g = GroupEntry::parse("wheel:x:10:alice,bob").unwrap();
        assert_eq!(g.members, vec!["alice", "bob"]);
        assert_eq!(g.to_line(), "wheel:x:10:alice,bob");
        assert!(GroupEntry::parse("empty:x:5:").unwrap().members.is_empty());
    }

    #[test]
    fn humans_and_memberships() {
        let dir = std::env::temp_dir().join(format!("plexy-acct-{}", std::process::id()));
        fs::create_dir_all(dir.join("etc")).unwrap();
        fs::write(
            dir.join("etc/passwd"),
            "root:x:0:0:root:/root:/bin/bash\nalice:x:1000:1000::/home/alice:/bin/bash\n\
             nobody:x:65534:65534::/:/sbin/nologin\nsvc:x:990:990::/:/sbin/nologin\n",
        )
        .unwrap();
        fs::write(dir.join("etc/group"), "wheel:x:10:alice\nsudo:x:27:\nalice:x:1000:\n").unwrap();
        let a = scan_accounts(&dir);
        assert_eq!(a.humans.len(), 1);
        assert_eq!(a.memberships["alice"], vec!["wheel"]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unit_file_parsing() {
        let out = "sshd.service enabled enabled\nnginx.service enabled disabled\n\
                   cups.service disabled enabled\nfoo@.service enabled enabled\nbar.service disabled disabled\n";
        let (en, dis) = parse_unit_files(out);
        assert_eq!(en, vec!["sshd.service", "nginx.service"]);
        assert_eq!(dis, vec!["cups.service"]);
    }

    #[test]
    fn snap_mapping() {
        assert_eq!(snap_to_flatpak("firefox"), Some("org.mozilla.firefox"));
        assert_eq!(snap_to_flatpak("nonexistent-snap"), None);
        assert!(is_snap_infrastructure("core22"));
        assert!(is_snap_infrastructure("gnome-42-2204"));
        assert!(!is_snap_infrastructure("gnome-calculator"));
    }
}
