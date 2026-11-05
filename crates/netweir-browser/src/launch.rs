//! Finding Chrome and starting it with the DevTools pipe.

use std::io::{PipeReader, PipeWriter};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};

use crate::lifeline::{Lifeline, tie};
use crate::{Error, Result};

/// netweir's own folder: `$NETWEIR_HOME`, or `~/.netweir`.
pub fn netweir_home() -> PathBuf {
    if let Some(home) = std::env::var_os("NETWEIR_HOME") {
        return PathBuf::from(home);
    }
    let user = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    user.map(PathBuf::from).unwrap_or_default().join(".netweir")
}

/// This machine as Chrome for Testing names platforms, if it has builds
/// for it.
pub fn cft_platform() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("mac-arm64"),
        ("macos", "x86_64") => Some("mac-x64"),
        ("linux", "x86_64") => Some("linux64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        ("windows", "x86_64") => Some("win64"),
        ("windows", "x86") => Some("win32"),
        _ => None,
    }
}

/// Where the executable is in an unpacked Chrome for Testing download for
/// `platform`: Chrome itself, or `chrome-headless-shell`.
pub fn cft_executable(platform: &str, headless_shell: bool) -> PathBuf {
    let exe = if platform.starts_with("win") {
        ".exe"
    } else {
        ""
    };
    if headless_shell {
        return PathBuf::from(format!(
            "chrome-headless-shell-{platform}/chrome-headless-shell{exe}"
        ));
    }
    if platform.starts_with("mac") {
        return PathBuf::from(format!(
            "chrome-{platform}/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"
        ));
    }
    PathBuf::from(format!("chrome-{platform}/chrome{exe}"))
}

/// The newest Chrome `netweir install chrome` put under `home`.
pub(crate) fn installed_chrome(home: &Path) -> Option<PathBuf> {
    let platform = cft_platform()?;
    let mut versions: Vec<(Vec<u32>, PathBuf)> = std::fs::read_dir(home.join("chrome"))
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
        .map(|(_, dir)| dir.join(cft_executable(platform, false)))
        .find(|p| p.is_file())
}

/// Written into an install once Chrome's sandbox has been given access to
/// it.
const SANDBOX_GRANTED: &str = ".netweir-sandbox-access";

/// What `icacls` is given to let Chrome's sandboxed processes read and run
/// what's in `dir`: read and execute, inherited by everything in it, for
/// ALL APPLICATION PACKAGES (S-1-15-2-1) and ALL RESTRICTED APPLICATION
/// PACKAGES (S-1-15-2-2), by SID, since the names are translated.
pub fn sandbox_grant_args(dir: &Path) -> Vec<std::ffi::OsString> {
    vec![
        dir.as_os_str().to_owned(),
        "/grant".into(),
        "*S-1-15-2-1:(OI)(CI)(RX)".into(),
        "/grant".into(),
        "*S-1-15-2-2:(OI)(CI)(RX)".into(),
        "/Q".into(),
    ]
}

/// The version folder of a Chrome `netweir install chrome` put under
/// `home`, if `executable` is one.
pub(crate) fn install_dir_of(executable: &Path, home: &Path) -> Option<PathBuf> {
    let root = home.join("chrome");
    let rest = executable.strip_prefix(&root).ok()?;
    let mut parts = rest.components();
    let version = parts.next()?;
    // A version folder holds the build; the executable is inside it.
    parts.next()?;
    Some(root.join(version))
}

/// Lets Chrome's sandbox read the Chrome installed in `dir`, once. On
/// Windows, Chrome runs its network service and other helpers in an
/// AppContainer, which can read only what grants app packages access;
/// Chrome's own installer grants it, and an unpacked download has none,
/// so the network service can't start, and every navigation under way
/// when it fails is aborted. Elsewhere there's nothing to do.
pub fn grant_sandbox_access(dir: &Path) -> std::result::Result<(), String> {
    if !cfg!(windows) || dir.join(SANDBOX_GRANTED).is_file() {
        return Ok(());
    }
    let out = Command::new("icacls")
        .args(sandbox_grant_args(dir))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            format!(
                "can't run icacls to give Chrome's sandbox access to {}: {e}",
                dir.display()
            )
        })?;
    if !out.status.success() {
        return Err(format!(
            "icacls couldn't give Chrome's sandbox access to {} ({}): {}",
            dir.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    std::fs::write(dir.join(SANDBOX_GRANTED), "")
        .map_err(|e| format!("can't mark {} as done: {e}", dir.display()))
}

/// [`grant_sandbox_access`] for `executable`, if it's a Chrome netweir
/// installed; one installed elsewhere is never touched.
pub(crate) fn grant_if_ours(executable: &Path) -> Result<()> {
    match install_dir_of(executable, &netweir_home()) {
        Some(dir) => grant_sandbox_access(&dir).map_err(Error::Launch),
        None => Ok(()),
    }
}

/// More Chrome switches, from `$NETWEIR_CHROME_ARGS`, for every Chrome
/// netweir launches: what a machine needs whatever the code says, such as
/// `--disable-gpu` on a virtual Mac whose GPU hangs.
pub(crate) fn env_switches() -> Vec<String> {
    switches_from(std::env::var_os("NETWEIR_CHROME_ARGS"))
}

fn switches_from(value: Option<std::ffi::OsString>) -> Vec<String> {
    value
        .map(|v| {
            v.to_string_lossy()
                .split_whitespace()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The name and value (empty without `=`) of `arg`, if Chrome reads it as
/// a switch: after `--` or `-`, or `/` on Windows, with the name in
/// lower case on Windows, where Chrome lowers it, and as given elsewhere.
/// Surrounding whitespace is trimmed first, as Chrome trims it.
fn switch_in(arg: &str) -> Option<(String, &str)> {
    let arg = arg.trim();
    let prefixes: &[&str] = if cfg!(windows) {
        &["--", "-", "/"]
    } else {
        &["--", "-"]
    };
    let rest = prefixes
        .iter()
        .find_map(|p| arg.strip_prefix(p))
        .filter(|rest| !rest.is_empty())?;
    let (name, value) = rest.split_once('=').unwrap_or((rest, ""));
    let name = if cfg!(windows) {
        name.to_ascii_lowercase()
    } else {
        name.to_string()
    };
    Some((name, value))
}

/// How many of `args` Chrome reads for switches: all of them, up to a
/// bare `--`, after which nothing is a switch.
fn switch_end(args: &[String]) -> usize {
    args.iter()
        .position(|a| a.trim() == "--")
        .unwrap_or(args.len())
}

/// The value Chrome takes for the switch `name` (given without its
/// prefix) in `args`: the last one's.
pub(crate) fn last_switch(args: &[String], name: &str) -> Option<String> {
    args[..switch_end(args)]
        .iter()
        .rev()
        .filter_map(|a| switch_in(a))
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.to_string())
}

/// Takes every switch `name` out of `args`, returning their values in
/// order.
pub(crate) fn take_switches(args: &mut Vec<String>, name: &str) -> Vec<String> {
    let end = switch_end(args);
    let mut values = Vec::new();
    let mut kept = Vec::with_capacity(args.len());
    for (i, arg) in args.drain(..).enumerate() {
        match switch_in(&arg) {
            Some((n, v)) if i < end && n == name => values.push(v.to_string()),
            _ => kept.push(arg),
        }
    }
    *args = kept;
    values
}

/// The profile Chrome opens in the user data directory `dir`, as Chrome
/// picks it: `--profile-directory`, then the one Local State says was
/// used last, then `Default`. A name that isn't a single folder inside
/// `dir` (`..`, a path, an absolute one) is refused rather than followed
/// outside it, as Local State from a profile someone else made could
/// ask; Chrome itself wouldn't start with it either, and falling back to
/// `Default` would prepare a profile Chrome doesn't open.
pub(crate) fn profile_in(dir: &Path, args: &[String]) -> Result<String> {
    let name = match last_switch(args, "profile-directory") {
        Some(named) => named,
        None => std::fs::read(dir.join("Local State"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|state| state["profile"]["last_used"].as_str().map(str::to_string))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Default".into()),
    };
    let mut parts = Path::new(&name).components();
    let single = matches!(
        (parts.next(), parts.next()),
        (Some(std::path::Component::Normal(_)), None)
    );
    if !single || name.contains(['/', '\\']) {
        return Err(Error::Launch(format!(
            "the profile {name:?} isn't a folder in {}: a profile is named by one folder \
             inside the user data directory",
            dir.display()
        )));
    }
    Ok(name)
}

/// Turns off Chrome's own "Preload pages" setting (Settings > Performance)
/// in the profile Chrome will open in `dir` (see [`profile_in`]). With it
/// on, Chrome fetches the pages a page's speculation rules name, and can
/// show one when a link to it is followed, outside every session netweir
/// has: no route, block or guard would see the request. Off, a page can
/// tell nothing from its own side: speculation rules are still supported,
/// and a real person can have it off too. The rest of the profile's
/// preferences are kept as they are; a file already set this way isn't
/// written.
pub(crate) fn turn_off_preloading(dir: &Path, args: &[String]) -> Result<()> {
    // 2 is "never": what the setting saves when it's switched off.
    set_pref(
        dir,
        args,
        ("net", "network_prediction_options"),
        serde_json::json!(2),
        "turn off preloading",
    )
}

/// Has Chrome download PDFs rather than show them, as Chrome on a phone
/// does, which has no PDF viewer: with it, `navigator.plugins` and
/// `navigator.mimeTypes` are empty and `navigator.pdfViewerEnabled` is
/// false, as they are there.
pub(crate) fn open_pdfs_outside(dir: &Path, args: &[String]) -> Result<()> {
    set_pref(
        dir,
        args,
        ("plugins", "always_open_pdf_externally"),
        serde_json::json!(true),
        "turn off the PDF viewer",
    )
}

/// Sets the preference `section.key` to `value` in the profile Chrome
/// will open in `dir`, saying it couldn't `what` if it can't.
fn set_pref(
    dir: &Path,
    args: &[String],
    (section, key): (&str, &str),
    value: serde_json::Value,
    what: &str,
) -> Result<()> {
    let profile = dir.join(profile_in(dir, args)?);
    let file = profile.join("Preferences");
    let failed = |why: String| Error::Launch(format!("can't {what} in {}: {why}", file.display()));
    let mut prefs = match std::fs::read(&file) {
        // Chrome starts afresh from an empty file, as one cut short by a
        // crash can be; anything else that isn't JSON is someone's to
        // look at, not netweir's to replace.
        Ok(bytes) if bytes.trim_ascii().is_empty() => serde_json::json!({}),
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| failed(format!("it isn't valid JSON ({e})")))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(failed(e.to_string())),
    };
    let Some(group) = prefs
        .as_object_mut()
        .map(|p| p.entry(section).or_insert_with(|| serde_json::json!({})))
        .and_then(|group| group.as_object_mut())
    else {
        return Err(failed("it isn't the preferences Chrome writes".into()));
    };
    if group.get(key) == Some(&value) {
        return Ok(());
    }
    group.insert(key.into(), value);
    std::fs::create_dir_all(&profile).map_err(|e| failed(e.to_string()))?;
    replace(&file, prefs.to_string().as_bytes()).map_err(|e| failed(e.to_string()))
}

/// Puts `contents` in `file` whole, so Chrome never reads half of it: in a
/// file beside it first, then renamed into place. A linked file is
/// changed where it really is, and the link kept; the file keeps its
/// permissions, and a new one is private, as Chrome makes it.
fn replace(file: &Path, contents: &[u8]) -> std::io::Result<()> {
    let (target, permissions) = match std::fs::metadata(file) {
        Ok(meta) => (std::fs::canonicalize(file)?, Some(meta.permissions())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (file.to_path_buf(), None),
        Err(e) => return Err(e),
    };
    let partial = target.with_extension("netweir");
    let written = (|| {
        let mut open = std::fs::OpenOptions::new();
        open.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
        std::io::Write::write_all(&mut open.open(&partial)?, contents)?;
        if let Some(permissions) = permissions {
            std::fs::set_permissions(&partial, permissions)?;
        }
        std::fs::rename(&partial, &target)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    written
}

/// `args` with `features` added to Chrome's `--enable-features`: those
/// already there, in whatever form Chrome reads, are gathered into one
/// switch with them, since Chrome reads only the last. It goes before a
/// bare `--`, after which Chrome reads no switches.
pub(crate) fn enable_features(mut args: Vec<String>, features: &[&str]) -> Vec<String> {
    let mut enabled: Vec<String> = features.iter().map(|f| f.to_string()).collect();
    for list in take_switches(&mut args, "enable-features") {
        enabled.extend(
            list.split(',')
                .filter(|f| !f.is_empty())
                .map(str::to_string),
        );
    }
    let end = switch_end(&args);
    args.insert(end, format!("--enable-features={}", enabled.join(",")));
    args
}

/// The Chrome to launch: `$NETWEIR_CHROME`, then the newest one
/// `netweir install chrome` installed, then the usual install paths
/// (Chrome, Chrome for Testing, Chromium). The error lists where it
/// looked.
pub fn find_chrome() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("NETWEIR_CHROME") {
        let path = PathBuf::from(path);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(Error::Launch(format!(
                "$NETWEIR_CHROME is {}, which doesn't exist",
                path.display()
            )))
        };
    }
    let home = netweir_home();
    if let Some(installed) = installed_chrome(&home) {
        return Ok(installed);
    }
    let candidates = candidates();
    candidates
        .iter()
        .find(|p| p.is_file())
        .cloned()
        .ok_or_else(|| {
            let looked: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
            Error::Launch(format!(
                "no Chrome found; run `netweir install chrome`, set $NETWEIR_CHROME or pass \
                 executable=. Looked in: {}, {}",
                home.join("chrome").display(),
                looked.join(", ")
            ))
        })
}

#[cfg(target_os = "macos")]
fn candidates() -> Vec<PathBuf> {
    let apps = [
        "Google Chrome.app/Contents/MacOS/Google Chrome",
        "Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
        "Chromium.app/Contents/MacOS/Chromium",
    ];
    let mut dirs = vec![PathBuf::from("/Applications")];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(Path::new(&home).join("Applications"));
    }
    dirs.iter()
        .flat_map(|d| apps.iter().map(move |a| d.join(a)))
        .collect()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn candidates() -> Vec<PathBuf> {
    let names = [
        "google-chrome",
        "google-chrome-stable",
        "chrome",
        "chromium",
        "chromium-browser",
    ];
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut found: Vec<PathBuf> = std::env::split_paths(&path)
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .collect();
    found.push(PathBuf::from("/opt/google/chrome/chrome"));
    found
}

#[cfg(windows)]
fn candidates() -> Vec<PathBuf> {
    let suffixes = [
        r"Google\Chrome\Application\chrome.exe",
        r"Google\Chrome for Testing\Application\chrome.exe",
        r"Chromium\Application\chrome.exe",
    ];
    ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"]
        .iter()
        .filter_map(std::env::var_os)
        .flat_map(|d| suffixes.iter().map(move |s| Path::new(&d).join(s)))
        .collect()
}

/// A started Chrome: the process, and the pipe's two ends on our side.
pub(crate) struct Started {
    pub child: Child,
    pub commands: PipeWriter,
    pub replies: PipeReader,
    pub said: Said,
    pub lifeline: Lifeline,
}

/// The last lines Chrome wrote to stderr, so an error can say why it
/// wouldn't start. Read on a thread of its own until Chrome closes it,
/// which also keeps a chatty Chrome from filling the pipe and stalling.
/// It also says, as soon as Chrome does, that Chrome won't be driven
/// with the profile it was given (see [`refuses_its_profile`]).
#[derive(Clone)]
pub(crate) struct Said(
    Arc<(Mutex<Lines>, Condvar, tokio::sync::watch::Sender<bool>)>,
    /// The browser saying it, for messages.
    &'static str,
);

impl Default for Said {
    fn default() -> Said {
        Said(
            Arc::new((
                Mutex::default(),
                Condvar::new(),
                tokio::sync::watch::Sender::new(false),
            )),
            "Chrome",
        )
    }
}

/// The lines kept, and whether Chrome has closed stderr.
type Lines = (std::collections::VecDeque<String>, bool);

/// Whether `line`, from Chrome's stderr, is Google Chrome refusing to be
/// driven with its own default profile. It says so and then runs on, not
/// listening, so without this a launch would wait out its timeout.
pub(crate) fn refuses_its_profile(line: &str) -> bool {
    line.contains("DevTools remote debugging requires a non-default data directory")
}

/// The error for Google Chrome given its own default profile, `dir`.
pub(crate) fn own_profile_refused(dir: &Path) -> Error {
    Error::Launch(format!(
        "Google Chrome won't be driven with its own default profile ({}): it turns \
         remote debugging off for that folder. Copy the profile to another folder and \
         use that, or use Chrome for Testing, which `netweir install chrome` installs",
        dir.display()
    ))
}

/// Where Google Chrome at `executable` keeps its default profile, if
/// it's a Google Chrome (Stable, Beta, Dev or Canary) installed where
/// its installer puts it. Chrome for Testing, Chromium and anything
/// installed elsewhere give None; Chrome's own refusal (see
/// [`refuses_its_profile`]) still catches those.
#[cfg(target_os = "macos")]
fn own_profile_of(executable: &Path) -> Option<PathBuf> {
    let channel = match executable.file_name()?.to_str()? {
        "Google Chrome" => "Chrome",
        "Google Chrome Beta" => "Chrome Beta",
        "Google Chrome Dev" => "Chrome Dev",
        "Google Chrome Canary" => "Chrome Canary",
        _ => return None,
    };
    let home = std::env::var_os("HOME")?;
    Some(
        Path::new(&home)
            .join("Library/Application Support/Google")
            .join(channel),
    )
}

#[cfg(all(unix, not(target_os = "macos")))]
fn own_profile_of(executable: &Path) -> Option<PathBuf> {
    // /usr/bin/google-chrome and the like lead to /opt/google/<channel>.
    let executable = executable
        .canonicalize()
        .unwrap_or(executable.to_path_buf());
    let install = executable.parent()?;
    if install.parent()? != Path::new("/opt/google") {
        return None;
    }
    let folder = match install.file_name()?.to_str()? {
        "chrome" => "google-chrome",
        "chrome-beta" => "google-chrome-beta",
        "chrome-unstable" => "google-chrome-unstable",
        "chrome-canary" => "google-chrome-canary",
        _ => return None,
    };
    let config = std::env::var_os("CHROME_CONFIG_HOME")
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))?;
    Some(config.join(folder))
}

#[cfg(windows)]
fn own_profile_of(executable: &Path) -> Option<PathBuf> {
    // <Program Files or LocalAppData>\Google\<channel>\Application\chrome.exe
    if !executable.file_name()?.eq_ignore_ascii_case("chrome.exe") {
        return None;
    }
    let application = executable.parent()?;
    let channel = application.parent()?;
    if !application.file_name()?.eq_ignore_ascii_case("Application")
        || !channel
            .parent()?
            .file_name()?
            .eq_ignore_ascii_case("Google")
    {
        return None;
    }
    let channel = channel.file_name()?.to_str()?;
    if !["Chrome", "Chrome Beta", "Chrome Dev", "Chrome SxS"]
        .iter()
        .any(|c| c.eq_ignore_ascii_case(channel))
    {
        return None;
    }
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(
        Path::new(&local)
            .join("Google")
            .join(channel)
            .join("User Data"),
    )
}

/// Refuses, before anything touches it, a `dir` that's the default
/// profile of the Google Chrome at `executable`, which Chrome would only
/// refuse once started.
pub(crate) fn check_profile(executable: &Path, dir: &Path) -> Result<()> {
    let Some(own) = own_profile_of(executable) else {
        return Ok(());
    };
    let same = match (dir.canonicalize(), own.canonicalize()) {
        (Ok(dir), Ok(own)) => dir == own,
        _ => dir == own,
    };
    if same {
        Err(own_profile_refused(dir))
    } else {
        Ok(())
    }
}

impl Said {
    fn read(stderr: PipeReader, browser: &'static str) -> Said {
        let said = Said(Said::default().0, browser);
        let writing = said.clone();
        let _ = std::thread::Builder::new()
            .name(format!("netweir-{}-stderr", browser.to_ascii_lowercase()))
            .spawn(move || {
                use std::io::BufRead;
                let (lock, done, refused) = &*writing.0;
                for line in std::io::BufReader::new(stderr)
                    .lines()
                    .map_while(|l| l.ok())
                {
                    if refuses_its_profile(&line) {
                        refused.send_replace(true);
                    }
                    let mut lines = lock.lock().unwrap_or_else(|e| e.into_inner());
                    if lines.0.len() == 20 {
                        lines.0.pop_front();
                    }
                    lines.0.push_back(line);
                }
                lock.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
                done.notify_all();
            });
        said
    }

    /// `message`, with what Chrome has said lately, if anything, after
    /// it; for a Chrome still running.
    pub(crate) fn lately(&self, message: String) -> String {
        match self.recent() {
            Some(said) => format!("{message}; {} said lately:\n{said}", self.1),
            None => message,
        }
    }

    /// Ends once Chrome has said it won't be driven with its profile;
    /// never, if it doesn't.
    pub(crate) async fn refused(&self) {
        let _ = self.0.2.subscribe().wait_for(|r| *r).await;
    }

    /// What Chrome has lately written to stderr, if anything.
    pub(crate) fn recent(&self) -> Option<String> {
        let lines = self.0.0.lock().unwrap_or_else(|e| e.into_inner());
        if lines.0.is_empty() {
            return None;
        }
        let said: Vec<&str> = lines.0.iter().map(String::as_str).collect();
        Some(said.join("\n"))
    }

    /// `message`, with what Chrome said, if anything, after it. Waits up
    /// to a second for a Chrome that has just exited to finish saying it;
    /// only ever on the way to an error.
    pub(crate) fn explain(&self, message: String) -> String {
        let (lock, done, _) = &*self.0;
        let lines = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (lines, _) = done
            .wait_timeout_while(lines, std::time::Duration::from_secs(1), |l| !l.1)
            .unwrap_or_else(|e| e.into_inner());
        if lines.0.is_empty() {
            return message;
        }
        let said: Vec<&str> = lines.0.iter().map(String::as_str).collect();
        format!("{message}; {} said:\n{}", self.1, said.join("\n"))
    }
}

/// A pipe whose ends no process started from now on comes by, unless
/// it's handed them. Elsewhere std's pipe is that already: its ends are
/// made close-on-exec as they're made.
#[cfg(not(target_os = "macos"))]
pub(crate) fn pipe() -> std::io::Result<(PipeReader, PipeWriter)> {
    std::io::pipe()
}

/// macOS has no pipe2, so std's pipe is made open to every process and
/// only then marked close-on-exec; a process the program starts in
/// between, on any thread, keeps its ends. One that kept Chrome's
/// writing end held the pipe open past Chrome's death, which then went
/// unseen. A named pipe's ends are opened close-on-exec instead, at
/// once, and the name is gone before they're used.
#[cfg(target_os = "macos")]
pub(crate) fn pipe() -> std::io::Result<(PipeReader, PipeWriter)> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;

    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = loop {
        let path = std::env::temp_dir().join(format!(
            "netweir-pipe-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let name = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } == 0 {
            break path;
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(e);
        }
    };
    // Read first, without waiting for a writer, then write, which has its
    // reader. std opens everything close-on-exec.
    let ends = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)
        .and_then(|reader| {
            let writer = std::fs::OpenOptions::new().write(true).open(&path)?;
            Ok((reader, writer))
        });
    let _ = std::fs::remove_file(&path);
    let (reader, writer) = ends?;
    if unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, 0) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((
        PipeReader::from(std::os::fd::OwnedFd::from(reader)),
        PipeWriter::from(std::os::fd::OwnedFd::from(writer)),
    ))
}

/// Taken while a process is started with handles made inheritable for it
/// (see start).
static SPAWNING: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn start(executable: &Path, args: &[String]) -> Result<Started> {
    if !executable.is_file() {
        return Err(Error::Launch(format!(
            "no Chrome at {}",
            executable.display()
        )));
    }
    let failed =
        |e: std::io::Error| Error::Launch(format!("can't start {}: {e}", executable.display()));
    // Chrome reads commands from one pipe and writes replies to the other.
    let (chrome_reads, commands) = pipe().map_err(failed)?;
    let (replies, chrome_writes) = pipe().map_err(failed)?;
    let Process {
        child,
        said,
        lifeline,
    } = spawn(
        executable,
        args,
        &[],
        "Chrome",
        Some((chrome_reads, chrome_writes)),
    )?;
    Ok(Started {
        child,
        commands,
        replies,
        said,
        lifeline,
    })
}

/// A started browser that talks over a socket rather than a pipe, as
/// Firefox does: the process, what it writes to stderr, and its lifeline.
pub(crate) struct Process {
    pub child: Child,
    pub said: Said,
    pub lifeline: Lifeline,
}

/// Starts `browser` (named for messages) at `executable` with `args` and
/// `envs`, tied to netweir: in a group of its own on macOS and Linux, in a
/// job on Windows. `ends` are the pipe ends a browser that reads its
/// commands from a pipe is handed.
pub(crate) fn spawn(
    executable: &Path,
    args: &[String],
    envs: &[(&str, &std::ffi::OsStr)],
    browser: &'static str,
    ends: Option<(PipeReader, PipeWriter)>,
) -> Result<Process> {
    let failed =
        |e: std::io::Error| Error::Launch(format!("can't start {}: {e}", executable.display()));
    // Its stderr through a pipe of netweir's, so no process started
    // meanwhile keeps the writing end and Said waits on it after the
    // browser has gone.
    let (stderr, browser_says) = pipe().map_err(failed)?;
    // Firefox's stdout goes there too, where it's read. On Windows, a
    // Firefox whose stdout was the null device now and then lost the ids
    // of all its windows, its first one's included, and opened no page
    // after (#247); with it read, 18 runs in CI in a row never did.
    let stdout = match browser {
        "Firefox" => Stdio::from(browser_says.try_clone().map_err(failed)?),
        _ => Stdio::null(),
    };
    let mut command = Command::new(executable);
    command
        .args(args)
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::from(browser_says));
    // In a group of its own, which its helpers join: what the watcher
    // ends (see tie).
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    // Started suspended, so it's in its job before it can start a helper.
    // chrome-headless-shell is a console program: with a console of its
    // own, hidden, a Ctrl+C or Ctrl-Break meant for the program can't end
    // it or its helpers before the program closes it. Chrome and Firefox
    // have no console, and the flag doesn't apply to them.
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, {
        use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};
        CREATE_NO_WINDOW | CREATE_SUSPENDED
    });
    // On Windows, the browser's ends are inheritable from hand_over until
    // they're dropped, and any child started meanwhile would get them too;
    // then a browser's reader would never see its browser exit. Launches
    // here take turns through that window, and each starts its watcher in
    // its own turn: a watcher started during another launch's window took
    // that browser's ends and kept them for as long as it ran.
    let (child, lifeline) = {
        let _turn = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((reads, writes)) = &ends {
            hand_over(&mut command, reads, writes).map_err(failed)?;
        }
        let child = command.spawn().map_err(failed)?;
        // Our copies of the browser's ends must close, or we'd never see
        // it exit; the command holds its stderr's.
        drop((ends, command));
        let lifeline = tie(&child);
        (child, lifeline)
    };
    #[cfg(windows)]
    let mut child = child;
    #[cfg(windows)]
    if let Err(e) = resume(&child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(failed(e));
    }
    let said = Said::read(stderr, browser);
    Ok(Process {
        child,
        said,
        lifeline,
    })
}

/// The Windows system folder, where `cmd.exe` and `PING.EXE` are.
#[cfg(windows)]
pub(crate) fn system_dir() -> std::io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut buffer = vec![0u16; 512];
    // SAFETY: the buffer is the size given.
    let len = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if len == 0 || len >= buffer.len() {
        return Err(std::io::Error::last_os_error());
    }
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer[..len])))
}

/// Lets a process started suspended run.
#[cfg(windows)]
pub(crate) fn resume(process: &Child) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;

    #[link(name = "ntdll", kind = "raw-dylib")]
    unsafe extern "system" {
        fn NtResumeProcess(process: *mut std::ffi::c_void) -> i32;
    }
    // SAFETY: the handle is the process's, open while `process` is.
    match unsafe { NtResumeProcess(process.as_raw_handle()) } {
        0 => Ok(()),
        status => Err(std::io::Error::other(format!(
            "it couldn't be resumed (NTSTATUS {status:#x})"
        ))),
    }
}

/// Gives Chrome its ends of the pipe as file descriptors 3 and 4, where
/// `--remote-debugging-pipe` expects them.
#[cfg(unix)]
fn hand_over(
    command: &mut Command,
    reads: &PipeReader,
    writes: &PipeWriter,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    command.arg("--remote-debugging-pipe");
    let (reads, writes) = (reads.as_raw_fd(), writes.as_raw_fd());
    // SAFETY: only async-signal-safe calls (fcntl, dup2) between fork and
    // exec.
    unsafe {
        command.pre_exec(move || {
            // Either end may already be 3 or 4; move both out of the way
            // first so the dup2s can't clobber each other. dup2 clears
            // close-on-exec on 3 and 4; the moved copies keep it.
            let reads = libc::fcntl(reads, libc::F_DUPFD_CLOEXEC, 5);
            let writes = libc::fcntl(writes, libc::F_DUPFD_CLOEXEC, 5);
            if reads < 0 || writes < 0 || libc::dup2(reads, 3) < 0 || libc::dup2(writes, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

/// Windows has no numbered descriptors to hand over: the pipe handles are
/// made inheritable and their values passed on the command line.
#[cfg(windows)]
fn hand_over(
    command: &mut Command,
    reads: &PipeReader,
    writes: &PipeWriter,
) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

    // start() keeps netweir's own launches apart, but a process started
    // elsewhere in the program during the launch still inherits these. It
    // holds them, unused, until it ends; the browser reads Chrome's exit
    // from the process instead (see conn::closes_with).
    for handle in [reads.as_raw_handle(), writes.as_raw_handle()] {
        if unsafe { SetHandleInformation(handle as _, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
            == 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    command.arg("--remote-debugging-pipe").arg(format!(
        "--remote-debugging-io-pipes={},{}",
        reads.as_raw_handle() as usize,
        writes.as_raw_handle() as usize
    ));
    Ok(())
}

/// A fresh profile folder netweir made in the temporary directory,
/// removed when this is dropped, and the lock that says it's in use: a
/// file beside the folder, named after it with `.lock`. On macOS and
/// Linux it's an `flock`, which the system lets go of with the process,
/// however it ends; on Windows, the file open sharing only its deletion,
/// which nothing else can open until the process lets go of it. A
/// filesystem with no `flock` gets no lock, and the profile is swept as
/// one from before there were locks.
pub(crate) struct TempProfile {
    dir: PathBuf,
    lock: Option<(PathBuf, std::fs::File)>,
}

impl TempProfile {
    /// Makes one, its lock taken first, so the folder never exists with
    /// it untaken.
    pub(crate) fn make() -> std::io::Result<TempProfile> {
        static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "netweir-chrome-{}-{}-{nanos}",
            std::process::id(),
            COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let lock = take_lock(&dir)?;
        let profile = TempProfile { dir, lock };
        std::fs::create_dir_all(&profile.dir)?;
        Ok(profile)
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for TempProfile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        // The lock goes last, and its file before it closes, so no sweep
        // takes it meanwhile; Windows deletes it once it closes.
        if let Some((path, _file)) = &self.lock {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Takes the lock for the profile `dir`, which doesn't exist yet. It's
/// made under another name and held before it's given its own, so a
/// sweep never finds it untaken.
#[cfg(unix)]
fn take_lock(dir: &Path) -> std::io::Result<Option<(PathBuf, std::fs::File)>> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = lock_of(dir);
    let making = dir.with_extension("lock-new");
    let made = (|| {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&making)?;
        if !try_lock(&file) {
            let e = std::io::Error::last_os_error();
            // Some network and FUSE filesystems have no flock.
            // (ENOTSUP and EOPNOTSUPP are one on Linux, two on macOS.)
            let unlockable = [libc::ENOLCK, libc::EOPNOTSUPP, libc::ENOTSUP, libc::ENOSYS];
            return match e.raw_os_error() {
                Some(code) if unlockable.contains(&code) => Ok(None),
                _ => Err(e),
            };
        }
        std::fs::rename(&making, &path)?;
        Ok(Some((path, file)))
    })();
    if !matches!(made, Ok(Some(_))) {
        let _ = std::fs::remove_file(&making);
    }
    made
}

#[cfg(windows)]
fn take_lock(dir: &Path) -> std::io::Result<Option<(PathBuf, std::fs::File)>> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
    let path = lock_of(dir);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_DELETE)
        .open(&path)?;
    Ok(Some((path, file)))
}

/// The lock file for the profile `dir`.
fn lock_of(dir: &Path) -> PathBuf {
    let mut name = dir.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Takes `file`'s lock if nothing holds it.
#[cfg(unix)]
fn try_lock(file: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: flock on a descriptor `file` owns.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// The lock file `path`, if nothing holds it: opened (never through a
/// link) and locked.
fn untaken(path: &Path) -> Option<std::fs::File> {
    if !std::fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .ok()?;
        try_lock(&file).then_some(file)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Fails while the profile's program holds it: it shares nothing
        // but deletion.
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .ok()
    }
}

/// Runs [`sweep_profiles`] on `std::env::temp_dir()` on a thread of its
/// own, which doesn't hold up the launch, nor the program's end. One
/// sweep at a time: a launch while one runs has it go round once more,
/// for what it may have read too soon to see.
pub(crate) fn sweep_soon() {
    // Whether one runs, and whether it's to go round again.
    static SWEEPING: std::sync::Mutex<(bool, bool)> = std::sync::Mutex::new((false, false));
    let state = || SWEEPING.lock().unwrap_or_else(|e| e.into_inner());
    {
        let mut sweeping = state();
        if sweeping.0 {
            sweeping.1 = true;
            return;
        }
        *sweeping = (true, false);
    }
    let started = std::thread::Builder::new()
        .name("netweir-profile-sweep".into())
        .spawn(move || {
            loop {
                sweep_profiles(&std::env::temp_dir());
                let mut sweeping = state();
                if !std::mem::take(&mut sweeping.1) {
                    sweeping.0 = false;
                    return;
                }
            }
        });
    if started.is_err() {
        state().0 = false;
    }
}

/// Removes the profiles in `temp` that programs ended without removing,
/// as one killed or crashed can't. Only folders named as netweir names
/// its own (`netweir-chrome-<pid>-<count>-<nanos>`) are looked at, and
/// links are never followed, so a profile someone gave is never touched.
/// One whose lock can be taken is no longer in use. One with no lock is
/// from before there were any, and goes once the process that made it
/// has: the only test there is, which is wrong where processes from more
/// than one container share the folder, so a profile made since always
/// has a lock.
pub(crate) fn sweep_profiles(temp: &Path) {
    let Ok(entries) = std::fs::read_dir(temp) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let path = entry.path();
        if let Some(made) = name.strip_suffix(".lock").filter(|n| left_by(n).is_some()) {
            let dir = temp.join(made);
            let Some(lock) = untaken(&path) else { continue };
            if std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
                let _ = std::fs::remove_dir_all(&dir);
            }
            if cfg!(windows) {
                drop(lock);
            }
            let _ = std::fs::remove_file(&path);
        } else if let Some(pid) = name.strip_suffix(".lock-new").and_then(left_by) {
            // A lock a program ended before naming. One being named right
            // now is in the moment between being made and being taken, so
            // its program must have gone too.
            if pid != std::process::id() && !running(pid) && untaken(&path).is_some() {
                let _ = std::fs::remove_file(&path);
            }
        } else if let Some(pid) = left_by(name) {
            let old = std::fs::symlink_metadata(lock_of(&path)).is_err()
                && std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
            if old && pid != std::process::id() && !running(pid) {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
}

/// The process that made a profile named `name`, if netweir named it:
/// `netweir-chrome-<pid>-<count>-<nanos>`, all three numbers.
fn left_by(name: &str) -> Option<u32> {
    let parts: Vec<&str> = name.strip_prefix("netweir-chrome-")?.split('-').collect();
    let [pid, count, nanos] = parts[..] else {
        return None;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !(digits(count) && digits(nanos) && digits(pid)) {
        return None;
    }
    pid.parse().ok().filter(|&pid| pid > 0)
}

/// Whether process `pid` runs. One netweir isn't allowed to look at
/// counts as running.
#[cfg(unix)]
pub(crate) fn running(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks; pid is positive, so it names one
    // process, never a group.
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

/// Whether process `pid` runs. One netweir isn't allowed to look at
/// counts as running.
#[cfg(windows)]
pub(crate) fn running(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, STILL_ACTIVE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: the handle is checked and closed.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return GetLastError() != ERROR_INVALID_PARAMETER;
        }
        let mut code = 0;
        let ok = GetExitCodeProcess(process, &mut code) != 0;
        CloseHandle(process);
        !ok || code == STILL_ACTIVE as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pipe made while another thread starts processes is none of
    /// theirs: once its writer is dropped, its reader reads the end, with
    /// every process started meanwhile still running. A Chrome that came
    /// by another's end of its pipe would hide that one's death.
    #[cfg(unix)]
    #[test]
    fn a_pipe_made_while_processes_start_reaches_none_of_them() {
        use std::io::Read;
        use std::os::fd::AsRawFd;

        // Processes lasting ten seconds, past each check below, started
        // for two, one every 5 ms. Started as fast as they would go, they
        // reached a macOS runner's limit on processes, and Chrome, in the
        // tests running alongside, couldn't start its own.
        let starter = std::thread::spawn(|| {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
            std::iter::from_fn(|| (std::time::Instant::now() < until).then_some(()))
                .map(|_| {
                    let child = Command::new("sleep")
                        .arg("10")
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    child
                })
                .collect::<Vec<_>>()
        });
        let ended = |reader: &mut PipeReader| {
            let read = reader.read(&mut [0]);
            !matches!(read, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
        };
        // Pipes made on eight threads at once, so most moments have one
        // being made.
        let held: usize = std::thread::scope(|s| {
            let makers: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        let mut held = 0;
                        while !starter.is_finished() {
                            let (mut reader, writer) = pipe().unwrap();
                            drop(writer);
                            unsafe {
                                libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK)
                            };
                            // A process still starting holds it until it
                            // runs, whatever it's marked, and with eight
                            // threads making pipes a forked one can wait
                            // well past 100 ms for a core; one that came by
                            // it holds it for good.
                            let until =
                                std::time::Instant::now() + std::time::Duration::from_secs(2);
                            while !ended(&mut reader) {
                                if std::time::Instant::now() > until {
                                    held += 1;
                                    break;
                                }
                                std::thread::sleep(std::time::Duration::from_millis(10));
                            }
                        }
                        held
                    })
                })
                .collect();
            makers.into_iter().map(|m| m.join().unwrap()).sum()
        });
        for mut child in starter.join().unwrap() {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert_eq!(
            held, 0,
            "{held} pipes were held by processes started meanwhile"
        );
    }

    #[test]
    fn switches_from_the_environment_are_split_on_whitespace() {
        assert_eq!(
            switches_from(Some("--disable-gpu  --no-sandbox\n".into())),
            ["--disable-gpu", "--no-sandbox"]
        );
        assert!(switches_from(Some("   ".into())).is_empty());
        assert!(switches_from(None).is_empty());
    }

    #[test]
    fn features_join_those_already_asked_for() {
        let args = enable_features(
            vec![
                "--enable-features=A,B".into(),
                "--headless".into(),
                "--enable-features=C".into(),
            ],
            &["Ours"],
        );
        assert_eq!(args, ["--headless", "--enable-features=Ours,A,B,C"]);
        assert_eq!(
            enable_features(vec![], &["Ours"]),
            ["--enable-features=Ours"]
        );
        // A single dash is the same switch; after "--" nothing is one.
        assert_eq!(
            enable_features(strings(&["-enable-features=A", "--", "page"]), &["Ours"]),
            ["--enable-features=Ours,A", "--", "page"]
        );
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("netweir-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn prediction(file: &Path) -> serde_json::Value {
        let prefs: serde_json::Value =
            serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
        prefs["net"]["network_prediction_options"].clone()
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn switches_are_read_as_chrome_reads_them() {
        let args = strings(&[
            "--profile-directory=A",
            "-profile-directory=B",
            " --other ",
            "--",
            "--profile-directory=after-the-end",
        ]);
        assert_eq!(
            last_switch(&args, "profile-directory").as_deref(),
            Some("B")
        );
        assert_eq!(last_switch(&args, "other").as_deref(), Some(""));
        assert_eq!(last_switch(&args, "missing"), None);
        let mut args = strings(&[
            "-user-data-dir=a",
            "x",
            "--user-data-dir=b=c",
            "--",
            "-user-data-dir=d",
        ]);
        assert_eq!(take_switches(&mut args, "user-data-dir"), ["a", "b=c"]);
        assert_eq!(args, ["x", "--", "-user-data-dir=d"]);
        // A slash, and capitals, count on Windows only, as in Chrome.
        let args = strings(&["/User-Data-Dir=w"]);
        assert_eq!(last_switch(&args, "user-data-dir").is_some(), cfg!(windows));
        let args = strings(&["--User-Data-Dir=w"]);
        assert_eq!(last_switch(&args, "user-data-dir").is_some(), cfg!(windows));
    }

    #[test]
    fn a_profile_name_must_be_a_folder_in_the_user_data_dir() {
        let dir = scratch("bad-profile");
        let mut bad = vec!["", ".", "..", "../outside", "a/b", "a\\b", "/abs"];
        if cfg!(windows) {
            bad.extend(["C:\\abs", "C:rel"]);
        }
        for name in bad {
            let args = [format!("--profile-directory={name}")];
            let err = profile_in(&dir, &args).unwrap_err().to_string();
            assert!(err.contains("profile"), "{name}: {err}");
            assert!(turn_off_preloading(&dir, &args).is_err(), "{name}");
            std::fs::write(
                dir.join("Local State"),
                serde_json::json!({"profile": {"last_used": name}}).to_string(),
            )
            .unwrap();
            // An empty name in Local State is Chrome's default.
            assert_eq!(profile_in(&dir, &[]).is_err(), !name.is_empty(), "{name}");
        }
        std::fs::remove_file(dir.join("Local State")).unwrap();
        // Nothing was written anywhere.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        assert!(!dir.parent().unwrap().join("outside").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_profile_is_the_one_chrome_opens() {
        let dir = scratch("which-profile");
        assert_eq!(profile_in(&dir, &[]).unwrap(), "Default");
        std::fs::write(
            dir.join("Local State"),
            r#"{"profile":{"last_used":"Profile 1"}}"#,
        )
        .unwrap();
        assert_eq!(profile_in(&dir, &[]).unwrap(), "Profile 1");
        let args = ["--profile-directory=Work".to_string()];
        assert_eq!(profile_in(&dir, &args).unwrap(), "Work");
        std::fs::write(dir.join("Local State"), "{not json").unwrap();
        assert_eq!(profile_in(&dir, &[]).unwrap(), "Default");

        std::fs::write(
            dir.join("Local State"),
            r#"{"profile":{"last_used":"Profile 1"}}"#,
        )
        .unwrap();
        turn_off_preloading(&dir, &[]).unwrap();
        assert_eq!(prediction(&dir.join("Profile 1/Preferences")), 2);
        assert!(!dir.join("Default").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_preferences_file_is_chromes_to_recover() {
        let dir = scratch("empty-prefs");
        std::fs::create_dir_all(dir.join("Default")).unwrap();
        let file = dir.join("Default/Preferences");
        for empty in ["", " \n"] {
            std::fs::write(&file, empty).unwrap();
            turn_off_preloading(&dir, &[]).unwrap();
            assert_eq!(prediction(&file), 2);
        }
        std::fs::write(&file, "{\"net\":").unwrap();
        let err = turn_off_preloading(&dir, &[]).unwrap_err().to_string();
        assert!(err.contains("isn't valid JSON"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_file_keeps_its_permissions_and_its_link() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("prefs-mode");
        let file = dir.join("Default/Preferences");
        // A new one is private, as Chrome makes it.
        turn_off_preloading(&dir, &[]).unwrap();
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::write(&file, "{}").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        turn_off_preloading(&dir, &[]).unwrap();
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o640
        );
        // A linked file is changed where it is, and stays linked.
        let real = dir.join("elsewhere.json");
        std::fs::write(&real, "{}").unwrap();
        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(&real, &file).unwrap();
        turn_off_preloading(&dir, &[]).unwrap();
        assert!(
            std::fs::symlink_metadata(&file)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(prediction(&real), 2);
        assert!(!dir.join("Default/Preferences.netweir").exists());
        assert!(!dir.join("elsewhere.netweir").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn preloading_is_turned_off_and_the_rest_kept() {
        let dir = std::env::temp_dir().join(format!("netweir-prefs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let read = |p: &str| -> serde_json::Value {
            serde_json::from_slice(&std::fs::read(dir.join(p).join("Preferences")).unwrap())
                .unwrap()
        };
        // A new profile.
        turn_off_preloading(&dir, &[]).unwrap();
        assert_eq!(read("Default")["net"]["network_prediction_options"], 2);
        // Someone's own, with the setting on and more besides.
        let own = dir.join("Work");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(
            own.join("Preferences"),
            r#"{"net":{"network_prediction_options":0,"other":1},"homepage":"x"}"#,
        )
        .unwrap();
        let args = ["--profile-directory=Work".to_string()];
        turn_off_preloading(&dir, &args).unwrap();
        let prefs = read("Work");
        assert_eq!(prefs["net"]["network_prediction_options"], 2);
        assert_eq!(
            (&prefs["net"]["other"], &prefs["homepage"]),
            (&1.into(), &"x".into())
        );
        // One that isn't Chrome's is left alone.
        std::fs::write(own.join("Preferences"), "[1]").unwrap();
        assert!(turn_off_preloading(&dir, &args).is_err());
        assert_eq!(std::fs::read(own.join("Preferences")).unwrap(), b"[1]");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn chrome_refusing_its_own_profile_is_heard() {
        assert!(refuses_its_profile(
            "[1019/101010.123456:ERROR:chrome/browser/devtools/remote_debugging_server.cc:\
             143] DevTools remote debugging requires a non-default data directory. Specify \
             this using --user-data-dir."
        ));
        assert!(!refuses_its_profile(
            "DevTools listening on ws://127.0.0.1:9222/devtools/browser/x"
        ));
    }

    /// Google Chrome where its installers put it, with each one's default
    /// profile, and Chromes that aren't Google Chrome.
    #[cfg(target_os = "macos")]
    fn chromes() -> (Vec<(PathBuf, PathBuf)>, Vec<PathBuf>) {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let support = home.join("Library/Application Support/Google");
        let app =
            |name: &str| PathBuf::from(format!("/Applications/{name}.app/Contents/MacOS/{name}"));
        (
            vec![
                (app("Google Chrome"), support.join("Chrome")),
                (app("Google Chrome Beta"), support.join("Chrome Beta")),
                (app("Google Chrome Canary"), support.join("Chrome Canary")),
            ],
            vec![app("Google Chrome for Testing"), app("Chromium")],
        )
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn chromes() -> (Vec<(PathBuf, PathBuf)>, Vec<PathBuf>) {
        let config = std::env::var_os("CHROME_CONFIG_HOME")
            .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".config"));
        (
            vec![
                (
                    "/opt/google/chrome/chrome".into(),
                    config.join("google-chrome"),
                ),
                (
                    "/opt/google/chrome-beta/chrome".into(),
                    config.join("google-chrome-beta"),
                ),
            ],
            vec![
                "/usr/lib/chromium/chromium".into(),
                "/opt/chrome/chrome".into(),
            ],
        )
    }

    #[cfg(windows)]
    fn chromes() -> (Vec<(PathBuf, PathBuf)>, Vec<PathBuf>) {
        let local = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap());
        (
            vec![
                (
                    r"C:\Program Files\Google\Chrome\Application\chrome.exe".into(),
                    local.join(r"Google\Chrome\User Data"),
                ),
                (
                    local.join(r"Google\Chrome SxS\Application\chrome.exe"),
                    local.join(r"Google\Chrome SxS\User Data"),
                ),
            ],
            vec![local.join(r"Google\Chrome for Testing\Application\chrome.exe")],
        )
    }

    #[test]
    fn google_chromes_own_profile_is_refused_before_launch() {
        let elsewhere = scratch("own-profile");
        let (google, others) = chromes();
        for (chrome, own) in &google {
            assert!(check_profile(chrome, own).is_err(), "{}", chrome.display());
            assert!(check_profile(chrome, &elsewhere).is_ok());
            // The same folder by another name, where it's there to follow.
            if own.is_dir() {
                assert!(check_profile(chrome, &own.join(".")).is_err());
            }
        }
        // Chrome for Testing and Chromium are driven with any profile.
        for chrome in &others {
            for (_, own) in &google {
                assert!(check_profile(chrome, own).is_ok(), "{}", chrome.display());
            }
        }
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn the_sandbox_grant_names_app_packages_by_sid() {
        let args: Vec<String> = sandbox_grant_args(Path::new("C:/n/chrome/154.0.1.2"))
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "C:/n/chrome/154.0.1.2",
                "/grant",
                "*S-1-15-2-1:(OI)(CI)(RX)",
                "/grant",
                "*S-1-15-2-2:(OI)(CI)(RX)",
                "/Q",
            ]
        );
    }

    #[test]
    fn only_chrome_netweir_installed_counts_as_its_own() {
        let home = Path::new("/h/.netweir");
        let exe = home.join("chrome/154.0.1.2/chrome-win64/chrome.exe");
        assert_eq!(
            install_dir_of(&exe, home),
            Some(home.join("chrome").join("154.0.1.2"))
        );
        assert_eq!(
            install_dir_of(Path::new("/Program Files/Google/Chrome/chrome.exe"), home),
            None
        );
        // Not one of netweir's versions, though under its folder.
        assert_eq!(install_dir_of(&home.join("chrome/chrome.exe"), home), None);
    }

    #[test]
    fn the_newest_installed_chrome_is_found() {
        let Some(platform) = cft_platform() else {
            return;
        };
        let home = std::env::temp_dir().join(format!("netweir-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(installed_chrome(&home), None);
        for version in [
            "154.0.8037.98",
            "154.0.8037.110",
            "153.0.1.1",
            "not-a-version",
        ] {
            let exe = home
                .join("chrome")
                .join(version)
                .join(cft_executable(platform, false));
            std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
            std::fs::write(&exe, "").unwrap();
        }
        let found = installed_chrome(&home).unwrap();
        assert!(
            found.starts_with(home.join("chrome").join("154.0.8037.110")),
            "{}",
            found.display()
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    /// A helper as Chrome starts one, in a job of its own inside Chrome's
    /// and at idle priority, holding a file in the profile: once Chrome
    /// is gone and the lifeline let go, it's gone too, and the file free.
    #[cfg(windows)]
    #[test]
    fn a_helper_holding_the_profile_is_gone_with_chrome() {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
        use windows_sys::Win32::System::Threading::{IDLE_PRIORITY_CLASS, SetPriorityClass};

        let dir = scratch("held");
        let held = std::fs::File::create(dir.join("held")).unwrap();
        let mut helper = Command::new(system_dir().unwrap().join("PING.EXE"))
            .args(["-n", "60", "127.0.0.1"])
            .stdout(held)
            .spawn()
            .unwrap();
        let lifeline = tie(&helper);
        // SAFETY: the handle is checked and owned from here on; the
        // helper's is open while `helper` is.
        let _inner = unsafe {
            let inner = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            assert!(!inner.is_null());
            let inner = OwnedHandle::from_raw_handle(inner as _);
            assert_ne!(
                AssignProcessToJobObject(inner.as_raw_handle() as _, helper.as_raw_handle() as _),
                0
            );
            assert_ne!(
                SetPriorityClass(helper.as_raw_handle() as _, IDLE_PRIORITY_CLASS),
                0
            );
            inner
        };
        lifeline.process_gone();
        let gone = helper.try_wait().unwrap();
        let removed = std::fs::remove_dir_all(&dir);
        let _ = helper.kill();
        let _ = helper.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gone.is_some(), "the helper outlived the lifeline");
        removed.unwrap();
    }
}
