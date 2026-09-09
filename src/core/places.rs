//! System places: known navigation destinations backed by the user's real
//! home directory and XDG user-dirs configuration.
//!
//! A place's identity is its [`Place::path`]; [`SystemPlaceKind`] only
//! drives canonical ordering and a presentation label. It is never used to
//! decide identity, equality, or deduplication — two places with the same
//! path are the same place, regardless of kind.
//!
//! Paths are never canonicalized or symlink-resolved here: whatever the
//! home dir lookup or XDG configuration reports is preserved as-is,
//! consistent with the logical-navigation philosophy in
//! [`crate::core::navigation`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The kind of a system place.
///
/// Presentation and ordering only — never identity. Do not compare places
/// by kind, or derive a path from it; always use [`Place::path`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SystemPlaceKind {
    Home,
    Desktop,
    Documents,
    Downloads,
    Pictures,
    Music,
    Videos,
}

impl SystemPlaceKind {
    /// Canonical, deterministic order in which places are discovered and
    /// should be presented.
    const ORDER: [SystemPlaceKind; 7] = [
        SystemPlaceKind::Home,
        SystemPlaceKind::Desktop,
        SystemPlaceKind::Documents,
        SystemPlaceKind::Downloads,
        SystemPlaceKind::Pictures,
        SystemPlaceKind::Music,
        SystemPlaceKind::Videos,
    ];

    /// Presentation-only label. Never used for identity or comparisons.
    pub fn label(self) -> &'static str {
        match self {
            SystemPlaceKind::Home => "Home",
            SystemPlaceKind::Desktop => "Desktop",
            SystemPlaceKind::Documents => "Documents",
            SystemPlaceKind::Downloads => "Downloads",
            SystemPlaceKind::Pictures => "Pictures",
            SystemPlaceKind::Music => "Music",
            SystemPlaceKind::Videos => "Videos",
        }
    }
}

/// A known navigation destination.
///
/// Identity is [`Place::path`] — a [`PathBuf`], used for navigation and
/// comparisons. [`Place::kind`] is presentation/ordering metadata only.
#[derive(Debug, Clone)]
pub struct Place {
    kind: SystemPlaceKind,
    path: PathBuf,
}

impl Place {
    pub fn kind(&self) -> SystemPlaceKind {
        self.kind
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Builds a `Place` directly from an already-decided kind/path pair,
    /// bypassing discovery entirely. Test-only seam for crates upstream of
    /// `places` (e.g. `app::state`) that need a deterministic `Vec<Place>`
    /// without touching the real `$HOME`/XDG configuration — production
    /// code only ever gets a `Place` through [`system_places`].
    #[cfg(test)]
    pub(crate) fn new_for_test(kind: SystemPlaceKind, path: PathBuf) -> Self {
        Place { kind, path }
    }
}

/// Identity is the path alone — `kind` is presentation/ordering metadata
/// and must never participate in equality (see the module-level docs).
impl PartialEq for Place {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

impl Eq for Place {}

/// Raw, not-yet-validated candidate paths for each system place.
///
/// `None` means "not configured" (or, for `home`, that the home directory
/// itself could not be resolved). This is the seam that makes place
/// discovery testable: production code builds it from
/// `directories::UserDirs`, which only ever reflects the real environment;
/// tests build it directly with controlled, isolated paths, so they never
/// depend on the developer's actual `$HOME` or XDG configuration.
#[derive(Debug, Clone, Default)]
struct SystemPathCandidates {
    home: Option<PathBuf>,
    desktop: Option<PathBuf>,
    documents: Option<PathBuf>,
    downloads: Option<PathBuf>,
    pictures: Option<PathBuf>,
    music: Option<PathBuf>,
    videos: Option<PathBuf>,
}

impl SystemPathCandidates {
    fn get(&self, kind: SystemPlaceKind) -> Option<&Path> {
        match kind {
            SystemPlaceKind::Home => self.home.as_deref(),
            SystemPlaceKind::Desktop => self.desktop.as_deref(),
            SystemPlaceKind::Documents => self.documents.as_deref(),
            SystemPlaceKind::Downloads => self.downloads.as_deref(),
            SystemPlaceKind::Pictures => self.pictures.as_deref(),
            SystemPlaceKind::Music => self.music.as_deref(),
            SystemPlaceKind::Videos => self.videos.as_deref(),
        }
    }
}

/// Discovers the system places available on this machine, from the real
/// home directory and XDG user-dirs configuration (`directories::UserDirs`).
///
/// Deterministic order (`Home`, `Desktop`, `Documents`, `Downloads`,
/// `Pictures`, `Music`, `Videos`); a place missing from that order is
/// simply omitted, never an error. Two places resolving to the same path
/// are deduplicated by path identity — the earlier one in canonical order
/// wins — never by label or kind.
pub fn system_places() -> Vec<Place> {
    build_places(&candidates_from_user_dirs(
        directories::UserDirs::new().as_ref(),
    ))
}

fn candidates_from_user_dirs(user_dirs: Option<&directories::UserDirs>) -> SystemPathCandidates {
    match user_dirs {
        None => SystemPathCandidates::default(),
        Some(dirs) => SystemPathCandidates {
            home: Some(dirs.home_dir().to_path_buf()),
            desktop: dirs.desktop_dir().map(Path::to_path_buf),
            documents: dirs.document_dir().map(Path::to_path_buf),
            downloads: dirs.download_dir().map(Path::to_path_buf),
            pictures: dirs.picture_dir().map(Path::to_path_buf),
            music: dirs.audio_dir().map(Path::to_path_buf),
            videos: dirs.video_dir().map(Path::to_path_buf),
        },
    }
}

/// Builds the deterministic, deduplicated place list from raw candidates.
///
/// Every candidate must be absolute to be included — a relative path is
/// simply omitted, never resolved against the current directory or
/// otherwise "fixed up". This applies to `Home` too. `Home` is otherwise
/// included whenever it resolves to an absolute path at all — no existence
/// check here; a stale/missing home directory is Navigation's problem the
/// moment something tries to actually visit it, not this list's. Every
/// other place is included only when configured, absolute, and its path
/// exists and is a directory right now (not canonicalized — a broken
/// symlink or a path that used to be valid is simply skipped, not resolved
/// further).
fn build_places(candidates: &SystemPathCandidates) -> Vec<Place> {
    let mut seen = HashSet::new();
    let mut places = Vec::new();

    for kind in SystemPlaceKind::ORDER {
        let Some(path) = candidates.get(kind) else {
            continue;
        };

        if !path.is_absolute() {
            continue;
        }

        if kind != SystemPlaceKind::Home && !is_usable_directory(path) {
            continue;
        }

        if seen.insert(path.to_path_buf()) {
            places.push(Place {
                kind,
                path: path.to_path_buf(),
            });
        }
    }

    places
}

fn is_usable_directory(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::fs;

    /// Creates a real subdirectory under `root` — an isolated temp dir,
    /// never the developer's real `$HOME` or XDG configuration.
    fn dir(root: &TempDir, name: &str) -> PathBuf {
        let path = root.path().join(name);
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn home_appears_first() {
        let root = TempDir::new();
        let candidates = SystemPathCandidates {
            home: Some(dir(&root, "home")),
            desktop: Some(dir(&root, "desktop")),
            ..Default::default()
        };

        let places = build_places(&candidates);

        assert_eq!(places[0].kind(), SystemPlaceKind::Home);
    }

    #[test]
    fn system_places_follow_deterministic_order() {
        let root = TempDir::new();
        let candidates = SystemPathCandidates {
            home: Some(dir(&root, "home")),
            desktop: Some(dir(&root, "desktop")),
            documents: Some(dir(&root, "documents")),
            downloads: Some(dir(&root, "downloads")),
            pictures: Some(dir(&root, "pictures")),
            music: Some(dir(&root, "music")),
            videos: Some(dir(&root, "videos")),
        };

        let places = build_places(&candidates);
        let kinds: Vec<_> = places.iter().map(Place::kind).collect();

        assert_eq!(
            kinds,
            vec![
                SystemPlaceKind::Home,
                SystemPlaceKind::Desktop,
                SystemPlaceKind::Documents,
                SystemPlaceKind::Downloads,
                SystemPlaceKind::Pictures,
                SystemPlaceKind::Music,
                SystemPlaceKind::Videos,
            ]
        );
    }

    #[test]
    fn downloads_uses_the_configured_xdg_path_not_a_hardcoded_english_name() {
        let root = TempDir::new();
        // Portuguese XDG_DOWNLOAD_DIR, as a real user might configure it —
        // proves the path is used as-is, never assumed to be "Downloads".
        let downloads = dir(&root, "Transferências");
        let candidates = SystemPathCandidates {
            home: Some(dir(&root, "home")),
            downloads: Some(downloads.clone()),
            ..Default::default()
        };

        let places = build_places(&candidates);

        let downloads_place = places
            .iter()
            .find(|place| place.kind() == SystemPlaceKind::Downloads)
            .expect("Downloads should be present");
        assert_eq!(downloads_place.path(), downloads);
    }

    #[test]
    fn documents_uses_the_configured_path() {
        let root = TempDir::new();
        let documents = dir(&root, "Documentos");
        let candidates = SystemPathCandidates {
            home: Some(dir(&root, "home")),
            documents: Some(documents.clone()),
            ..Default::default()
        };

        let places = build_places(&candidates);

        let documents_place = places
            .iter()
            .find(|place| place.kind() == SystemPlaceKind::Documents)
            .expect("Documents should be present");
        assert_eq!(documents_place.path(), documents);
    }

    #[test]
    fn unconfigured_place_is_omitted() {
        let root = TempDir::new();
        let candidates = SystemPathCandidates {
            home: Some(dir(&root, "home")),
            music: None,
            ..Default::default()
        };

        let places = build_places(&candidates);

        assert!(
            !places
                .iter()
                .any(|place| place.kind() == SystemPlaceKind::Music)
        );
    }

    #[test]
    fn configured_but_nonexistent_path_is_omitted() {
        let root = TempDir::new();
        let candidates = SystemPathCandidates {
            home: Some(dir(&root, "home")),
            downloads: Some(root.path().join("does-not-exist")),
            ..Default::default()
        };

        let places = build_places(&candidates);

        assert!(
            !places
                .iter()
                .any(|place| place.kind() == SystemPlaceKind::Downloads)
        );
    }

    #[test]
    fn configured_path_that_is_a_file_not_a_directory_is_omitted() {
        let root = TempDir::new();
        let file_path = root.path().join("pictures-is-a-file");
        fs::write(&file_path, b"").unwrap();
        let candidates = SystemPathCandidates {
            home: Some(dir(&root, "home")),
            pictures: Some(file_path),
            ..Default::default()
        };

        let places = build_places(&candidates);

        assert!(
            !places
                .iter()
                .any(|place| place.kind() == SystemPlaceKind::Pictures)
        );
    }

    #[test]
    fn two_places_with_the_same_path_appear_only_once() {
        let root = TempDir::new();
        let shared = dir(&root, "shared");
        let candidates = SystemPathCandidates {
            home: Some(dir(&root, "home")),
            documents: Some(shared.clone()),
            downloads: Some(shared.clone()),
            ..Default::default()
        };

        let places = build_places(&candidates);

        let matches: Vec<_> = places
            .iter()
            .filter(|place| place.path() == shared)
            .collect();
        assert_eq!(matches.len(), 1);
        // Documents precedes Downloads in canonical order, so it wins.
        assert_eq!(matches[0].kind(), SystemPlaceKind::Documents);
    }

    #[test]
    fn home_duplicated_by_an_xdg_place_still_appears_once() {
        let root = TempDir::new();
        let home = dir(&root, "home");
        let candidates = SystemPathCandidates {
            home: Some(home.clone()),
            desktop: Some(home.clone()),
            ..Default::default()
        };

        let places = build_places(&candidates);

        let matches: Vec<_> = places.iter().filter(|place| place.path() == home).collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].kind(), SystemPlaceKind::Home);
    }

    #[test]
    fn partial_configuration_does_not_prevent_other_valid_places() {
        let root = TempDir::new();
        let candidates = SystemPathCandidates {
            home: Some(dir(&root, "home")),
            desktop: None,
            documents: Some(dir(&root, "documents")),
            downloads: Some(root.path().join("does-not-exist")),
            pictures: Some(dir(&root, "pictures")),
            music: None,
            videos: None,
        };

        let places = build_places(&candidates);
        let kinds: Vec<_> = places.iter().map(Place::kind).collect();

        assert_eq!(
            kinds,
            vec![
                SystemPlaceKind::Home,
                SystemPlaceKind::Documents,
                SystemPlaceKind::Pictures,
            ]
        );
    }

    #[test]
    fn paths_with_spaces_are_preserved() {
        let root = TempDir::new();
        let pictures = dir(&root, "My Pictures");
        let candidates = SystemPathCandidates {
            home: Some(dir(&root, "home")),
            pictures: Some(pictures.clone()),
            ..Default::default()
        };

        let places = build_places(&candidates);

        let pictures_place = places
            .iter()
            .find(|place| place.kind() == SystemPlaceKind::Pictures)
            .expect("Pictures should be present");
        assert_eq!(pictures_place.path(), pictures);
    }

    #[test]
    fn unicode_paths_are_preserved() {
        let root = TempDir::new();
        let music = dir(&root, "Música 音楽 🎵");
        let candidates = SystemPathCandidates {
            home: Some(dir(&root, "home")),
            music: Some(music.clone()),
            ..Default::default()
        };

        let places = build_places(&candidates);

        let music_place = places
            .iter()
            .find(|place| place.kind() == SystemPlaceKind::Music)
            .expect("Music should be present");
        assert_eq!(music_place.path(), music);
    }

    #[cfg(unix)]
    #[test]
    fn no_path_is_canonicalized() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new();
        let real = dir(&root, "real-videos");
        let link = root.path().join("videos-link");
        symlink(&real, &link).unwrap();

        let candidates = SystemPathCandidates {
            home: Some(dir(&root, "home")),
            videos: Some(link.clone()),
            ..Default::default()
        };

        let places = build_places(&candidates);

        let videos_place = places
            .iter()
            .find(|place| place.kind() == SystemPlaceKind::Videos)
            .expect("Videos should be present");
        // The symlink path itself is preserved, never resolved to `real`.
        assert_eq!(videos_place.path(), link);
        assert_ne!(videos_place.path(), real);
    }

    #[test]
    fn same_path_different_kind_is_equal() {
        // Place::eq must compare path only. Two Places with the same path
        // but different kinds are the same place, per the documented
        // contract ("Place identity is its path").
        let path = PathBuf::from("/same/path");
        let place_a = Place {
            kind: SystemPlaceKind::Desktop,
            path: path.clone(),
        };
        let place_b = Place {
            kind: SystemPlaceKind::Documents,
            path,
        };

        assert_eq!(place_a, place_b);
    }

    #[test]
    fn different_path_same_kind_is_not_equal() {
        let place_a = Place {
            kind: SystemPlaceKind::Desktop,
            path: PathBuf::from("/path/one"),
        };
        let place_b = Place {
            kind: SystemPlaceKind::Desktop,
            path: PathBuf::from("/path/two"),
        };

        assert_ne!(place_a, place_b);
    }

    #[test]
    fn labels_and_kinds_are_not_used_as_identity() {
        // Same underlying kind values, different labels, but identity must
        // come from `path()` alone: two different kinds sharing a path
        // still collapse to a single place (see also the dedup tests
        // above), and equality between two `Place`s with different kinds
        // but the same path is not assumed anywhere in this module.
        assert_ne!(
            SystemPlaceKind::Desktop.label(),
            SystemPlaceKind::Documents.label()
        );

        let root = TempDir::new();
        let shared = dir(&root, "shared-by-kind");
        let candidates = SystemPathCandidates {
            home: Some(dir(&root, "home")),
            pictures: Some(shared.clone()),
            music: Some(shared.clone()),
            ..Default::default()
        };

        let places = build_places(&candidates);
        let matches: Vec<_> = places
            .iter()
            .filter(|place| place.path() == shared)
            .collect();
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn relative_home_is_omitted() {
        let candidates = SystemPathCandidates {
            home: Some(PathBuf::from("relative-home")),
            ..Default::default()
        };

        let places = build_places(&candidates);

        assert!(
            !places
                .iter()
                .any(|place| place.kind() == SystemPlaceKind::Home)
        );
    }

    #[test]
    fn relative_xdg_place_is_omitted() {
        let root = TempDir::new();
        let candidates = SystemPathCandidates {
            home: Some(dir(&root, "home")),
            downloads: Some(PathBuf::from("Downloads")),
            ..Default::default()
        };

        let places = build_places(&candidates);

        assert!(
            !places
                .iter()
                .any(|place| place.kind() == SystemPlaceKind::Downloads)
        );
    }

    #[test]
    fn all_built_places_are_absolute() {
        let root = TempDir::new();
        let candidates = SystemPathCandidates {
            home: Some(dir(&root, "home")),
            desktop: Some(dir(&root, "desktop")),
            documents: Some(dir(&root, "documents")),
            downloads: Some(dir(&root, "downloads")),
            pictures: Some(dir(&root, "pictures")),
            music: Some(dir(&root, "music")),
            videos: Some(dir(&root, "videos")),
        };

        let places = build_places(&candidates);

        assert!(places.iter().all(|p| p.path().is_absolute()));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_path_survives_discovery() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let root = TempDir::new();
        // 0x66, 0x80 ("f" followed by a stray continuation byte): invalid
        // UTF-8, but a perfectly valid Unix path component.
        let raw_name = OsStr::from_bytes(&[0x66, 0x80, 0x6f]).to_os_string();
        assert!(raw_name.to_str().is_none(), "name should be invalid UTF-8");
        let desktop = root.path().join(&raw_name);
        fs::create_dir(&desktop).unwrap();

        let candidates = SystemPathCandidates {
            home: Some(dir(&root, "home")),
            desktop: Some(desktop.clone()),
            ..Default::default()
        };

        let places = build_places(&candidates);

        let desktop_place = places
            .iter()
            .find(|place| place.kind() == SystemPlaceKind::Desktop)
            .expect("Desktop should be present");
        assert_eq!(desktop_place.path(), desktop);
    }
}
