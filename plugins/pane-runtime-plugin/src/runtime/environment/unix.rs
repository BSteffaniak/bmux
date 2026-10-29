use bmux_pane_runtime_plugin_api::pane_runtime_environment_v1::EnvironmentEntry;
use std::os::unix::ffi::OsStrExt as _;

pub(super) const ENCODING: &str = "unix-bytes";
pub(super) fn encode(value: &std::ffi::OsStr) -> Vec<u8> {
    value.as_bytes().to_vec()
}
pub(super) fn keys_equal(left: &[u8], right: &[u8]) -> bool {
    left == right
}
pub(super) fn valid(entry: &EnvironmentEntry) -> bool {
    entry
        .encoding
        .as_deref()
        .is_none_or(|encoding| encoding == ENCODING)
        && !entry.key.is_empty()
        && !entry.key.contains(&0)
        && !entry.key.contains(&b'=')
        && !entry.value.contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arbitrary_unix_bytes_and_legacy_encoding_are_preserved() {
        let entry = EnvironmentEntry {
            key: b"NATIVE".to_vec(),
            value: vec![255],
            encoding: None,
        };
        assert!(valid(&entry));
        assert_eq!(encode(std::ffi::OsStr::from_bytes(&[255])), vec![255]);
    }
}
