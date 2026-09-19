//! Stub `.app` bundles that give each mirror helper a real Dock identity.
//! The coordinator spawns the bundle's inner executable directly; macOS
//! resolves the enclosing bundle and grants Dock label, Cmd-Tab entry,
//! menu-bar name, and minimize-into-own-tile.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub struct StubBundle {
    pub dir: PathBuf,
    pub exe: PathBuf,
}

/// Session-independent cache root for stub bundles.
pub fn mirrors_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join("Library/Application Support/Shebbak/Mirrors")
}

/// Filesystem-safe bundle directory stem (not the Dock label).
fn dir_stem(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c == '/' || c == ':' { '-' } else { c })
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        "Mirror".into()
    } else {
        cleaned
    }
}

/// Reverse-DNS-safe identifier segment.
fn bundle_slug(stem: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for c in stem.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
    }
    let slug = slug.trim_end_matches('-').to_string();
    if slug.is_empty() {
        "mirror".into()
    } else {
        slug
    }
}

/// Create or refresh `<mirrors_dir>/<Name>.app`, suffixing " 2", " 3", …
/// while `taken` claims the directory for a different app this session.
/// The plist is always rewritten (cheap; handles host-side renames) and
/// the helper binary is re-copied (unlink first: copying over a running
/// helper's executable would fail with ETXTBSY).
pub fn ensure_bundle(
    mirrors_dir: &Path,
    display_name: &str,
    helper_src: &Path,
    taken: &HashSet<PathBuf>,
) -> Result<StubBundle> {
    let stem = dir_stem(display_name);
    let (dir, exe_name) = (2..)
        .map(|n| {
            let candidate = if n == 2 {
                stem.clone()
            } else {
                format!("{stem} {}", n - 1)
            };
            (mirrors_dir.join(format!("{candidate}.app")), candidate)
        })
        .find(|(dir, _)| !taken.contains(dir))
        .expect("unbounded iterator always yields");

    let macos_dir = dir.join("Contents/MacOS");
    std::fs::create_dir_all(&macos_dir).context("create bundle dirs")?;

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>
	<string>{display}</string>
	<key>CFBundleDisplayName</key>
	<string>{display}</string>
	<key>CFBundleIdentifier</key>
	<string>io.shebbak.mirror.{slug}</string>
	<key>CFBundleExecutable</key>
	<string>{exe_name}</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
</dict>
</plist>
"#,
        display = xml_escape(display_name),
        slug = bundle_slug(&exe_name),
        exe_name = xml_escape(&exe_name),
    );
    std::fs::write(dir.join("Contents/Info.plist"), plist).context("write Info.plist")?;

    let exe = macos_dir.join(&exe_name);
    match std::fs::remove_file(&exe) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("unlink stale helper copy"),
    }
    std::fs::copy(helper_src, &exe).context("copy helper binary into bundle")?;
    Ok(StubBundle { dir, exe })
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn fake_helper(dir: &std::path::Path) -> std::path::PathBuf {
        let src = dir.join("srw-mirror-helper");
        std::fs::write(&src, b"#!/bin/sh\nexit 0\n").unwrap();
        // Executable bit, so the copy-preserves-permissions assertion means something.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).unwrap();
        src
    }

    #[test]
    fn creates_bundle_with_plist_and_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let helper = fake_helper(tmp.path());
        let b = ensure_bundle(tmp.path(), "Safari", &helper, &HashSet::new()).unwrap();
        assert_eq!(b.dir, tmp.path().join("Safari.app"));
        assert_eq!(b.exe, b.dir.join("Contents/MacOS/Safari"));
        let plist = std::fs::read_to_string(b.dir.join("Contents/Info.plist")).unwrap();
        assert!(plist.contains("<string>Safari</string>"), "CFBundleName");
        assert!(plist.contains("io.shebbak.mirror.safari"), "namespaced id");
        assert!(!plist.contains("LSUIElement"), "must be a regular Dock app");
        assert_eq!(
            std::fs::read(&b.exe).unwrap(),
            std::fs::read(&helper).unwrap()
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&b.exe).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0, "helper copy must stay executable");
    }

    #[test]
    fn name_collision_suffixes_the_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let helper = fake_helper(tmp.path());
        let first = ensure_bundle(tmp.path(), "Safari", &helper, &HashSet::new()).unwrap();
        let taken: HashSet<_> = [first.dir.clone()].into();
        let second = ensure_bundle(tmp.path(), "Safari", &helper, &taken).unwrap();
        assert_eq!(second.dir, tmp.path().join("Safari 2.app"));
        assert_eq!(second.exe, second.dir.join("Contents/MacOS/Safari 2"));
        // Dock label stays the display name for both (spec: labels may collide).
        let plist = std::fs::read_to_string(second.dir.join("Contents/Info.plist")).unwrap();
        assert!(plist.contains("<key>CFBundleName</key>\n\t<string>Safari</string>"));
    }

    #[test]
    fn reuse_refreshes_binary_and_plist() {
        let tmp = tempfile::tempdir().unwrap();
        let helper = fake_helper(tmp.path());
        let b1 = ensure_bundle(tmp.path(), "Safari", &helper, &HashSet::new()).unwrap();
        std::fs::write(&helper, b"#!/bin/sh\nexit 1\n").unwrap();
        let b2 = ensure_bundle(tmp.path(), "Safari", &helper, &HashSet::new()).unwrap();
        assert_eq!(b1.dir, b2.dir, "same app reuses the cached bundle dir");
        assert_eq!(
            std::fs::read(&b2.exe).unwrap(),
            std::fs::read(&helper).unwrap()
        );
    }

    #[test]
    fn hostile_names_are_sanitized() {
        let tmp = tempfile::tempdir().unwrap();
        let helper = fake_helper(tmp.path());
        let b = ensure_bundle(tmp.path(), "My/App: Weird", &helper, &HashSet::new()).unwrap();
        assert!(b.dir.starts_with(tmp.path()));
        assert!(!b.dir.file_name().unwrap().to_str().unwrap().contains('/'));
        let plist = std::fs::read_to_string(b.dir.join("Contents/Info.plist")).unwrap();
        assert!(plist.contains("io.shebbak.mirror.my-app-weird"));
    }
}
