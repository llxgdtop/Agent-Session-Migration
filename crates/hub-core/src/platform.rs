//! Cross-platform integration data for hub-core consumers.
//!
//! Currently exposes the CJK font candidates used by hub-app to install a
//! UI font that renders Chinese and Latin text on the same baseline. The
//! candidate list is ordered by preference: curated per-platform system
//! fonts first, then font files found in per-user font directories (a
//! user-installed pure-Latin font such as JetBrains Mono must not shadow
//! the curated CJK entries). Existence is not verified here; callers walk
//! the list and use the first entry they can actually load.

use std::path::{Path, PathBuf};

/// CJK font candidates for the compile target platform, best first.
///
/// - macOS: the system CJK whitelist (PingFang first) followed by scans of
///   `~/Library/Fonts` and `~/.fonts`.
/// - Linux: common Noto Sans CJK install paths followed by scans of
///   `~/.fonts` and `~/.local/share/fonts`.
/// - Windows: the standard Simplified Chinese fonts under the system font
///   directory (Microsoft YaHei, SimHei) followed by a scan of the
///   per-user font directory (`%LOCALAPPDATA%\Microsoft\Windows\Fonts`,
///   Windows 10 1809+).
pub fn cjk_font_candidates() -> Vec<PathBuf> {
    let mut candidates = known_cjk_fonts();
    candidates.extend(user_font_candidates());
    candidates
}

/// Curated system font whitelist for the platform (existence not checked).
#[cfg(target_os = "macos")]
fn known_cjk_fonts() -> Vec<PathBuf> {
    [
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "/System/Library/Fonts/Supplemental/Songti.ttc",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

/// Linux whitelist: covers the common Noto Sans CJK package layouts —
/// Debian/Ubuntu (`opentype/noto`, newer `noto-cjk`) and Fedora
/// (`google-noto-cjk`).
#[cfg(target_os = "linux")]
fn known_cjk_fonts() -> Vec<PathBuf> {
    [
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

/// Windows whitelist: msyh.ttc (Microsoft YaHei), simhei.ttf (SimHei) and
/// the YaHei bold face, resolved under `%SystemRoot%\Fonts` (falls back to
/// `C:\Windows\Fonts`).
#[cfg(target_os = "windows")]
fn known_cjk_fonts() -> Vec<PathBuf> {
    let fonts_dir = windows_fonts_dir();
    ["msyh.ttc", "simhei.ttf", "msyhbd.ttc"]
        .iter()
        .map(|name| fonts_dir.join(name))
        .collect()
}

/// Resolves `%SystemRoot%\Fonts`, defaulting to `C:\Windows\Fonts` when the
/// variable is unset or empty.
#[cfg(target_os = "windows")]
fn windows_fonts_dir() -> PathBuf {
    std::env::var_os("SystemRoot")
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("Fonts")
}

/// Per-user font candidates for the platform (missing directories are
/// skipped silently).
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn user_font_candidates() -> Vec<PathBuf> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    for dir in user_font_dirs(&home) {
        candidates.extend(scan_font_dir(&dir));
    }
    candidates
}

/// Per-user fonts on Windows: `%LOCALAPPDATA%\Microsoft\Windows\Fonts`
/// (fonts installed via Settings on Windows 10 1809+).
#[cfg(target_os = "windows")]
fn user_font_candidates() -> Vec<PathBuf> {
    match std::env::var_os("LOCALAPPDATA") {
        Some(local_appdata) => {
            let fonts_dir = PathBuf::from(local_appdata)
                .join("Microsoft")
                .join("Windows")
                .join("Fonts");
            scan_font_dir(&fonts_dir)
        }
        None => Vec::new(),
    }
}

/// Per-user font directories to scan, relative to the home directory.
#[cfg(target_os = "macos")]
fn user_font_dirs(home: &Path) -> Vec<PathBuf> {
    vec![home.join("Library").join("Fonts"), home.join(".fonts")]
}

/// Linux: the classic `~/.fonts` plus the XDG data location
/// `~/.local/share/fonts` (where modern desktop environments install
/// per-user fonts).
#[cfg(target_os = "linux")]
fn user_font_dirs(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".fonts"),
        home.join(".local").join("share").join("fonts"),
    ]
}

/// Home directory from `$HOME` (unix consumers only).
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// Collects the font files (`ttf`/`ttc`/`otf`, case-insensitive) directly
/// inside `dir`; missing or unreadable directories yield an empty list.
fn scan_font_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(is_font_file_name)
                .unwrap_or(false)
        })
        .collect()
}

/// Font-file check by file-name extension, case-insensitive (Windows fonts
/// can be installed as `.TTF` etc.).
fn is_font_file_name(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    lower.ends_with(".ttf") || lower.ends_with(".ttc") || lower.ends_with(".otf")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TC-PLATFORM-01: the candidate list is never empty and the curated
    /// whitelist comes first — user-scan results must never shadow it
    /// (ordering is the key contract for the caller).
    #[test]
    fn tc_platform_01_known_fonts_first_and_non_empty() {
        let all = cjk_font_candidates();
        let known = known_cjk_fonts();
        assert!(!known.is_empty());
        assert!(all.len() >= known.len());
        assert_eq!(&all[..known.len()], &known[..]);
    }

    /// TC-PLATFORM-02: curated whitelist path shapes per platform.
    #[test]
    fn tc_platform_02_known_font_path_shapes() {
        for path in known_cjk_fonts() {
            let display = path.display().to_string();
            #[cfg(target_os = "macos")]
            assert!(display.starts_with("/System/Library/Fonts/"), "{display}");
            #[cfg(target_os = "linux")]
            assert!(display.starts_with("/usr/share/fonts/"), "{display}");
            #[cfg(target_os = "windows")]
            assert!(
                display.ends_with(r"\Fonts\msyh.ttc")
                    || display.ends_with(r"\Fonts\simhei.ttf")
                    || display.ends_with(r"\Fonts\msyhbd.ttc"),
                "{display}"
            );
        }
    }

    /// TC-PLATFORM-03: the font-directory scan picks up ttf/ttc/otf files
    /// case-insensitively and skips everything else.
    #[test]
    fn tc_platform_03_scan_font_dir_filters_extensions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.ttf"), b"").unwrap();
        std::fs::write(dir.path().join("B.TTC"), b"").unwrap();
        std::fs::write(dir.path().join("c.otf"), b"").unwrap();
        std::fs::write(dir.path().join("d.txt"), b"").unwrap();
        std::fs::write(dir.path().join("noext"), b"").unwrap();
        let found = scan_font_dir(dir.path());
        assert_eq!(found.len(), 3, "{found:?}");
    }

    /// TC-PLATFORM-04: font file-name check (case-insensitive extensions).
    #[test]
    fn tc_platform_04_is_font_file_name() {
        assert!(is_font_file_name("PingFang.ttc"));
        assert!(is_font_file_name("MSYH.TTC"));
        assert!(is_font_file_name("noto.otf"));
        assert!(!is_font_file_name("readme.txt"));
        assert!(!is_font_file_name("no-extension"));
    }

    /// TC-PLATFORM-05: scanning a missing directory yields an empty list
    /// instead of panicking (common when a tool or font set is absent).
    #[test]
    fn tc_platform_05_scan_missing_dir_is_empty() {
        let missing = if cfg!(target_os = "windows") {
            Path::new(r"C:\definitely\not\a\font\dir")
        } else {
            Path::new("/definitely/not/a/font/dir")
        };
        assert!(scan_font_dir(missing).is_empty());
    }
}
