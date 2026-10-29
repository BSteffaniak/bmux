use std::path::Path;

use anyhow::{Context, Result, ensure};

pub(in super::super) async fn connect(path: &Path) -> Result<tokio::net::UnixStream> {
    ensure!(path.is_absolute(), "observer socket path must be absolute");
    tokio::net::UnixStream::connect(path)
        .await
        .context("connect private observer socket")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn relative_endpoint_is_rejected() {
        assert!(connect(Path::new("relative.sock")).await.is_err());
    }
}
