use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Component, Path};
use std::sync::Arc;

use crate::HostConfig;

#[derive(Clone, Copy, Debug)]
pub enum ContentType {
    Html,
    JavaScript,
    TextCss,
    Wasm,
}

impl ContentType {
    pub(crate) const fn header(self) -> &'static str {
        match self {
            Self::Html => "text/html; charset=utf-8",
            Self::JavaScript => "text/javascript; charset=utf-8",
            Self::TextCss => "text/css; charset=utf-8",
            Self::Wasm => "application/wasm",
        }
    }
}

/// An exact URL -> regular file mapping; filenames must be single safe components.
#[derive(Clone, Debug)]
pub struct Asset {
    pub url: String,
    pub filename: String,
    pub content_type: ContentType,
}

impl Asset {
    #[must_use]
    pub fn browser_defaults() -> Vec<Self> {
        [
            ("/", "index.html", ContentType::Html),
            ("/index.html", "index.html", ContentType::Html),
            ("/bootstrap.js", "bootstrap.js", ContentType::JavaScript),
            (
                "/canvas-viewer.js",
                "canvas-viewer.js",
                ContentType::JavaScript,
            ),
            ("/live-viewer.js", "live-viewer.js", ContentType::JavaScript),
            ("/live-page.js", "live-page.js", ContentType::JavaScript),
            ("/style.css", "style.css", ContentType::TextCss),
            (
                "/bmux_web_renderer.js",
                "bmux_web_renderer.js",
                ContentType::JavaScript,
            ),
            (
                "/presenter-adapter.js",
                "presenter-adapter.js",
                ContentType::JavaScript,
            ),
            (
                "/bmux_presenter.js",
                "bmux_presenter.js",
                ContentType::JavaScript,
            ),
            (
                "/bmux_presenter_bg.wasm",
                "bmux_presenter_bg.wasm",
                ContentType::Wasm,
            ),
            (
                "/bmux_web_renderer_bg.wasm",
                "bmux_web_renderer_bg.wasm",
                ContentType::Wasm,
            ),
        ]
        .into_iter()
        .map(|(url, filename, content_type)| Self {
            url: url.into(),
            filename: filename.into(),
            content_type,
        })
        .collect()
    }
}

pub(crate) struct LoadedAsset {
    pub bytes: Arc<[u8]>,
    pub content_type: ContentType,
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "invalid asset path or allowlist",
    )
}

fn safe_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn safe_url(url: &str) -> bool {
    url == "/" || url.strip_prefix('/').is_some_and(safe_filename)
}

fn open_at(parent: &File, name: &str, flags: libc::c_int) -> io::Result<File> {
    let name = CString::new(name).map_err(|_| invalid())?;
    // SAFETY: parent is a live descriptor and name is NUL terminated. O_NOFOLLOW
    // applies to every component separately; ownership of a successful fd is transferred.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat returned a new uniquely owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_directory(path: &Path) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(invalid());
    }
    let mut directory = File::open("/")?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory = open_at(
                    &directory,
                    name.to_str().ok_or_else(invalid)?,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )?;
            }
            _ => return Err(invalid()),
        }
    }
    Ok(directory)
}

pub(crate) fn load(config: &HostConfig) -> io::Result<BTreeMap<String, LoadedAsset>> {
    if config.assets.is_empty() || config.assets.len() > 16 {
        return Err(invalid());
    }
    if let crate::BrowserDelivery::Document { adapter } = &config.browser_delivery
        && (!safe_url(adapter)
            || !adapter.ends_with(".js")
            || !config.assets.iter().any(|asset| {
                asset.url == *adapter && matches!(asset.content_type, ContentType::JavaScript)
            })
            || !config.assets.iter().any(|asset| {
                asset.url == "/bootstrap.js"
                    && matches!(asset.content_type, ContentType::JavaScript)
            }))
    {
        return Err(invalid());
    }
    let directory = open_directory(&config.asset_directory)?;
    let mut assets = BTreeMap::new();
    let mut total = 0_usize;
    for asset in &config.assets {
        if !safe_url(&asset.url)
            || !safe_filename(&asset.filename)
            || asset.url == "/snapshot"
            || assets.contains_key(&asset.url)
        {
            return Err(invalid());
        }
        let file = open_at(
            &directory,
            &asset.filename,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.len() == 0
            || metadata.len() > config.limits.max_asset_bytes as u64
        {
            return Err(invalid());
        }
        let mut bytes = Vec::new();
        file.take(config.limits.max_asset_bytes as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.is_empty() || bytes.len() > config.limits.max_asset_bytes {
            return Err(invalid());
        }
        if matches!(asset.content_type, ContentType::Wasm) && !bytes.starts_with(b"\0asm\x01\0\0\0")
        {
            return Err(invalid());
        }
        if matches!(asset.content_type, ContentType::JavaScript)
            && asset.url == "/bootstrap.js"
            && let crate::BrowserDelivery::Document { adapter } = &config.browser_delivery
        {
            let defaults = serde_json::json!({"delivery": "document", "adapter": adapter});
            let mut configured = format!("const bmuxViewerDefaults = {defaults};\n").into_bytes();
            configured.extend_from_slice(&bytes);
            bytes = configured;
        }
        if bytes.len() > config.limits.max_asset_bytes {
            return Err(invalid());
        }
        total = total.checked_add(bytes.len()).ok_or_else(invalid)?;
        if total > config.limits.max_total_asset_bytes {
            return Err(invalid());
        }
        assets.insert(
            asset.url.clone(),
            LoadedAsset {
                bytes: bytes.into(),
                content_type: asset.content_type,
            },
        );
    }
    Ok(assets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_include_only_explicit_bounded_stylesheet() {
        let defaults = Asset::browser_defaults();
        let actual: Vec<_> = defaults
            .iter()
            .map(|asset| {
                (
                    asset.url.as_str(),
                    asset.filename.as_str(),
                    asset.content_type.header(),
                )
            })
            .collect();
        assert_eq!(
            actual,
            vec![
                ("/", "index.html", "text/html; charset=utf-8"),
                ("/index.html", "index.html", "text/html; charset=utf-8"),
                (
                    "/bootstrap.js",
                    "bootstrap.js",
                    "text/javascript; charset=utf-8"
                ),
                (
                    "/canvas-viewer.js",
                    "canvas-viewer.js",
                    "text/javascript; charset=utf-8"
                ),
                (
                    "/live-viewer.js",
                    "live-viewer.js",
                    "text/javascript; charset=utf-8"
                ),
                (
                    "/live-page.js",
                    "live-page.js",
                    "text/javascript; charset=utf-8"
                ),
                ("/style.css", "style.css", "text/css; charset=utf-8"),
                (
                    "/bmux_web_renderer.js",
                    "bmux_web_renderer.js",
                    "text/javascript; charset=utf-8"
                ),
                (
                    "/presenter-adapter.js",
                    "presenter-adapter.js",
                    "text/javascript; charset=utf-8"
                ),
                (
                    "/bmux_presenter.js",
                    "bmux_presenter.js",
                    "text/javascript; charset=utf-8"
                ),
                (
                    "/bmux_presenter_bg.wasm",
                    "bmux_presenter_bg.wasm",
                    "application/wasm"
                ),
                (
                    "/bmux_web_renderer_bg.wasm",
                    "bmux_web_renderer_bg.wasm",
                    "application/wasm"
                ),
            ]
        );
        let css = defaults
            .iter()
            .find(|asset| asset.url == "/style.css")
            .unwrap();
        assert_eq!(css.filename, "style.css");
        assert_eq!(css.content_type.header(), "text/css; charset=utf-8");
        let directory = tempfile::tempdir().unwrap();
        for asset in &defaults {
            let bytes: &[u8] = if matches!(asset.content_type, ContentType::Wasm) {
                b"\0asm\x01\0\0\0"
            } else {
                b"x"
            };
            std::fs::write(directory.path().join(&asset.filename), bytes).unwrap();
        }
        std::fs::write(directory.path().join("unlisted.css"), b"hidden").unwrap();
        let mut config = HostConfig::new(directory.path().canonicalize().unwrap());
        let loaded = load(&config).unwrap();
        assert_eq!(loaded.len(), defaults.len());
        for asset in &defaults {
            assert_eq!(
                loaded[&asset.url].content_type.header(),
                asset.content_type.header()
            );
        }
        assert!(!loaded.contains_key("/unlisted.css"));
        assert_eq!(loaded["/style.css"].bytes.as_ref(), b"x");
        // Keep valid WASM within the per-file budget so only the stylesheet exceeds it.
        config.limits.max_asset_bytes = 8;
        std::fs::write(directory.path().join("style.css"), b"123456789").unwrap();
        assert!(load(&config).is_err());
        std::fs::write(directory.path().join("style.css"), b"x").unwrap();
        // Ten one-byte mappings (including the index alias), two eight-byte WASMs.
        config.limits.max_total_asset_bytes = 26;
        assert!(load(&config).is_ok());
        config.limits.max_total_asset_bytes = 25;
        assert!(load(&config).is_err());
    }

    #[test]
    fn document_defaults_are_validated_and_embedded_once() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = HostConfig::new(directory.path().to_path_buf());
        for asset in &config.assets {
            let bytes: &[u8] = if matches!(asset.content_type, ContentType::Wasm) {
                b"\0asm\x01\0\0\0"
            } else {
                b"// asset\n"
            };
            std::fs::write(directory.path().join(&asset.filename), bytes).unwrap();
        }
        let original = load(&config).unwrap();
        assert_eq!(&*original["/bootstrap.js"].bytes, b"// asset\n");
        config.browser_delivery = crate::BrowserDelivery::Document {
            adapter: "/presenter-adapter.js".into(),
        };
        let configured = load(&config).unwrap();
        let bootstrap = std::str::from_utf8(&configured["/bootstrap.js"].bytes).unwrap();
        assert!(bootstrap.starts_with("const bmuxViewerDefaults = "));
        assert!(bootstrap.contains("\"delivery\":\"document\""));
        assert!(bootstrap.contains("\"adapter\":\"/presenter-adapter.js\""));
        assert!(bootstrap.ends_with("// asset\n"));
        assert_eq!(configured["/"].bytes, original["/"].bytes);
        for adapter in [
            "https://other/adapter.js",
            "/missing.js",
            "/style.css",
            "/../x.js",
            "/presenter-adapter.js?x=1",
        ] {
            config.browser_delivery = crate::BrowserDelivery::Document {
                adapter: adapter.into(),
            };
            assert!(load(&config).is_err(), "{adapter}");
        }
        config.browser_delivery = crate::BrowserDelivery::Document {
            adapter: "/presenter-adapter.js".into(),
        };
        config.limits.max_asset_bytes = 32;
        assert!(load(&config).is_err());
        config.limits.max_asset_bytes = 1024;
        config.limits.max_total_asset_bytes = 128;
        assert!(load(&config).is_err());
    }

    #[test]
    fn only_exact_flat_assets_are_allowed() {
        for name in [
            "",
            ".",
            "..",
            "../secret",
            "a/b",
            "a\\b",
            "%2e",
            "a?b",
            "a\0b",
        ] {
            assert!(!safe_filename(name), "{name:?}");
        }
        for url in ["//x", "/../x", "/x?token=secret", "/%2e%2e", "https://x/x"] {
            assert!(!safe_url(url), "{url:?}");
        }
        assert!(safe_filename("bmux_web_renderer_bg.wasm"));
        assert!(safe_url("/"));
        assert!(safe_url("/bootstrap.js"));
    }

    #[test]
    fn directory_must_be_absolute_without_parent_components() {
        assert!(open_directory(Path::new("relative")).is_err());
        assert!(open_directory(Path::new("/../")).is_err());
    }
}
