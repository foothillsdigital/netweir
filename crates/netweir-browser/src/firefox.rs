//! Finding a Firefox `netweir install firefox` installed, and preparing it
//! to start without the automation flag (see firefox/mozilla.cfg).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::LaunchOptions;
use crate::conn::{Connection, Dialect};
use crate::launch::{Process, Said, TempProfile, netweir_home};
use crate::{Error, Result};

/// Firefox reads it from its `defaults/pref` folder and is told by it to
/// read `mozilla.cfg`, unsandboxed.
const AUTOCONFIG: &str = include_str!("firefox/autoconfig.js");

/// What a Netweir Firefox runs at start. Its first line, which Firefox
/// skips, marks the install as Netweir's.
const MOZILLA_CFG: &str = include_str!("firefox/mozilla.cfg");

const MARK: &str = "// netweir";

/// This machine as Mozilla's release archive names platforms, if it has
/// builds for it.
pub fn firefox_platform() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", _) => Some("mac"),
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("linux", "aarch64") => Some("linux-aarch64"),
        ("windows", "x86_64") => Some("win64"),
        ("windows", "aarch64") => Some("win64-aarch64"),
        _ => None,
    }
}

/// Where the executable is in an install folder, as Mozilla's archive
/// unpacks: the app from the disk image on macOS, the tarball's `firefox`
/// folder on Linux, and what the installer's `/ExtractDir=` leaves on
/// Windows.
pub fn firefox_executable(dir: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        dir.join("Firefox.app/Contents/MacOS/firefox")
    } else if cfg!(windows) {
        dir.join("core/firefox.exe")
    } else {
        dir.join("firefox/firefox")
    }
}

/// The folder Firefox reads `mozilla.cfg` and `defaults/pref` from: the
/// executable's own, or on macOS the app's Resources.
fn resources_of(executable: &Path) -> Option<PathBuf> {
    let dir = executable.parent()?;
    Some(if cfg!(target_os = "macos") {
        dir.parent()?.join("Resources")
    } else {
        dir.to_path_buf()
    })
}

/// Writes the startup files into the Firefox at `executable`, where they
/// differ from Netweir's own: an install from an older Netweir is brought
/// up to date. Only ever called for a Firefox Netweir installed.
pub fn prepare_firefox(executable: &Path) -> std::result::Result<(), String> {
    let resources = resources_of(executable)
        .ok_or_else(|| format!("{} isn't in a Firefox install", executable.display()))?;
    for (path, contents) in [
        (resources.join("defaults/pref/autoconfig.js"), AUTOCONFIG),
        (resources.join("mozilla.cfg"), MOZILLA_CFG),
    ] {
        if std::fs::read(&path).is_ok_and(|now| now == contents.as_bytes()) {
            continue;
        }
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, contents));
        written.map_err(|e| format!("can't write {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Whether `executable` is a Firefox Netweir installed under `home`: the
/// only kind it starts without the automation flag, and the only kind it
/// writes into.
pub(crate) fn is_ours_in(executable: &Path, home: &Path) -> bool {
    let root = home.join("firefox");
    // Both sides as the file system has them, so a path through a link or
    // with `..` in it is judged by where it leads.
    let (Ok(executable), Ok(root)) = (executable.canonicalize(), root.canonicalize()) else {
        return false;
    };
    executable.starts_with(&root)
        && resources_of(&executable)
            .and_then(|r| std::fs::read_to_string(r.join("mozilla.cfg")).ok())
            .is_some_and(|cfg| cfg.lines().next() == Some(MARK))
}

pub(crate) fn is_ours(executable: &Path) -> bool {
    is_ours_in(executable, &netweir_home())
}

/// The newest Firefox `netweir install firefox` put under `home`.
pub(crate) fn installed_firefox(home: &Path) -> Option<PathBuf> {
    let mut versions: Vec<(Vec<u32>, PathBuf)> = std::fs::read_dir(home.join("firefox"))
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let version: Option<Vec<u32>> = name.split('.').map(|n| n.parse().ok()).collect();
            Some((version?, e.path()))
        })
        .collect();
    versions.sort();
    versions
        .into_iter()
        .rev()
        .map(|(_, dir)| firefox_executable(&dir))
        .find(|p| p.is_file())
}

/// The Firefox to launch: `$NETWEIR_FIREFOX`, then the newest one
/// `netweir install firefox` installed. A Firefox installed some other way
/// is never picked: Netweir can drive one only in automation mode, which
/// pages can see, so that takes asking for by name.
pub fn find_firefox() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("NETWEIR_FIREFOX") {
        let path = PathBuf::from(path);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(Error::Launch(format!(
                "$NETWEIR_FIREFOX is {}, which doesn't exist",
                path.display()
            )))
        };
    }
    let home = netweir_home();
    installed_firefox(&home).ok_or_else(|| {
        Error::Launch(format!(
            "no Firefox in {}: run `netweir install firefox`",
            home.join("firefox").display()
        ))
    })
}

/// The oldest Firefox Netweir drives: the first whose remote control start
/// it was tested on.
const OLDEST: u32 = 159;

/// Written into every profile Netweir makes for Firefox. A folder holding
/// a Firefox profile without it is a person's, which Netweir won't touch.
const PROFILE_MARK: &str = "netweir-profile";

/// Where mozilla.cfg writes the BiDi server's port, in the profile.
const PORT_FILE: &str = "netweir-port";

/// Only what a fresh profile would otherwise show or do that no page
/// asked for. Nothing from the remote agent's own list of recommended
/// prefs that a page could see (the popup blocker, focus emulation).
const USER_JS: &str = r#"// Written by Netweir at each launch.
// Not asked to be the default browser.
user_pref("browser.shell.checkDefaultBrowser", false);
// No welcome or "what's new" page in place of the one asked for.
user_pref("browser.aboutwelcome.enabled", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("startup.homepage_welcome_url", "");
user_pref("startup.homepage_welcome_url.additional", "");
user_pref("browser.startup.page", 0);
// No data-reporting notice bar, which shortens the page.
user_pref("datareporting.policy.dataSubmissionPolicyBypassNotification", true);
user_pref("datareporting.healthreport.uploadEnabled", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("toolkit.telemetry.reportingpolicy.firstRun", false);
// The pinned build stays the pinned build: Netweir moves the pin.
user_pref("app.update.disabledForTesting", true);
// Downloads stay in the profile, never in the person's own folder.
user_pref("browser.download.folderList", 2);
user_pref("browser.download.useDownloadDir", true);
// Lets a file Netweir is given for an upload be read where the page runs,
// as choosing it in the file picker would. Only Firefox's own code can
// make a file from a path, never a page's.
user_pref("dom.file.createInChild", true);
// A dialog that opens within three seconds of the last gets a box for
// stopping the page's dialogs, which BiDi doesn't tell of: it would wait
// unanswered. The box is Firefox's own, out of the page's sight.
user_pref("dom.successive_dialog_time_limit", 0);
// Firefox loads sites from its history again in a hidden browser, for the
// new tab page's pictures of them: requests no route or guard hears of,
// which no page asked for.
user_pref("browser.pagethumbnails.capturing_disabled", true);
// Where the system has kept location from Firefox (macOS), a page asking
// for its position opens the system's settings and waits on a dialog
// for someone to answer it, whatever position Netweir gave the page.
user_pref("geo.prompt.open_system_prefs", false);
"#;

/// `user.js` for the profile at `profile`. Public only for the stealth
/// test, whose undriven Firefox must start as Netweir's does.
#[doc(hidden)]
pub fn user_js(profile: &Path) -> String {
    let downloads = profile.join("downloads");
    // As a JSON string, which is a JavaScript one: Windows' backslashes
    // escaped.
    let dir = serde_json::to_string(&downloads.display().to_string()).expect("a string serialises");
    format!("{USER_JS}user_pref(\"browser.download.dir\", {dir});\n")
}

/// A Firefox started and spoken to: what [`crate::Browser`] is made from.
pub(crate) struct Started {
    pub conn: Connection,
    pub process: Process,
    pub profile: PathBuf,
    pub temp: Option<TempProfile>,
    /// Its product string, `Firefox/<version>`.
    pub version: String,
    /// The user context the session is confined to; "default" in
    /// automation mode.
    pub default_context: String,
    pub stealth: bool,
    /// Its first window's place and size, and whether it's maximized.
    pub first_window: Option<([i64; 4], bool)>,
}

/// Starts Firefox as `options` ask: the stealth start for a Firefox
/// Netweir installed, automation mode only when `stealth` is false.
pub(crate) async fn start(options: &LaunchOptions) -> Result<Started> {
    let started = start_once(options).await?;
    if names_its_windows(&started.conn).await? {
        return Ok(started);
    }
    // A Firefox on Windows can start unable to name any of its windows, and
    // then no page opens in it: end it and start another.
    end(started);
    let again = start_once(options).await?;
    if names_its_windows(&again.conn).await? {
        return Ok(again);
    }
    end(again);
    Err(Error::Launch(
        "Firefox started twice without an id for any of its windows, so no page can open in it"
            .into(),
    ))
}

/// Whether Firefox gives its first window an id within a few seconds of
/// starting.
async fn names_its_windows(conn: &Connection) -> Result<bool> {
    let gives_up = Instant::now() + Duration::from_secs(5);
    loop {
        let tree = conn
            .call("", "browsingContext.getTree", json!({"maxDepth": 0}))
            .await?;
        let named = tree["contexts"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|c| c["context"].as_str().is_some_and(|id| !id.is_empty()));
        if named || Instant::now() >= gives_up {
            return Ok(named);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Ends a Firefox started here before any browser took it over.
fn end(started: Started) {
    let Started {
        conn,
        mut process,
        temp,
        ..
    } = started;
    conn.shut();
    let _ = process.child.kill();
    let _ = process.child.wait();
    process.lifeline.process_gone();
    // The temporary profile goes only once Firefox has let go of it.
    drop(temp);
}

async fn start_once(options: &LaunchOptions) -> Result<Started> {
    // A person's profile is refused first, whatever Firefox there is.
    if let Some(dir) = &options.user_data_dir {
        if dir.as_os_str().to_string_lossy().trim().is_empty() {
            return Err(Error::Launch(
                "the profile folder (user_data_dir) is empty: name a folder".into(),
            ));
        }
        check_profile(dir)?;
    }
    let executable = match &options.executable {
        Some(path) => path.clone(),
        None => find_firefox()?,
    };
    if !executable.is_file() {
        return Err(Error::Launch(format!(
            "no Firefox at {}",
            executable.display()
        )));
    }
    let ours = is_ours(&executable);
    if options.stealth && !ours {
        return Err(Error::Launch(format!(
            "{} isn't a Firefox Netweir installed, and Netweir won't change one it didn't. Run \
             `netweir install firefox`, or launch with stealth: false to drive it in automation \
             mode, where pages see navigator.webdriver === true",
            executable.display()
        )));
    }
    if ours {
        // One from an older Netweir gets this one's startup files.
        prepare_firefox(&executable).map_err(Error::Launch)?;
    }
    let stealth = options.stealth;
    if !stealth {
        crate::warn(&format!(
            "pages can see navigator.webdriver === true: {} runs in automation mode",
            executable.display()
        ));
    }

    let (profile, temp) = match &options.user_data_dir {
        Some(dir) => (dir.clone(), None),
        None => {
            let temp = TempProfile::make()
                .map_err(|e| Error::Launch(format!("can't make a profile directory: {e}")))?;
            (temp.dir().to_path_buf(), Some(temp))
        }
    };
    let port_file = profile.join(PORT_FILE);
    let prepared = std::fs::create_dir_all(&profile)
        .and_then(|()| std::fs::write(profile.join(PROFILE_MARK), ""))
        .and_then(|()| std::fs::write(profile.join("user.js"), user_js(&profile)));
    prepared.map_err(|e| {
        Error::Launch(format!(
            "can't prepare the profile {}: {e}",
            profile.display()
        ))
    })?;
    // What an earlier launch left must never be read as this one's.
    for stale in [
        port_file.clone(),
        sibling(&port_file, ".tmp"),
        sibling(&port_file, ".failed"),
        profile.join("WebDriverBiDiServer.json"),
    ] {
        match std::fs::remove_file(&stale) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(Error::Launch(format!(
                    "can't remove {} from an earlier launch: {e}",
                    stale.display()
                )));
            }
            _ => {}
        }
    }

    let mut args = vec![
        "-profile".to_string(),
        profile.display().to_string(),
        "-no-remote".into(),
        "-new-instance".into(),
        // On Windows firefox.exe is a launcher that starts the browser
        // and exits; this keeps it until the browser ends, so its exit
        // means the browser's.
        "-wait-for-browser".into(),
    ];
    if options.headless {
        args.push("-headless".into());
    }
    if !stealth {
        args.push("--remote-debugging-port=0".into());
    }
    args.extend(options.args.iter().cloned());
    let envs: Vec<(&str, &std::ffi::OsStr)> = if stealth {
        vec![("NETWEIR_FIREFOX_PORT_FILE", port_file.as_os_str())]
    } else {
        Vec::new()
    };
    let mut process = crate::launch::spawn(&executable, &args, &envs, "Firefox", None)?;
    let said = process.said.clone();

    let deadline = Instant::now() + options.timeout;
    let port = wait_for_port(&mut process, &profile, stealth, deadline)
        .await
        .map_err(|e| explained(e, &said))?;
    let url = format!("ws://127.0.0.1:{port}/session");
    let socket = tokio::time::timeout_at(deadline.into(), tokio_tungstenite::connect_async(&url))
        .await
        .map_err(|_| {
            Error::Launch(format!(
                "Firefox didn't take the connection to {url} in time"
            ))
        })?
        .map_err(|e| Error::Launch(said.explain(format!("can't connect to {url}: {e}"))))?
        .0;
    let conn = Connection::over_websocket(socket, Dialect::Bidi);
    let capabilities = json!({"alwaysMatch": {
        // Firefox leaves dialogs to the session, as Chrome does: Netweir,
        // not Firefox, decides how each is answered.
        "unhandledPromptBehavior": {"default": "ignore"},
    }});
    let session = tokio::time::timeout_at(
        deadline.into(),
        conn.call("", "session.new", json!({"capabilities": capabilities})),
    )
    .await
    .map_err(|_| Error::Launch("Firefox didn't answer session.new in time".into()))?
    .map_err(|e| Error::Launch(format!("Firefox refused session.new: {e}")))?;
    let capabilities = &session["capabilities"];
    let version = capabilities["browserVersion"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    check_version(&version)?;
    let default_context = match capabilities["moz:userContext"].as_str() {
        Some(id) => id.to_string(),
        None if !stealth => "default".to_string(),
        None => {
            return Err(Error::Launch(
                "Firefox's session.new had no moz:userContext, so the stealth start didn't take"
                    .into(),
            ));
        }
    };
    // A new window opens cascaded from the last; each page's is put where
    // the first opened instead.
    let windows = conn.call("", "browser.getClientWindows", json!({})).await?;
    let first_window = windows["clientWindows"].get(0).map(|w| {
        (
            ["x", "y", "width", "height"].map(|k| w[k].as_i64().unwrap_or_default()),
            w["state"] == "maximized",
        )
    });
    Ok(Started {
        conn,
        process,
        profile,
        temp,
        version: format!("Firefox/{version}"),
        default_context,
        stealth,
        first_window,
    })
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// A launch error with what Firefox wrote to stderr after it.
fn explained(e: Error, said: &Said) -> Error {
    match e {
        Error::Launch(message) => Error::Launch(said.explain(message)),
        other => other,
    }
}

/// Refuses a folder holding a Firefox profile Netweir didn't make.
fn check_profile(dir: &Path) -> Result<()> {
    // A profile's own files, or those of the folder a person's profiles
    // are in.
    let firefoxs = [
        "prefs.js",
        "times.json",
        "places.sqlite",
        "cookies.sqlite",
        "profiles.ini",
        "installs.ini",
    ]
    .iter()
    .any(|f| dir.join(f).exists());
    if firefoxs && !dir.join(PROFILE_MARK).exists() {
        return Err(Error::Launch(format!(
            "{} holds a Firefox profile Netweir didn't make, and Netweir won't change a person's \
             profile: give a new folder, or one an earlier launch made",
            dir.display()
        )));
    }
    Ok(())
}

/// Refuses a Firefox older than the oldest Netweir drives.
fn check_version(version: &str) -> Result<()> {
    let major = version
        .split('.')
        .next()
        .and_then(|m| m.parse::<u32>().ok());
    match major {
        Some(major) if major >= OLDEST => Ok(()),
        _ => Err(Error::Launch(format!(
            "Firefox {version} is older than {OLDEST}, the oldest Netweir drives"
        ))),
    }
}

/// The BiDi server's port, once Firefox has written it: to the port file
/// in the stealth start, or as automation mode writes it into the profile.
/// Fails as soon as Firefox exits or mozilla.cfg says a step failed.
async fn wait_for_port(
    process: &mut Process,
    profile: &Path,
    stealth: bool,
    deadline: Instant,
) -> Result<u16> {
    let port_file = profile.join(PORT_FILE);
    let failed_file = sibling(&port_file, ".failed");
    loop {
        let port = if stealth {
            std::fs::read_to_string(&port_file)
                .ok()
                .and_then(|p| p.trim().parse::<u16>().ok())
        } else {
            std::fs::read_to_string(profile.join("WebDriverBiDiServer.json"))
                .ok()
                .and_then(|j| serde_json::from_str::<Value>(&j).ok())
                .and_then(|j| j["ws_port"].as_u64())
                .and_then(|p| u16::try_from(p).ok())
        };
        if let Some(port) = port.filter(|p| *p != 0) {
            return Ok(port);
        }
        // mozilla.cfg says which step failed, in a file beside the port's.
        if let Ok(failure) = std::fs::read_to_string(&failed_file) {
            return Err(Error::Launch(format!(
                "Firefox didn't start its remote agent: {}",
                failure.trim()
            )));
        }
        if let Ok(Some(status)) = process.child.try_wait() {
            return Err(Error::Launch(format!(
                "Firefox exited ({status}) before it was ready"
            )));
        }
        if Instant::now() >= deadline {
            return Err(Error::Launch(
                "Firefox didn't say which port to connect to in time".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("netweir-ff-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An install folder holding an empty executable where Firefox's is.
    fn install(dir: &Path) -> PathBuf {
        let executable = firefox_executable(dir);
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, "").unwrap();
        executable
    }

    #[test]
    fn prepare_writes_both_files_where_firefox_reads_them() {
        let dir = temp("prepare");
        let executable = install(&dir);
        prepare_firefox(&executable).unwrap();
        let resources = if cfg!(target_os = "macos") {
            dir.join("Firefox.app/Contents/Resources")
        } else if cfg!(windows) {
            dir.join("core")
        } else {
            dir.join("firefox")
        };
        assert_eq!(
            std::fs::read_to_string(resources.join("defaults/pref/autoconfig.js")).unwrap(),
            AUTOCONFIG
        );
        assert_eq!(
            std::fs::read_to_string(resources.join("mozilla.cfg")).unwrap(),
            MOZILLA_CFG
        );
        // Firefox skips the first line; ours is the mark.
        assert_eq!(MOZILLA_CFG.lines().next(), Some(MARK));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prepare_leaves_unchanged_files_alone_and_mends_changed_ones() {
        let dir = temp("unchanged");
        let executable = install(&dir);
        prepare_firefox(&executable).unwrap();
        let cfg = resources_of(&executable).unwrap().join("mozilla.cfg");
        let autoconfig = resources_of(&executable)
            .unwrap()
            .join("defaults/pref/autoconfig.js");
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        for f in [&cfg, &autoconfig] {
            std::fs::File::options()
                .write(true)
                .open(f)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        std::fs::write(&cfg, "// netweir\n// an older one\n").unwrap();
        prepare_firefox(&executable).unwrap();
        assert_eq!(
            std::fs::metadata(&autoconfig).unwrap().modified().unwrap(),
            old
        );
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), MOZILLA_CFG);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn installed_firefox_picks_the_newest_version() {
        let home = temp("newest");
        for version in ["159.0", "159.0.1", "160.0b3", ".159.0-firefox-12", "158.0"] {
            install(&home.join("firefox").join(version));
        }
        // A version folder without an executable doesn't count.
        std::fs::create_dir_all(home.join("firefox/161.0")).unwrap();
        assert_eq!(
            installed_firefox(&home),
            Some(firefox_executable(&home.join("firefox/159.0.1")))
        );
        assert_eq!(installed_firefox(&home.join("nothing")), None);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn firefox_older_than_159_is_refused() {
        assert!(check_version("159.0").is_ok());
        assert!(check_version("160.0b3").is_ok());
        let err = check_version("158.0.2").unwrap_err().to_string();
        assert_eq!(
            err,
            "Firefox 158.0.2 is older than 159, the oldest Netweir drives"
        );
        assert!(check_version("").is_err());
    }

    #[test]
    fn a_profile_netweir_didnt_make_is_refused() {
        let dir = temp("theirs");
        assert!(check_profile(&dir).is_ok(), "an empty folder is fine");
        std::fs::write(dir.join("prefs.js"), "// theirs").unwrap();
        let err = check_profile(&dir).unwrap_err().to_string();
        assert!(err.contains("didn't make"), "{err}");
        // Nor the folder a person's profiles are in.
        let root = temp("theirs-root");
        std::fs::write(root.join("profiles.ini"), "[General]\n").unwrap();
        assert!(check_profile(&root).is_err());
        std::fs::remove_dir_all(root).unwrap();
        std::fs::write(dir.join(PROFILE_MARK), "").unwrap();
        assert!(check_profile(&dir).is_ok(), "one Netweir made");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_a_netweir_install_is_ours() {
        let home = temp("ours");
        let ours = install(&home.join("firefox/159.0"));
        assert!(!is_ours_in(&ours, &home), "no cfg yet");
        prepare_firefox(&ours).unwrap();
        assert!(is_ours_in(&ours, &home));
        // The same files in a Firefox somewhere else don't make it ours.
        let elsewhere = install(&home.join("elsewhere"));
        prepare_firefox(&elsewhere).unwrap();
        assert!(!is_ours_in(&elsewhere, &home));
        // Nor does a path that only looks as if it's inside.
        let sneaky = home.join("firefox/159.0/../../elsewhere");
        assert!(!is_ours_in(&firefox_executable(&sneaky), &home));
        // A cfg someone else wrote isn't Netweir's.
        let theirs = install(&home.join("firefox/158.0"));
        let resources = resources_of(&theirs).unwrap();
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::write(resources.join("mozilla.cfg"), "// company policy\n").unwrap();
        assert!(!is_ours_in(&theirs, &home));
        std::fs::remove_dir_all(home).unwrap();
    }
}
