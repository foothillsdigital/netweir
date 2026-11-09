//! `netweir install chrome`: the Chrome for Testing build that matches
//! netweir's newest Chrome profile, unpacked where `find_chrome` looks.

use std::path::{Path, PathBuf};
use std::time::Duration;

use netweir_browser::{cft_executable, cft_platform};
use serde_json::Value;
use sha2::Digest;

use crate::fetch::{FetchOptions, Fetcher};
use crate::profile::{DEFAULT, Profile};

/// Google's list of the newest build of each Chrome milestone.
pub const CHROME_FOR_TESTING: &str = "https://googlechromelabs.github.io/chrome-for-testing/latest-versions-per-milestone-with-downloads.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub version: String,
    pub executable: PathBuf,
    /// It was there already; nothing was downloaded.
    pub already: bool,
}

/// Installs the Chrome for Testing build (or, with `headless_shell`,
/// `chrome-headless-shell`) of netweir's newest Chrome profile's
/// milestone, as `index` lists it, into `home/chrome/<version>/`. A build
/// already there isn't downloaded again.
pub async fn install_chrome(
    index: &str,
    home: &Path,
    headless_shell: bool,
) -> Result<Installed, String> {
    let platform = cft_platform().ok_or_else(|| {
        format!(
            "no Chrome for Testing builds for {} on {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let milestone = DEFAULT
        .split('-')
        .nth(1)
        .ok_or("netweir's default profile has no version in its name")?;
    let mut options = FetchOptions::new(Profile::named("chrome").map_err(|e| e.to_string())?);
    // A download is a few hundred megabytes.
    options.timeout = Duration::from_secs(600);
    let fetcher = Fetcher::new(options).map_err(|e| e.message)?;

    let listing = fetcher
        .get(index, &[])
        .await
        .map_err(|e| format!("{index}: {}", e.message))?;
    if listing.status != 200 {
        return Err(format!("{index}: HTTP {}", listing.status));
    }
    let listing: Value =
        serde_json::from_slice(&listing.body).map_err(|e| format!("{index}: {e}"))?;
    let build = &listing["milestones"][milestone];
    let version = build["version"]
        .as_str()
        .ok_or_else(|| format!("{index} has no Chrome {milestone}"))?
        .to_string();
    // It becomes a folder name, and find_chrome reads only dotted numbers.
    if version.is_empty()
        || !version
            .split('.')
            .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(format!("{index} lists {version:?} as a version"));
    }
    let kind = if headless_shell {
        "chrome-headless-shell"
    } else {
        "chrome"
    };
    let downloads = build["downloads"][kind]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let url = downloads
        .iter()
        .find(|d| d["platform"] == platform)
        .and_then(|d| d["url"].as_str())
        .ok_or_else(|| {
            let there: Vec<&str> = downloads
                .iter()
                .filter_map(|d| d["platform"].as_str())
                .collect();
            format!(
                "no Chrome for Testing {kind} {version} for {platform}; there is for: {}",
                if there.is_empty() {
                    "nothing".to_string()
                } else {
                    there.join(", ")
                }
            )
        })?
        .to_string();

    let dir = home.join("chrome").join(&version);
    let executable = dir.join(cft_executable(platform, headless_shell));
    if executable.is_file() {
        // One installed before netweir granted the sandbox access gets it.
        netweir_browser::grant_sandbox_access(&dir)?;
        return Ok(Installed {
            version,
            executable,
            already: true,
        });
    }
    let zip = fetcher
        .get(&url, &[])
        .await
        .map_err(|e| format!("{url}: {}", e.message))?;
    if zip.status != 200 {
        return Err(format!("{url}: HTTP {}", zip.status));
    }
    // Unpacked beside its final place and moved in whole, so a download
    // cut short never looks installed.
    let prefix = format!(".{version}-{kind}-");
    sweep_partials(&home.join("chrome"), &prefix);
    let partial = home
        .join("chrome")
        .join(format!("{prefix}{}", std::process::id()));
    let unpacked = tokio::task::spawn_blocking({
        let partial = partial.clone();
        move || -> Result<(), String> {
            std::fs::create_dir_all(&partial).map_err(|e| e.to_string())?;
            let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip.body))
                .map_err(|e| format!("not a zip: {e}"))?;
            archive.extract(&partial).map_err(|e| e.to_string())
        }
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);
    let moved = unpacked.and_then(|()| move_into(&partial, &dir));
    let _ = std::fs::remove_dir_all(&partial);
    moved.map_err(|e| format!("unpacking {url} into {}: {e}", dir.display()))?;
    if !executable.is_file() {
        return Err(format!("{url} didn't hold {}", executable.display()));
    }
    netweir_browser::grant_sandbox_access(&dir)?;
    Ok(Installed {
        version,
        executable,
        already: false,
    })
}

/// Removes what an install that was killed left behind in `root`: folders
/// named with `prefix`. Not one under way now, so only those an hour old.
fn sweep_partials(root: &Path, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(3600));
        if old && entry.file_name().to_string_lossy().starts_with(prefix) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Moves what was unpacked into `partial` into `dir`, entry by entry.
fn move_into(partial: &Path, dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(partial).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let to = dir.join(entry.file_name());
        // Ours is complete; one already there isn't, or the executable
        // check before the download would have found it.
        if to.exists() {
            std::fs::remove_dir_all(&to).map_err(|e| e.to_string())?;
        }
        std::fs::rename(entry.path(), &to).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Where Mozilla keeps every Firefox release, by version.
pub const FIREFOX_RELEASES: &str = "https://archive.mozilla.org/pub/firefox/releases";

/// A Firefox release Netweir installs, and the SHA-256 of its download
/// for each platform, as Mozilla's signed SHA256SUMS lists them.
#[derive(Debug, Clone, Copy)]
pub struct FirefoxRelease<'a> {
    pub version: &'a str,
    /// (platform as Mozilla names it, lowercase hex SHA-256).
    pub sums: &'a [(&'a str, &'a str)],
}

/// The Firefox Netweir installs. Moved on with each release, once the
/// Firefox tests pass on it; the sums come from that release's
/// SHA256SUMS, after checking its signature.
pub const FIREFOX: FirefoxRelease<'static> = FirefoxRelease {
    version: "159.0.1",
    sums: &[
        (
            "mac",
            "0c214da4439c1eb4cea28a3c141ff112d511aa32c67f9d12e91116569c8d8dda",
        ),
        (
            "linux-x86_64",
            "469ce17120018e1c95411d254cf3c5f8db2dd1768a7acde5a2a089b00072ac65",
        ),
        (
            "linux-aarch64",
            "847651efd44351972a281afa54845708faffb358692f551af391dd77736bbd6b",
        ),
        (
            "win64",
            "729af643b8e0a3809d6153a57a448cb96a01e6fa5e2e7ff782152ede6202eadd",
        ),
        (
            "win64-aarch64",
            "c4e5f2440df619b968aadb87db4f6948fcda23e24d8f6c211070bd7fbfe2c2b2",
        ),
    ],
};

/// Installs `release`'s US English build from `base` (laid out as
/// [`FIREFOX_RELEASES`]) into `home/firefox/<version>/`, then writes the
/// startup files that let Netweir drive it without the automation flag.
/// The download must match its pinned SHA-256 before anything is
/// unpacked. A build already there isn't downloaded again.
pub async fn install_firefox(
    base: &str,
    home: &Path,
    release: &FirefoxRelease<'_>,
) -> Result<Installed, String> {
    let version = release.version;
    let number = |n: &str| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
    // A release such as 159.0.1, or a beta such as 160.0b5, which the
    // weekly Firefox Beta check installs.
    let mut parts: Vec<&str> = version.split('.').collect();
    let last = parts.pop().unwrap_or_default();
    let last_ok = number(last)
        || last
            .split_once('b')
            .is_some_and(|(minor, beta)| number(minor) && number(beta));
    if !last_ok || !parts.iter().all(|n| number(n)) {
        return Err(format!("{version:?} isn't a Firefox release version"));
    }
    let platform = netweir_browser::firefox_platform().ok_or_else(|| {
        format!(
            "no Firefox builds for {} on {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let sum = release
        .sums
        .iter()
        .find(|(p, _)| *p == platform)
        .map(|(_, sum)| *sum)
        .ok_or_else(|| format!("Netweir has no checksum for Firefox {version} on {platform}"))?;
    let root = home.join("firefox");
    let dir = root.join(version);
    let executable = netweir_browser::firefox_executable(&dir);
    if executable.is_file() {
        // One from an older Netweir gets this one's startup files.
        netweir_browser::prepare_firefox(&executable)?;
        return Ok(Installed {
            version: version.to_string(),
            executable,
            already: true,
        });
    }
    let file = if platform == "mac" {
        format!("Firefox {version}.dmg")
    } else if platform.starts_with("win") {
        format!("Firefox Setup {version}.exe")
    } else {
        format!("firefox-{version}.tar.xz")
    };
    let url = format!(
        "{base}/{version}/{platform}/en-US/{}",
        file.replace(' ', "%20")
    );
    let mut options = FetchOptions::new(Profile::named("firefox").map_err(|e| e.to_string())?);
    // A download is over a hundred megabytes.
    options.timeout = Duration::from_secs(600);
    let fetcher = Fetcher::new(options).map_err(|e| e.message)?;
    let download = fetcher
        .get(&url, &[])
        .await
        .map_err(|e| format!("{url}: {}", e.message))?;
    if download.status != 200 {
        return Err(format!("{url}: HTTP {}", download.status));
    }
    let got = hex(&sha2::Sha256::digest(&download.body));
    if got != sum {
        return Err(format!(
            "{url} has SHA-256 {got}, not the {sum} Mozilla published for it; nothing was installed"
        ));
    }

    let prefix = format!(".{version}-firefox-");
    sweep_partials(&root, &prefix);
    let partial = root.join(format!("{prefix}{}", std::process::id()));
    let unpacked = tokio::task::spawn_blocking({
        let partial = partial.clone();
        move || unpack_firefox(&download.body, &file, &partial)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);
    let moved = unpacked.and_then(|()| move_into(&partial, &dir));
    let _ = std::fs::remove_dir_all(&partial);
    moved.map_err(|e| format!("unpacking {url} into {}: {e}", dir.display()))?;
    if !executable.is_file() {
        return Err(format!("{url} didn't hold {}", executable.display()));
    }
    netweir_browser::prepare_firefox(&executable)?;
    Ok(Installed {
        version: version.to_string(),
        executable,
        already: false,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Unpacks Mozilla's download, named `file`, into `partial`, with the
/// system's own tools: the disk image is mounted and its app copied out;
/// the tarball is untarred; the Windows installer, already checked, is
/// run only to extract itself, so nothing is installed into Program Files.
fn unpack_firefox(archive: &[u8], file: &str, partial: &Path) -> Result<(), String> {
    use std::process::{Command, Stdio};

    std::fs::create_dir_all(partial).map_err(|e| e.to_string())?;
    // Beside the folder, not in it, so only what's unpacked is moved in.
    // The folder's name has dots in it, so not with_extension.
    let beside = |suffix: &str| {
        let mut name = partial.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    let downloaded = beside(match Path::new(file).extension().and_then(|e| e.to_str()) {
        Some("dmg") => ".dmg",
        Some("exe") => ".exe",
        _ => ".tar.xz",
    });
    std::fs::write(&downloaded, archive).map_err(|e| e.to_string())?;
    let run = |command: &mut Command| -> Result<(), String> {
        let out = command
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("can't run {:?}: {e}", command.get_program()))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "{:?} failed ({}): {}",
                command.get_program(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    };
    let done = if file.ends_with(".dmg") {
        let mount = beside(".mount");
        run(Command::new("hdiutil")
            .args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-quiet"])
            .arg("-mountpoint")
            .arg(&mount)
            .arg(&downloaded))
        .and_then(|()| {
            // ditto keeps the app's links and its signature's attributes.
            let copied = run(Command::new("ditto")
                .arg(mount.join("Firefox.app"))
                .arg(partial.join("Firefox.app")));
            let detached = run(Command::new("hdiutil")
                .args(["detach", "-quiet", "-force"])
                .arg(&mount));
            copied.and(detached)
        })
    } else if file.ends_with(".exe") {
        let mut extract = std::ffi::OsString::from("/ExtractDir=");
        extract.push(partial);
        run(Command::new(&downloaded).arg(extract))
    } else {
        run(Command::new("tar")
            .arg("-xJf")
            .arg(&downloaded)
            .arg("-C")
            .arg(partial))
    };
    let _ = std::fs::remove_file(&downloaded);
    done
}
