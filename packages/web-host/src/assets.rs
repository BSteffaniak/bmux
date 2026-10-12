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
    Wasm,
}

impl ContentType {
    pub(crate) const fn header(self) -> &'static str {
        match self {
            Self::Html => "text/html; charset=utf-8",
            Self::JavaScript => "text/javascript; charset=utf-8",
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
                "/bmux_web_renderer.js",
                "bmux_web_renderer.js",
                ContentType::JavaScript,
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
