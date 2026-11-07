// Fixture paths are Unix-absolute; base variables must be absolute on the host.
#![cfg(unix)]

use bmux_config_composition::{AppDirs, ErrorKind, Platform, expand_home, home_dir};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn env(vars: &[(&str, &str)]) -> BTreeMap<String, String> {
    vars.iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn dirs(config: &str, data: &str, cache: &str, runtime: &str) -> AppDirs {
    AppDirs {
        config: config.into(),
        data: data.into(),
        cache: cache.into(),
        runtime: runtime.into(),
    }
}

#[test]
fn unix_uses_xdg_bases_with_home_fallbacks() {
    let home_only = env(&[("HOME", "/home/u")]);
    assert_eq!(
        AppDirs::resolve("app", &home_only, Platform::Unix).unwrap(),
        dirs(
            "/home/u/.config/app",
            "/home/u/.local/share/app",
            "/home/u/.cache/app",
            "/home/u/.local/share/app/run",
        )
    );
    let xdg = env(&[
        ("HOME", "/home/u"),
        ("XDG_CONFIG_HOME", "/x/config"),
        ("XDG_DATA_HOME", "/x/data"),
        ("XDG_CACHE_HOME", "/x/cache"),
        ("XDG_RUNTIME_DIR", "/run/user/1"),
    ]);
    assert_eq!(
        AppDirs::resolve("app", &xdg, Platform::Unix).unwrap(),
        dirs(
            "/x/config/app",
            "/x/data/app",
            "/x/cache/app",
            "/run/user/1/app"
        )
    );
}

#[test]
fn macos_uses_dot_config_and_library() {
    let environment = env(&[("HOME", "/Users/u"), ("XDG_RUNTIME_DIR", "/ignored")]);
    let resolved = AppDirs::resolve("app", &environment, Platform::MacOs).unwrap();
    assert_eq!(
        resolved,
        dirs(
            "/Users/u/.config/app",
            "/Users/u/Library/Application Support/app",
            "/Users/u/Library/Caches/app",
            "/Users/u/Library/Application Support/app/run",
        )
    );
    assert_eq!(
        resolved.config_file(),
        PathBuf::from("/Users/u/.config/app/config.toml")
    );
}

#[test]
fn windows_uses_appdata_and_nests_local_directories() {
    let environment = env(&[("APPDATA", "/roaming"), ("LOCALAPPDATA", "/local")]);
    assert_eq!(
        AppDirs::resolve("app", &environment, Platform::Windows).unwrap(),
        dirs(
            "/roaming/app",
            "/local/app/data",
            "/local/app/cache",
            "/local/app/run",
        )
    );
    let profile_only = env(&[("USERPROFILE", "/profile")]);
    assert_eq!(
        AppDirs::resolve("app", &profile_only, Platform::Windows)
            .unwrap()
            .config,
        PathBuf::from("/profile/AppData/Roaming/app")
    );
}

#[test]
fn empty_or_relative_bases_are_ignored() {
    let environment = env(&[
        ("HOME", "/home/u"),
        ("XDG_CONFIG_HOME", ""),
        ("XDG_DATA_HOME", "relative"),
    ]);
    let resolved = AppDirs::resolve("app", &environment, Platform::Unix).unwrap();
    assert_eq!(resolved.config, PathBuf::from("/home/u/.config/app"));
    assert_eq!(resolved.data, PathBuf::from("/home/u/.local/share/app"));
}

#[test]
fn missing_home_and_invalid_names_are_structured_errors() {
    let error = AppDirs::resolve("app", &BTreeMap::new(), Platform::Unix).unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidContext);
    let home = env(&[("HOME", "/home/u")]);
    for name in ["", "a/b", "..", "/abs"] {
        let error = AppDirs::resolve(name, &home, Platform::Unix).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidPolicy, "{name:?}");
    }
}

#[test]
fn home_expansion() {
    let environment = env(&[("HOME", "/home/u"), ("USERPROFILE", "/profile")]);
    assert_eq!(
        home_dir(&environment, Platform::Windows),
        Some(PathBuf::from("/profile"))
    );
    let expand = |path| expand_home(path, &environment, Platform::Unix).unwrap();
    assert_eq!(expand("~"), PathBuf::from("/home/u"));
    assert_eq!(expand("~/data"), PathBuf::from("/home/u/data"));
    assert_eq!(expand("~other/data"), PathBuf::from("~other/data"));
    assert_eq!(expand("/abs"), PathBuf::from("/abs"));
    assert_eq!(
        expand_home("~\\data", &environment, Platform::Windows).unwrap(),
        PathBuf::from("/profile").join("data")
    );
    assert_eq!(
        expand_home("~", &BTreeMap::new(), Platform::Unix)
            .unwrap_err()
            .kind,
        ErrorKind::InvalidContext
    );
}
