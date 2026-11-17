//! Loading the bytes of a Kitty transmission a pane's program sent.
//!
//! A transmission's bytes are in its payload (`t=d`), or in a file (`t=f`), temporary file
//! (`t=t`) or shared memory object (`t=s`) the payload names, and may be zlib-compressed
//! (`o=z`). Named media are read the way Kitty reads them: only regular files, never under
//! `/proc`, `/sys` or `/dev`; a temporary file only from a temporary directory and only when its
//! name says it is one (`tty-graphics-protocol`), deleted once read; a shared memory object
//! unlinked once read. Everything is bounded by the caller's byte limit.
//!
//! Errors are the reply text Kitty sends for them (`ECODE: message`).

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::model::{KittyMedium, KittyTransmitDetails};

/// The bytes `data` and `details` describe, decompressed, at most `limit` of them.
///
/// # Errors
/// Returns the Kitty error reply for a medium that cannot be read, is not allowed, holds more
/// than `limit` bytes, or does not decompress.
pub(crate) fn load(
    data: Vec<u8>,
    details: &KittyTransmitDetails,
    limit: usize,
) -> Result<Vec<u8>, &'static str> {
    let bytes = match details.medium {
        KittyMedium::Direct => data,
        KittyMedium::File => read_file(&named_path(&data)?, details, limit, false)?,
        KittyMedium::TempFile => read_file(&named_path(&data)?, details, limit, true)?,
        KittyMedium::SharedMemory => read_shared_memory(&data, details, limit)?,
    };
    if details.compressed {
        return inflate(&bytes, limit);
    }
    if bytes.len() > limit {
        return Err("EFBIG: image data too large");
    }
    Ok(bytes)
}

/// The absolute path a payload names.
fn named_path(data: &[u8]) -> Result<PathBuf, &'static str> {
    let path = std::str::from_utf8(data).map_err(|_| "EINVAL: path is not UTF-8")?;
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err("EINVAL: path is not absolute");
    }
    Ok(path.to_path_buf())
}

/// Whether `path` (resolved) is under a directory Kitty never reads from.
fn forbidden(path: &Path) -> bool {
    ["/proc", "/sys", "/dev"]
        .iter()
        .any(|root| path.starts_with(root))
}

/// Whether `path` (resolved) is in a temporary directory.
fn in_temporary_directory(path: &Path) -> bool {
    let mut roots = vec![std::env::temp_dir(), PathBuf::from("/tmp")];
    if cfg!(target_os = "linux") {
        roots.push(PathBuf::from("/dev/shm"));
    }
    roots
        .into_iter()
        .filter_map(|root| root.canonicalize().ok())
        .any(|root| path.starts_with(root))
}

fn read_file(
    path: &Path,
    details: &KittyTransmitDetails,
    limit: usize,
    temporary: bool,
) -> Result<Vec<u8>, &'static str> {
    let resolved = path.canonicalize().map_err(|_| "ENOENT: no such file")?;
    if temporary {
        let named_temporary = resolved
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains("tty-graphics-protocol"));
        if !named_temporary || !in_temporary_directory(&resolved) {
            return Err("EPERM: not a temporary file");
        }
    } else if forbidden(&resolved) {
        return Err("EPERM: file not allowed");
    }
    let bytes = read_regular(&resolved, details, limit);
    if temporary && bytes.is_ok() {
        // A temporary file is the program's handoff: it is gone once read.
        let _ = std::fs::remove_file(&resolved);
    }
    bytes
}

/// Read a regular file at `path` from `details.offset`, `details.size` bytes (or to its end),
/// at most `limit` bytes.
fn read_regular(
    path: &Path,
    details: &KittyTransmitDetails,
    limit: usize,
) -> Result<Vec<u8>, &'static str> {
    let mut file = std::fs::File::open(path).map_err(|_| "ENOENT: cannot open file")?;
    let metadata = file.metadata().map_err(|_| "EBADF: cannot inspect file")?;
    if !metadata.is_file() {
        return Err("EINVAL: not a regular file");
    }
    if details.offset > 0 {
        file.seek(SeekFrom::Start(details.offset))
            .map_err(|_| "EINVAL: offset beyond the file")?;
    }
    let wanted = details.size.unwrap_or(u64::MAX);
    if wanted > u64::try_from(limit).unwrap_or(u64::MAX) && details.size.is_some() {
        return Err("EFBIG: image data too large");
    }
    let mut bytes = Vec::new();
    file.take(wanted.min(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1)))
        .read_to_end(&mut bytes)
        .map_err(|_| "EBADF: cannot read file")?;
    if bytes.len() > limit {
        return Err("EFBIG: image data too large");
    }
    if details
        .size
        .is_some_and(|size| u64::try_from(bytes.len()).ok() != Some(size))
    {
        return Err("EINVAL: file shorter than its size");
    }
    Ok(bytes)
}

/// Read and unlink the POSIX shared memory object a payload names. On Linux shared memory
/// objects are files under `/dev/shm`; elsewhere this build does not read them, and a program
/// falls back to another medium.
fn read_shared_memory(
    data: &[u8],
    details: &KittyTransmitDetails,
    limit: usize,
) -> Result<Vec<u8>, &'static str> {
    let name = std::str::from_utf8(data).map_err(|_| "EINVAL: name is not UTF-8")?;
    let name = name.strip_prefix('/').unwrap_or(name);
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return Err("EINVAL: bad shared memory name");
    }
    if !cfg!(target_os = "linux") {
        return Err("EINVAL: shared memory transmission is not supported");
    }
    let path = Path::new("/dev/shm").join(name);
    let bytes = read_regular(&path, details, limit);
    // The object is the program's handoff, consumed whether or not it held a valid image.
    let _ = std::fs::remove_file(&path);
    bytes
}

/// Decompress zlib `bytes`, at most `limit` bytes of output.
fn inflate(bytes: &[u8], limit: usize) -> Result<Vec<u8>, &'static str> {
    let mut output = Vec::new();
    flate2::read::ZlibDecoder::new(bytes)
        .take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut output)
        .map_err(|_| "EINVAL: bad zlib data")?;
    if output.len() > limit {
        return Err("EFBIG: image data too large");
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::load;
    use crate::model::{KittyMedium, KittyTransmitDetails};
    use std::io::Write;

    fn details(medium: KittyMedium) -> KittyTransmitDetails {
        KittyTransmitDetails {
            medium,
            ..KittyTransmitDetails::default()
        }
    }

    fn temporary(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("{name}-{}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn direct_and_compressed_payloads_load_within_the_limit() {
        assert_eq!(
            load(b"abc".to_vec(), &details(KittyMedium::Direct), 3),
            Ok(b"abc".to_vec())
        );
        assert!(load(b"abcd".to_vec(), &details(KittyMedium::Direct), 3).is_err());
        let mut compressed = Vec::new();
        let mut encoder =
            flate2::write::ZlibEncoder::new(&mut compressed, flate2::Compression::default());
        encoder.write_all(&[7; 100]).unwrap();
        encoder.finish().unwrap();
        let zlib = KittyTransmitDetails {
            compressed: true,
            ..details(KittyMedium::Direct)
        };
        assert_eq!(load(compressed.clone(), &zlib, 100), Ok(vec![7; 100]));
        assert_eq!(
            load(compressed, &zlib, 99),
            Err("EFBIG: image data too large")
        );
        assert_eq!(
            load(b"not zlib".to_vec(), &zlib, 100),
            Err("EINVAL: bad zlib data")
        );
    }

    #[test]
    fn files_are_read_from_their_offset_and_temporary_files_are_consumed() {
        let file = temporary("bmux-kitty-file", b"0123456789");
        let ranged = KittyTransmitDetails {
            size: Some(4),
            offset: 3,
            ..details(KittyMedium::File)
        };
        let path = file.to_str().unwrap().as_bytes().to_vec();
        assert_eq!(load(path.clone(), &ranged, 64), Ok(b"3456".to_vec()));
        assert!(file.exists(), "a file is the program's own");
        assert_eq!(
            load(path.clone(), &details(KittyMedium::TempFile), 64),
            Err("EPERM: not a temporary file"),
            "a temporary file must say it is one"
        );
        std::fs::remove_file(&file).unwrap();

        let handoff = temporary("bmux-tty-graphics-protocol", b"pixels");
        let path = handoff.to_str().unwrap().as_bytes().to_vec();
        assert_eq!(
            load(path, &details(KittyMedium::TempFile), 64),
            Ok(b"pixels".to_vec())
        );
        assert!(!handoff.exists(), "a temporary file is deleted once read");
    }

    #[test]
    fn named_media_outside_what_kitty_reads_are_refused() {
        let file = details(KittyMedium::File);
        assert_eq!(
            load(b"relative/path".to_vec(), &file, 64),
            Err("EINVAL: path is not absolute")
        );
        assert_eq!(
            load(b"/nonexistent/bmux-kitty".to_vec(), &file, 64),
            Err("ENOENT: no such file")
        );
        assert_eq!(
            load(b"/dev/null".to_vec(), &file, 64),
            Err("EPERM: file not allowed")
        );
        let directory = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(
            load(directory.to_str().unwrap().as_bytes().to_vec(), &file, 64),
            Err("EINVAL: not a regular file")
        );
        assert_eq!(
            load(b"/a/b".to_vec(), &details(KittyMedium::SharedMemory), 64),
            Err("EINVAL: bad shared memory name")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shared_memory_objects_are_read_and_unlinked() {
        let name = format!("bmux-kitty-intake-{}", std::process::id());
        let path = std::path::Path::new("/dev/shm").join(&name);
        std::fs::write(&path, b"shared").unwrap();
        assert_eq!(
            load(
                format!("/{name}").into_bytes(),
                &details(KittyMedium::SharedMemory),
                64
            ),
            Ok(b"shared".to_vec())
        );
        assert!(!path.exists());
    }
}
