//! `netweir install chrome` against a local stand-in for Google's Chrome
//! for Testing index.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use netweir_browser::{cft_executable, cft_platform};
use netweir_core::install::{FirefoxRelease, install_chrome, install_firefox};

/// The paths a server was asked for.
type Hits = Arc<Mutex<Vec<String>>>;

/// Serves `files`, and an index listing builds for `platform` that point
/// back at this server. Returns its address and the paths requested.
fn serve(platform: &str, mut files: Vec<(&'static str, Vec<u8>)>) -> (String, Hits) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    files.push(("/index.json", index(&base, platform)));
    let hits: Hits = Arc::default();
    let log = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                    break;
                }
            }
            let path = line.split(' ').nth(1).unwrap_or("/").to_string();
            log.lock().unwrap().push(path.clone());
            let (status, body) = match files.iter().find(|(p, _)| *p == path) {
                Some((_, b)) => ("200 OK", b.clone()),
                None => ("404 Not Found", b"missing".to_vec()),
            };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    (base, hits)
}

/// The milestone of netweir's newest Chrome profile.
fn milestone() -> String {
    let name = netweir_core::Profile::named("chrome").unwrap().name;
    name.split('-').nth(1).unwrap().to_string()
}

fn index(base: &str, platform: &str) -> Vec<u8> {
    let milestone = milestone();
    serde_json::json!({
        "timestamp": "2026-10-01T00:00:00Z",
        "milestones": {
            "153": {"milestone": "153", "version": "153.0.1.1", "downloads": {}},
            milestone.clone(): {
                "milestone": milestone,
                "version": format!("{milestone}.0.9000.1"),
                "downloads": {
                    "chrome": [
                        {"platform": "elsewhere", "url": format!("{base}/wrong.zip")},
                        {"platform": platform, "url": format!("{base}/chrome.zip")},
                    ],
                    "chrome-headless-shell": [
                        {"platform": platform, "url": format!("{base}/shell.zip")},
                    ],
                },
            },
        },
    })
    .to_string()
    .into_bytes()
}

/// A zip laid out like a Chrome for Testing download for `platform`.
fn fake_download(platform: &str, headless_shell: bool) -> Vec<u8> {
    use zip::write::SimpleFileOptions;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let exe = cft_executable(platform, headless_shell);
    zip.start_file(
        exe.to_string_lossy(),
        SimpleFileOptions::default().unix_permissions(0o755),
    )
    .unwrap();
    zip.write_all(b"#!/bin/sh\necho chrome\n").unwrap();
    // A link, as the macOS app bundle has, which must stay a link.
    let dir = exe.parent().unwrap().to_string_lossy().to_string();
    zip.add_symlink(format!("{dir}/Current"), ".", SimpleFileOptions::default())
        .unwrap();
    zip.finish().unwrap().into_inner()
}

#[tokio::test]
async fn chrome_for_testing_is_installed_once_where_find_chrome_looks() {
    let Some(platform) = cft_platform() else {
        return;
    };
    let (base, hits) = serve(
        platform,
        vec![
            ("/chrome.zip", fake_download(platform, false)),
            ("/shell.zip", fake_download(platform, true)),
        ],
    );
    let home = std::env::temp_dir().join(format!("netweir-install-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let index = format!("{base}/index.json");

    let installed = install_chrome(&index, &home, false).await.unwrap();
    assert!(!installed.already);
    assert_eq!(installed.version, format!("{}.0.9000.1", milestone()));
    let exe = home
        .join("chrome")
        .join(&installed.version)
        .join(cft_executable(platform, false));
    assert_eq!(installed.executable, exe);
    assert!(exe.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(exe.metadata().unwrap().permissions().mode() & 0o111, 0o111);
        let link = exe.parent().unwrap().join("Current");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    }

    let again = install_chrome(&index, &home, false).await.unwrap();
    assert!(again.already);
    let downloads = hits
        .lock()
        .unwrap()
        .iter()
        .filter(|p| *p == "/chrome.zip")
        .count();
    assert_eq!(downloads, 1, "not downloaded twice");

    let shell = install_chrome(&index, &home, true).await.unwrap();
    assert_eq!(
        shell.executable,
        home.join("chrome")
            .join(&shell.version)
            .join(cft_executable(platform, true))
    );
    assert!(shell.executable.is_file());
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn a_platform_with_no_build_says_so_and_installs_nothing() {
    let (base, _) = serve("nowhere-arm", vec![]);
    let home = std::env::temp_dir().join(format!("netweir-install-none-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let err = install_chrome(&format!("{base}/index.json"), &home, false)
        .await
        .unwrap_err();
    assert!(err.contains("no Chrome for Testing"), "{err}");
    assert!(!home.join("chrome").exists());
}

#[tokio::test]
async fn a_version_that_isnt_a_version_isnt_made_a_folder() {
    let Some(platform) = cft_platform() else {
        return;
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let index = serde_json::json!({"milestones": {milestone(): {
        "version": "../../escaped",
        "downloads": {"chrome": [{"platform": platform, "url": format!("{base}/chrome.zip")}]},
    }}})
    .to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                    break;
                }
            }
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{index}",
                index.len()
            );
        }
    });
    let home = std::env::temp_dir().join(format!("netweir-install-bad-{}", std::process::id()));
    let err = install_chrome(&format!("{base}/index.json"), &home, false)
        .await
        .unwrap_err();
    assert!(err.contains("escaped"), "{err}");
    assert!(!home.exists());
}

#[tokio::test]
async fn a_broken_install_is_replaced_and_leftovers_swept() {
    let Some(platform) = cft_platform() else {
        return;
    };
    let (base, _) = serve(
        platform,
        vec![("/chrome.zip", fake_download(platform, false))],
    );
    let home = std::env::temp_dir().join(format!("netweir-install-fix-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let version = format!("{}.0.9000.1", milestone());
    // An earlier install whose executable is gone, and the half-unpacked
    // folder of one that was killed.
    let top = cft_executable(platform, false)
        .components()
        .next()
        .unwrap()
        .as_os_str()
        .to_owned();
    std::fs::create_dir_all(home.join("chrome").join(&version).join(&top)).unwrap();
    let leftover = home.join("chrome").join(format!(".{version}-chrome-1"));
    std::fs::create_dir_all(&leftover).unwrap();
    // Windows can't open a folder to date it; the sweep is the same there.
    #[cfg(unix)]
    {
        let hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        std::fs::File::open(&leftover)
            .unwrap()
            .set_modified(hours_ago)
            .unwrap();
    }
    let running = home.join("chrome").join(format!(".{version}-chrome-2"));
    std::fs::create_dir_all(&running).unwrap();

    let installed = install_chrome(&format!("{base}/index.json"), &home, false)
        .await
        .unwrap();
    assert!(installed.executable.is_file());
    #[cfg(unix)]
    assert!(!leftover.exists());
    assert!(running.exists(), "an install under way is left alone");
    std::fs::remove_dir_all(&home).unwrap();
}

/// An archive laid out as Mozilla's download for this machine, holding a
/// stand-in executable, made with the tools the installer unpacks with;
/// None on Windows, whose download is an installer that can't be faked.
fn fake_firefox() -> Option<(&'static str, Vec<u8>)> {
    // Tests run at once, so each gets a folder of its own.
    static MADE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = MADE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let work = std::env::temp_dir().join(format!("netweir-fake-ff-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let tree = work.join("tree");
    let exe = netweir_browser::firefox_executable(&tree);
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(&exe, "#!/bin/sh\necho firefox\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let (name, archive) = if cfg!(target_os = "macos") {
        let dmg = work.join("ff.dmg");
        let ok = std::process::Command::new("hdiutil")
            .args(["create", "-quiet", "-format", "UDZO", "-srcfolder"])
            .arg(&tree)
            .arg(&dmg)
            .status()
            .unwrap()
            .success();
        assert!(ok, "hdiutil create");
        ("Firefox%20159.0.1.dmg", dmg)
    } else if cfg!(windows) {
        let _ = std::fs::remove_dir_all(&work);
        return None;
    } else {
        let tar = work.join("ff.tar.xz");
        let ok = std::process::Command::new("tar")
            .arg("-cJf")
            .arg(&tar)
            .arg("-C")
            .arg(&tree)
            .arg("firefox")
            .status()
            .unwrap()
            .success();
        assert!(ok, "tar -cJf");
        ("firefox-159.0.1.tar.xz", tar)
    };
    let bytes = std::fs::read(&archive).unwrap();
    std::fs::remove_dir_all(&work).unwrap();
    Some((name, bytes))
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// This machine's platform, pinned to `sum`.
fn pin(sum: &str) -> [(&'static str, &str); 1] {
    [(netweir_browser::firefox_platform().unwrap(), sum)]
}

fn release<'a>(sums: &'a [(&'a str, &'a str)]) -> FirefoxRelease<'a> {
    FirefoxRelease {
        version: "159.0.1",
        sums,
    }
}

fn firefox_path(name: &str) -> String {
    let platform = netweir_browser::firefox_platform().unwrap();
    format!("/159.0.1/{platform}/en-US/{name}")
}

#[tokio::test]
async fn firefox_installs_once_with_its_startup_files() {
    let Some((name, archive)) = fake_firefox() else {
        return;
    };
    let sum = sha256(&archive);
    let path: &'static str = String::leak(firefox_path(name));
    let (base, hits) = serve("none", vec![(path, archive)]);
    let home = std::env::temp_dir().join(format!("netweir-install-ff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let sums = pin(&sum);
    let release = release(&sums);

    let installed = install_firefox(&base, &home, &release).await.unwrap();
    assert!(!installed.already);
    assert_eq!(installed.version, "159.0.1");
    assert_eq!(
        installed.executable,
        netweir_browser::firefox_executable(&home.join("firefox/159.0.1"))
    );
    assert!(installed.executable.is_file());
    let resources = if cfg!(target_os = "macos") {
        installed
            .executable
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("Resources")
    } else {
        installed.executable.parent().unwrap().to_path_buf()
    };
    assert!(resources.join("mozilla.cfg").is_file());
    assert!(resources.join("defaults/pref/autoconfig.js").is_file());
    // Nothing but the version folder is left: no download, no mount.
    let left: Vec<_> = std::fs::read_dir(home.join("firefox"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(left, ["159.0.1"]);

    let again = install_firefox(&base, &home, &release).await.unwrap();
    assert!(again.already);
    let downloads = hits.lock().unwrap().iter().filter(|p| *p == path).count();
    assert_eq!(downloads, 1, "not downloaded twice");
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn a_firefox_download_with_the_wrong_sum_is_refused() {
    let Some((name, archive)) = fake_firefox() else {
        return;
    };
    let path: &'static str = String::leak(firefox_path(name));
    let (base, _) = serve("none", vec![(path, archive)]);
    let home = std::env::temp_dir().join(format!("netweir-install-ffsum-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let err = install_firefox(&base, &home, &release(&pin(&"0".repeat(64))))
        .await
        .unwrap_err();
    assert!(err.contains("SHA-256"), "{err}");
    assert!(err.contains("nothing was installed"), "{err}");
    let left = std::fs::read_dir(home.join("firefox")).map_or(0, |d| d.count());
    assert_eq!(left, 0, "nothing unpacked");
}

#[tokio::test]
async fn no_firefox_for_a_platform_netweir_has_no_sum_for() {
    let Some(platform) = netweir_browser::firefox_platform() else {
        return;
    };
    let (base, hits) = serve("none", vec![]);
    let home = std::env::temp_dir().join(format!("netweir-install-ffnone-{}", std::process::id()));
    let nowhere = FirefoxRelease {
        version: "159.0.1",
        sums: &[("elsewhere", "00")],
    };
    let err = install_firefox(&base, &home, &nowhere).await.unwrap_err();
    assert!(err.contains(platform), "{err}");
    assert!(hits.lock().unwrap().is_empty(), "nothing fetched");
    assert!(!home.exists());
}

#[tokio::test]
async fn a_firefox_version_that_isnt_one_isnt_made_a_folder() {
    let (base, hits) = serve("none", vec![]);
    let home = std::env::temp_dir().join(format!("netweir-install-ffbad-{}", std::process::id()));
    for version in [
        "../../escaped",
        "",
        "160.0b",
        "160.0b5b1",
        "160.b5.0",
        "162.0a1",
        "160.0b/5",
    ] {
        let bad = FirefoxRelease { version, sums: &[] };
        let err = install_firefox(&base, &home, &bad).await.unwrap_err();
        assert!(
            err.contains("isn't a Firefox release version"),
            "{version}: {err}"
        );
    }
    assert!(hits.lock().unwrap().is_empty());
    assert!(!home.exists());
}

#[tokio::test]
async fn a_firefox_beta_is_a_version() {
    let Some(platform) = netweir_browser::firefox_platform() else {
        return;
    };
    let (base, hits) = serve("none", vec![]);
    let home = std::env::temp_dir().join(format!("netweir-install-ffbeta-{}", std::process::id()));
    let beta = FirefoxRelease {
        version: "160.0b5",
        sums: &[("elsewhere", "00")],
    };
    // Past the version check, to the checksum this platform hasn't got.
    let err = install_firefox(&base, &home, &beta).await.unwrap_err();
    assert!(
        err.contains(&format!("Firefox 160.0b5 on {platform}")),
        "{err}"
    );
    assert!(hits.lock().unwrap().is_empty(), "nothing fetched");
    assert!(!home.exists());
}

/// The real thing, from Mozilla, as `netweir install firefox` gets it.
#[tokio::test]
#[ignore = "downloads Firefox from archive.mozilla.org"]
async fn real_firefox_installs() {
    let home = std::env::temp_dir().join(format!("netweir-install-real-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let installed = install_firefox(
        netweir_core::install::FIREFOX_RELEASES,
        &home,
        &netweir_core::install::FIREFOX,
    )
    .await
    .unwrap();
    assert!(
        installed.executable.is_file(),
        "{}",
        installed.executable.display()
    );
    eprintln!("installed {}", installed.executable.display());
}
