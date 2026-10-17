//! Package-name mapping between ecosystems.
//!
//! Strategy, in order of confidence:
//!   1. identical name exists in the target repository / formula index
//!   2. curated table of well-known divergences (below)
//!   3. mechanical rules (lib*-dev <-> *-devel, python3-x, ...)
//!   4. "who provides this command": ask the target's package manager which
//!      package ships each binary the source package shipped
//!
//! Anything still unmapped is reported, and in hybrid/toolbox modes it is
//! routed to a container of the *source* distribution, where no mapping is
//! needed at all.

use crate::os::Family;
use std::collections::{BTreeMap, BTreeSet};

/// Debian/Ubuntu name -> Fedora name for packages whose names diverge.
const DEB_TO_RPM: &[(&str, &str)] = &[
    ("build-essential", "gcc gcc-c++ make"),
    ("apache2", "httpd"),
    ("apache2-utils", "httpd-tools"),
    ("openssh-server", "openssh-server"),
    ("openssh-client", "openssh-clients"),
    ("vim", "vim-enhanced"),
    ("vim-gtk3", "vim-X11"),
    ("fd-find", "fd-find"),
    ("bat", "bat"),
    ("golang", "golang"),
    ("golang-go", "golang"),
    ("nodejs", "nodejs"),
    ("npm", "nodejs-npm"),
    ("python3-pip", "python3-pip"),
    ("python3-venv", "python3"),
    ("python-is-python3", "python-unversioned-command"),
    ("default-jdk", "java-latest-openjdk-devel"),
    ("default-jre", "java-latest-openjdk"),
    ("openjdk-21-jdk", "java-21-openjdk-devel"),
    ("openjdk-21-jre", "java-21-openjdk"),
    ("openjdk-17-jdk", "java-17-openjdk-devel"),
    ("docker.io", "moby-engine"),
    ("docker-compose", "docker-compose"),
    ("docker-compose-v2", "docker-compose"),
    ("podman-docker", "podman-docker"),
    ("qemu-kvm", "qemu-kvm"),
    ("qemu-system-x86", "qemu-system-x86"),
    ("libvirt-daemon-system", "libvirt-daemon-kvm"),
    ("libvirt-clients", "libvirt-client"),
    ("virt-manager", "virt-manager"),
    ("network-manager", "NetworkManager"),
    ("network-manager-openvpn-gnome", "NetworkManager-openvpn-gnome"),
    ("wireguard", "wireguard-tools"),
    ("wireguard-tools", "wireguard-tools"),
    ("dnsutils", "bind-utils"),
    ("bind9-dnsutils", "bind-utils"),
    ("bind9", "bind"),
    ("iputils-ping", "iputils"),
    ("netcat-openbsd", "netcat"),
    ("netcat-traditional", "nmap-ncat"),
    ("net-tools", "net-tools"),
    ("traceroute", "traceroute"),
    ("mtr-tiny", "mtr"),
    ("ack", "ack"),
    ("silversearcher-ag", "the_silver_searcher"),
    ("ripgrep", "ripgrep"),
    ("fzf", "fzf"),
    ("gnupg", "gnupg2"),
    ("gnupg2", "gnupg2"),
    ("xz-utils", "xz"),
    ("bzip2", "bzip2"),
    ("p7zip-full", "p7zip p7zip-plugins"),
    ("7zip", "7zip"),
    ("unrar", "unrar"),
    ("cifs-utils", "cifs-utils"),
    ("nfs-common", "nfs-utils"),
    ("nfs-kernel-server", "nfs-utils"),
    ("samba", "samba"),
    ("smbclient", "samba-client"),
    ("postgresql", "postgresql-server"),
    ("postgresql-client", "postgresql"),
    ("mariadb-server", "mariadb-server"),
    ("mariadb-client", "mariadb"),
    ("default-mysql-server", "mariadb-server"),
    ("redis-server", "redis"),
    ("redis-tools", "redis"),
    ("nginx", "nginx"),
    ("nginx-full", "nginx"),
    ("certbot", "certbot"),
    ("cron", "cronie"),
    ("anacron", "cronie-anacron"),
    ("rsyslog", "rsyslog"),
    ("ufw", "firewalld"),
    ("gufw", "firewall-config"),
    ("fonts-firacode", "fira-code-fonts"),
    ("fonts-jetbrains-mono", "jetbrains-mono-fonts"),
    ("fonts-noto", "google-noto-sans-fonts"),
    ("fonts-noto-color-emoji", "google-noto-color-emoji-fonts"),
    ("fonts-liberation", "liberation-fonts"),
    ("ttf-mscorefonts-installer", "mscore-fonts-all"),
    ("gnome-tweaks", "gnome-tweaks"),
    ("gnome-shell-extension-manager", "gnome-extensions-app"),
    ("gnome-shell-extension-appindicator", "gnome-shell-extension-appindicator"),
    ("gnome-shell-extension-dashtodock", "gnome-shell-extension-dash-to-dock"),
    ("gnome-shell-extension-ubuntu-dock", "gnome-shell-extension-dash-to-dock"),
    ("gnome-shell-extension-gsconnect", "gnome-shell-extension-gsconnect"),
    ("gnome-shell-extension-caffeine", "gnome-shell-extension-caffeine"),
    ("gnome-shell-extension-blur-my-shell", "gnome-shell-extension-blur-my-shell"),
    ("kde-config-sddm", "sddm-kcm"),
    ("plasma-discover", "plasma-discover"),
    ("timeshift", "timeshift"),
    ("gparted", "gparted"),
    ("synaptic", ""),
    ("software-properties-common", ""),
    ("software-properties-gtk", ""),
    ("ubuntu-restricted-extras", ""),
    ("ubuntu-desktop", ""),
    ("ubuntu-desktop-minimal", ""),
    ("ubuntu-server", ""),
    ("ubuntu-minimal", ""),
    ("ubuntu-standard", ""),
    ("apt-transport-https", ""),
    ("snapd", ""),
    ("language-pack-en", "langpacks-en"),
    ("locales", "glibc-langpack-en"),
    ("linux-generic", ""),
    ("linux-image-generic", ""),
    ("grub-efi-amd64-signed", ""),
    ("shim-signed", ""),
    ("cloud-init", ""),
    ("libreoffice", "libreoffice"),
    ("firefox-esr", "firefox"),
    ("chromium-browser", "chromium"),
    ("thunderbird", "thunderbird"),
    ("vlc", "vlc"),
    ("mpv", "mpv"),
    ("ffmpeg", "ffmpeg-free"),
    ("gimp", "gimp"),
    ("inkscape", "inkscape"),
    ("keepassxc", "keepassxc"),
    ("steam-installer", "steam"),
    ("steam", "steam"),
    ("zsh", "zsh"),
    ("fish", "fish"),
    ("tmux", "tmux"),
    ("htop", "htop"),
    ("btop", "btop"),
    ("neovim", "neovim"),
    ("emacs", "emacs"),
    ("git", "git"),
    ("git-lfs", "git-lfs"),
    ("gh", "gh"),
    ("jq", "jq"),
    ("curl", "curl"),
    ("wget", "wget2-wget"),
    ("tree", "tree"),
    ("ncdu", "ncdu"),
    ("nmap", "nmap"),
    ("tcpdump", "tcpdump"),
    ("wireshark", "wireshark"),
    ("strace", "strace"),
    ("gdb", "gdb"),
    ("valgrind", "valgrind"),
    ("cmake", "cmake"),
    ("meson", "meson"),
    ("ninja-build", "ninja-build"),
    ("clang", "clang"),
    ("llvm", "llvm"),
    ("rustc", "rust"),
    ("cargo", "cargo"),
    ("libssl-dev", "openssl-devel"),
    ("zlib1g-dev", "zlib-ng-compat-devel"),
    ("libffi-dev", "libffi-devel"),
    ("libsqlite3-dev", "sqlite-devel"),
    ("libreadline-dev", "readline-devel"),
    ("libncurses-dev", "ncurses-devel"),
    ("pkg-config", "pkgconf-pkg-config"),
    ("pkgconf", "pkgconf"),
    ("manpages-dev", "man-pages"),
];

/// Package -> Homebrew formula when the names differ.
const TO_BREW: &[(&str, &str)] = &[
    ("fd-find", "fd"),
    ("golang", "go"),
    ("golang-go", "go"),
    ("golang-bin", "go"),
    ("nodejs", "node"),
    ("nodejs-npm", "node"),
    ("npm", "node"),
    ("vim-enhanced", "vim"),
    ("neovim", "neovim"),
    ("silversearcher-ag", "the_silver_searcher"),
    ("the_silver_searcher", "the_silver_searcher"),
    ("kubernetes-client", "kubernetes-cli"),
    ("kubectl", "kubernetes-cli"),
    ("helm", "helm"),
    ("awscli", "awscli"),
    ("awscli2", "awscli"),
    ("azure-cli", "azure-cli"),
    ("gnupg2", "gnupg"),
    ("bind-utils", "bind"),
    ("dnsutils", "bind"),
    ("iperf3", "iperf3"),
    ("mtr-tiny", "mtr"),
    ("p7zip", "p7zip"),
    ("p7zip-full", "p7zip"),
    ("rust", "rust"),
    ("rustc", "rust"),
    ("cargo", "rust"),
    ("python3-pipx", "pipx"),
    ("pipx", "pipx"),
    ("ShellCheck", "shellcheck"),
    ("shellcheck", "shellcheck"),
    ("hugo", "hugo"),
    ("gh", "gh"),
    ("glab", "glab"),
    ("lazygit", "lazygit"),
    ("ripgrep", "ripgrep"),
    ("git-delta", "git-delta"),
    ("du-dust", "dust"),
    ("eza", "eza"),
    ("exa", "eza"),
    ("ncurses-term", ""),
];

/// Packages that only make sense on the host or are provided by every target:
/// kernels, boot chain, package managers, release metadata.
pub fn is_infrastructure(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "rpm", "dnf", "dnf5", "yum", "apt", "apt-utils", "dpkg", "aptitude", "snapd", "cloud-init",
        "cloud-utils-growpart", "cloud-guest-utils", "grub2", "grub-pc", "grub-common", "shim", "efibootmgr",
        "dracut", "initramfs-tools", "plymouth", "anaconda", "initial-setup", "gnome-initial-setup",
        "systemd", "systemd-udev", "udev", "glibc", "libc6", "filesystem", "base-files", "base-passwd",
        "setup", "basesystem", "bash", "coreutils", "sudo", "passwd", "shadow-utils", "login", "util-linux",
        "ostree", "rpm-ostree", "bootc", "bootupd", "abroot", "ubuntu-pro-client", "ubuntu-advantage-tools",
        "unattended-upgrades", "update-notifier", "update-manager", "ubuntu-release-upgrader-core",
        "dnf-plugins-core", "dnf-automatic", "python3-dnf", "PackageKit", "packagekit", "gnome-software",
        "linux-firmware", "microcode_ctl", "amd-ucode-firmware", "intel-microcode", "amd64-microcode",
        "selinux-policy", "selinux-policy-targeted", "apparmor", "zram-generator-defaults", "qemu-guest-agent",
    ];
    const PREFIX: &[&str] = &[
        "kernel", "linux-image", "linux-headers", "linux-modules", "linux-generic", "linux-signed", "linux-hwe",
        "grub", "shim-", "systemd-boot", "fedora-release", "fedora-repos", "fedora-gpg-keys", "generic-release",
        "ultramarine-release", "ultramarine-repos", "terra-release", "rpmfusion-", "ubuntu-", "debian-archive",
        "lsb-release", "os-prober", "firmware-", "akmod", "kmod-", "dkms", "langpacks-core", "dracut-",
        "anaconda-", "cloud-init", "ec2-", "google-guest", "walinuxagent", "WALinuxAgent", "vanilla-",
        "abroot", "ublue-", "bluefin-",
    ];
    EXACT.contains(&name) || PREFIX.iter().any(|p| name.starts_with(p)) || name.ends_with("-release")
}

/// Flathub app id for a desktop application package (same names as their
/// snaps for the common apps, plus distro package names that differ).
pub fn app_flatpak(pkg: &str) -> Option<&'static str> {
    let alias = match pkg {
        "vlc" | "vlc-gui-qt" => "vlc",
        "chromium-browser" => "chromium",
        "keepassxc" => "keepassxc",
        "libreoffice-writer" | "libreoffice-calc" | "libreoffice-impress" | "libreoffice-core" => "libreoffice",
        "obs-studio" => "obs-studio",
        "telegram-desktop" => "telegram-desktop",
        "thunderbird" => "thunderbird",
        "steam" | "steam-installer" => "steam",
        "gimp" => "gimp",
        "inkscape" => "inkscape",
        "krita" => "krita",
        "kdenlive" => "kdenlive",
        "audacity" => "audacity",
        "blender" => "blender",
        "mpv" => "mpv",
        "discord" => "discord",
        "code" => "code",
        _ => return None,
    };
    crate::state::snap_to_flatpak(alias)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Mapped {
    /// Names to install in the target (may be several, may be empty = "drop").
    pub names: Vec<String>,
    pub how: &'static str,
}

/// Map a source package name into the target family.
pub fn map_package(name: &str, from: Family, to: Family, available: &BTreeSet<String>) -> Option<Mapped> {
    let ok = |n: &str| available.is_empty() || available.contains(n);
    if from == to {
        return Some(Mapped { names: vec![name.to_string()], how: "same-name" });
    }
    let table: Vec<(&str, &str)> = match (from, to) {
        (Family::Debian, Family::Fedora) => DEB_TO_RPM.to_vec(),
        (Family::Fedora, Family::Debian) => DEB_TO_RPM.iter().map(|(d, r)| (*r, *d)).collect(),
        _ => vec![],
    };
    if let Some((_, t)) = table.iter().find(|(s, _)| *s == name) {
        let names: Vec<String> = t.split_whitespace().map(str::to_string).collect();
        return Some(Mapped { names, how: "curated" });
    }
    if ok(name) && !available.is_empty() {
        return Some(Mapped { names: vec![name.to_string()], how: "same-name" });
    }
    if let Some(r) = rule_map(name, from, to)
        && ok(&r) {
            return Some(Mapped { names: vec![r], how: "rule" });
        }
    None
}

fn rule_map(name: &str, from: Family, to: Family) -> Option<String> {
    match (from, to) {
        (Family::Debian, Family::Fedora) => {
            if let Some(base) = name.strip_suffix("-dev") {
                let base = base.strip_prefix("lib").unwrap_or(base);
                // libfoo1.2-dev -> foo-devel
                let base = base.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.' || c == '-');
                return Some(format!("{base}-devel"));
            }
            if let Some(m) = name.strip_prefix("python3-") {
                return Some(format!("python3-{m}"));
            }
            if let Some(f) = name.strip_prefix("fonts-") {
                return Some(format!("{f}-fonts"));
            }
            None
        }
        (Family::Fedora, Family::Debian) => {
            if let Some(base) = name.strip_suffix("-devel") {
                return Some(format!("lib{base}-dev"));
            }
            if let Some(f) = name.strip_suffix("-fonts") {
                return Some(format!("fonts-{f}"));
            }
            None
        }
        _ => None,
    }
}

/// Look up the Homebrew formula for a package, given the formula index
/// (name/alias -> canonical formula).
pub fn brew_formula(name: &str, index: &BTreeMap<String, String>) -> Option<String> {
    if let Some((_, f)) = TO_BREW.iter().find(|(p, _)| *p == name) {
        if f.is_empty() {
            return None;
        }
        return index.get(*f).cloned().or_else(|| index.is_empty().then(|| f.to_string()));
    }
    index.get(name).cloned()
}

/// Parse the Homebrew formula API JSON (https://formulae.brew.sh/api/formula.json)
/// into name/alias/oldname -> canonical name, keeping only formulae with a Linux bottle
/// or source build support.
pub fn parse_brew_index(json: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return m };
    for f in v.as_array().into_iter().flatten() {
        let Some(name) = f.get("name").and_then(|x| x.as_str()) else { continue };
        if f.get("disabled").and_then(|x| x.as_bool()) == Some(true) {
            continue;
        }
        let linux = f
            .pointer("/bottle/stable/files")
            .and_then(|x| x.as_object())
            .map(|o| o.keys().any(|k| k.contains("linux")))
            .unwrap_or(true);
        if !linux {
            continue;
        }
        m.insert(name.to_string(), name.to_string());
        for key in ["aliases", "oldnames"] {
            for a in f.get(key).and_then(|x| x.as_array()).into_iter().flatten() {
                if let Some(a) = a.as_str() {
                    m.entry(a.to_string()).or_insert_with(|| name.to_string());
                }
            }
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn same_family_is_identity() {
        let m = map_package("htop", Family::Fedora, Family::Fedora, &BTreeSet::new()).unwrap();
        assert_eq!(m.names, vec!["htop"]);
    }

    #[test]
    fn curated_debian_to_fedora() {
        let m = map_package("build-essential", Family::Debian, Family::Fedora, &BTreeSet::new()).unwrap();
        assert_eq!(m.names, vec!["gcc", "gcc-c++", "make"]);
        assert_eq!(m.how, "curated");
        let drop = map_package("snapd", Family::Debian, Family::Fedora, &BTreeSet::new()).unwrap();
        assert!(drop.names.is_empty());
    }

    #[test]
    fn rules_dev_packages() {
        let avail = set(&["openssl-devel", "yaml-devel", "foo"]);
        let m = map_package("libyaml-dev", Family::Debian, Family::Fedora, &avail).unwrap();
        assert_eq!(m.names, vec!["yaml-devel"]);
        assert!(map_package("libnotreal-dev", Family::Debian, Family::Fedora, &avail).is_none());
        let m = map_package("foo", Family::Debian, Family::Fedora, &avail).unwrap();
        assert_eq!(m.how, "same-name");
    }

    #[test]
    fn reverse_table_fedora_to_debian() {
        let m = map_package("httpd", Family::Fedora, Family::Debian, &BTreeSet::new()).unwrap();
        assert_eq!(m.names, vec!["apache2"]);
    }

    #[test]
    fn infrastructure_detection() {
        for n in ["kernel-core", "linux-image-generic", "grub2-efi-x64", "fedora-release-cloud", "dnf5", "cloud-init"] {
            assert!(is_infrastructure(n), "{n}");
        }
        for n in ["htop", "nginx", "tmux", "gnome-tweaks"] {
            assert!(!is_infrastructure(n), "{n}");
        }
    }

    #[test]
    fn desktop_apps_to_flathub() {
        assert_eq!(app_flatpak("vlc"), Some("org.videolan.VLC"));
        assert_eq!(app_flatpak("libreoffice-writer"), Some("org.libreoffice.LibreOffice"));
        assert_eq!(app_flatpak("htop"), None);
    }

    #[test]
    fn brew_index_and_lookup() {
        let js = r#"[{"name":"fd","aliases":["fd-find"],"oldnames":[],"bottle":{"stable":{"files":{"x86_64_linux":{}}}}},
                     {"name":"macos-only","aliases":[],"bottle":{"stable":{"files":{"arm64_sonoma":{}}}}},
                     {"name":"htop","aliases":[]}]"#;
        let idx = parse_brew_index(js);
        assert_eq!(brew_formula("fd-find", &idx).as_deref(), Some("fd"));
        assert_eq!(brew_formula("htop", &idx).as_deref(), Some("htop"));
        assert_eq!(brew_formula("macos-only", &idx), None);
        assert_eq!(brew_formula("nginx", &idx), None);
    }
}
