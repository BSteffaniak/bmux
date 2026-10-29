use bmux_pane_runtime_plugin_api::pane_runtime_environment_v1::EnvironmentEntry;
use std::os::windows::ffi::OsStrExt as _;
use windows_sys::Win32::Globalization::CompareStringOrdinal;

pub(super) const ENCODING: &str = "windows-utf16le";
pub(super) fn encode(value: &std::ffi::OsStr) -> Vec<u8> {
    value.encode_wide().flat_map(u16::to_le_bytes).collect()
}
fn units(bytes: &[u8]) -> Option<Vec<u16>> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    Some(
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect(),
    )
}
pub(super) fn keys_equal(left: &[u8], right: &[u8]) -> bool {
    let (Some(left), Some(right)) = (units(left), units(right)) else {
        return false;
    };
    let (Ok(left_len), Ok(right_len)) = (i32::try_from(left.len()), i32::try_from(right.len()))
    else {
        return false;
    };
    // SAFETY: both buffers contain initialized UTF-16 units for their explicit lengths.
    unsafe { CompareStringOrdinal(left.as_ptr(), left_len, right.as_ptr(), right_len, 1) == 2 }
}
pub(super) fn valid(entry: &EnvironmentEntry) -> bool {
    if entry.encoding.as_deref() != Some(ENCODING) {
        return false;
    }
    let (Some(key), Some(value)) = (units(&entry.key), units(&entry.value)) else {
        return false;
    };
    crate::windows_environment::valid_key(&key) && !value.contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStringExt as _;
    #[test]
    fn native_surrogates_are_not_lossily_converted() {
        let value = std::ffi::OsString::from_wide(&[0xd800]);
        assert_eq!(encode(&value), vec![0, 0xd8]);
        assert!(valid(&EnvironmentEntry {
            key: encode(std::ffi::OsStr::new("NATIVE")),
            value: encode(&value),
            encoding: Some(ENCODING.into())
        }));
    }
    #[test]
    fn keys_follow_windows_case_insensitive_identity() {
        assert!(keys_equal(
            &encode(std::ffi::OsStr::new("Path")),
            &encode(std::ffi::OsStr::new("PATH"))
        ));
    }
    #[test]
    fn legacy_unix_and_malformed_utf16_are_rejected() {
        assert!(!valid(&EnvironmentEntry {
            key: b"A".to_vec(),
            value: vec![],
            encoding: None
        }));
        assert!(!valid(&EnvironmentEntry {
            key: vec![1],
            value: vec![],
            encoding: Some(ENCODING.into())
        }));
    }
}
