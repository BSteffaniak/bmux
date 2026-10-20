//! Linux candidate discovery without external executables or inherited PATH.
//! Only discovery: the joining helper still revalidates session membership.
use std::collections::BTreeSet;
use std::io::{self, Read};
use std::time::Instant;

fn parse_stat(bytes: &[u8]) -> io::Result<(i32, i32, bool)> {
    // comm is parenthesized and can contain spaces, ')' and non-UTF8 bytes.
    let end = bytes
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or_else(|| io::Error::other("missing proc stat command"))?;
    let tail = std::str::from_utf8(&bytes[end + 1..]).map_err(io::Error::other)?;
    let mut fields = tail.split_whitespace();
    let state = fields
        .next()
        .ok_or_else(|| io::Error::other("missing proc state"))?;
    let _parent: i32 = fields
        .next()
        .ok_or_else(|| io::Error::other("missing proc parent"))?
        .parse()
        .map_err(io::Error::other)?;
    let group = fields
        .next()
        .ok_or_else(|| io::Error::other("missing proc group"))?
        .parse()
        .map_err(io::Error::other)?;
    let session = fields
        .next()
        .ok_or_else(|| io::Error::other("missing proc session"))?
        .parse()
        .map_err(io::Error::other)?;
    Ok((group, session, matches!(state, "Z" | "X" | "x")))
}

pub(super) fn groups(
    session: i32,
    own_group: i32,
    protected_group: Option<u32>,
    deadline: Instant,
) -> io::Result<BTreeSet<i32>> {
    let mut result = BTreeSet::new();
    for entry in std::fs::read_dir("/proc")? {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "process scan timed out",
            ));
        }
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        // Avoid opening every process's stat file on busy hosts. This syscall
        // is discovery only; stat and the join helper revalidate membership.
        let candidate_session = unsafe { libc::getsid(pid) };
        if candidate_session < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                continue;
            }
            return Err(error);
        }
        if candidate_session != session {
            continue;
        }
        let mut bytes = Vec::new();
        let read = std::fs::File::open(entry.path().join("stat"))
            .and_then(|file| file.take(8193).read_to_end(&mut bytes));
        match read {
            // Exiting processes can disappear between enumeration and read.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        if bytes.len() > 8192 {
            return Err(io::Error::other("proc stat exceeds limit"));
        }
        let (group, candidate_session, exited) = parse_stat(&bytes)?;
        if !exited
            && candidate_session == session
            && group > 0
            && group != session
            && group != own_group
            && u32::try_from(group).ok() != protected_group
        {
            result.insert(group);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_names_cannot_confuse_stat_fields() {
        assert_eq!(
            parse_stat(b"12 (odd ) name\xff) S 1 42 7 0\n").unwrap(),
            (42, 7, false)
        );
        assert_eq!(parse_stat(b"12 (zombie) Z 1 42 7").unwrap(), (42, 7, true));
        assert!(parse_stat(b"12 (truncated) S 1").is_err());
        assert!(parse_stat(b"12 (invalid) S 1 nope 7").is_err());
    }

    #[test]
    fn scan_excludes_own_and_protected_groups_and_honors_deadline() {
        let session = unsafe { libc::getsid(0) };
        let own = unsafe { libc::getpgrp() };
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        let found = groups(session, own, None, deadline).unwrap();
        assert!(!found.contains(&own));
        assert!(!found.contains(&session));
        if let Some(group) = found.first() {
            assert!(
                !groups(session, own, u32::try_from(*group).ok(), deadline)
                    .unwrap()
                    .contains(group)
            );
        }
        assert_eq!(
            groups(session, own, None, Instant::now())
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }
}
