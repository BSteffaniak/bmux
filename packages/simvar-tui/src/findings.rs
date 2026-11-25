//! Failures that outlive a dashboard session.
//!
//! Each failure group is saved as a JSON file under a findings directory (by default `target/sim-findings/<simulation>/`), so restarting the
//! dashboard keeps them, and a later build can re-check them.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::store::FailureGroup;

/// One saved failure group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// The group's signature.
    pub signature: String,
    /// The first failing run's seed.
    pub seed: u64,
    /// Its epoch offset.
    pub epoch_offset: u64,
    /// Its report.
    pub report: String,
    /// Its shape tags.
    pub shape: Vec<String>,
    /// How many runs failed this way.
    pub count: u64,
    /// The build it was found with ([`build_identity`]).
    pub build: String,
    /// Whether a replay reproduced it.
    pub reproduced: Option<bool>,
    /// The smallest failing schedule, once shrunk.
    pub schedule: Option<String>,
    /// Choices in the original and the smallest schedule, once shrunk.
    pub shrunk: Option<(u64, u64)>,
    /// When it was last seen (seconds since the Unix epoch).
    pub last_seen: u64,
}

impl Finding {
    /// The finding for `group`, found with `build`.
    #[must_use]
    pub fn of(group: &FailureGroup, build: &str) -> Self {
        let shrink = group.shrink.as_ref().filter(|s| s.done);
        Self {
            signature: group.signature.clone(),
            seed: group.seed,
            epoch_offset: group.epoch_offset,
            report: group.report.clone(),
            shape: group.shape.clone(),
            count: group.count,
            build: build.to_owned(),
            reproduced: group.reproduced,
            schedule: shrink.and_then(|s| s.schedule.clone()),
            shrunk: shrink.map(|s| (s.original, s.best)),
            last_seen: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        }
    }

    /// The failure group this finding describes (seen at `now`; whether it reproduced is
    /// unknown when it came from another build).
    #[must_use]
    pub fn group(&self, build: &str, now: Instant) -> FailureGroup {
        FailureGroup {
            signature: self.signature.clone(),
            seed: self.seed,
            epoch_offset: self.epoch_offset,
            report: self.report.clone(),
            shape: self.shape.clone(),
            runs: Vec::new(),
            count: self.count,
            first_seen: now,
            last_seen: now,
            reproduced: (self.build == build).then_some(self.reproduced).flatten(),
            confirming: None,
            shrink: None,
            verifying: None,
            verification: None,
            saved: true,
            stale: self.build != build,
        }
    }
}

/// A findings directory.
#[derive(Debug, Clone)]
pub struct Findings {
    dir: PathBuf,
}

impl Findings {
    /// Findings under `dir` (created when first saved to).
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Every saved finding (unreadable files are skipped), in file name order.
    #[must_use]
    pub fn load(&self) -> Vec<Finding> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|e| e == "json"))
            .collect();
        paths.sort();
        paths
            .iter()
            .filter_map(|path| std::fs::read(path).ok())
            .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
            .collect()
    }

    /// Delete the finding with `signature`, if saved.
    ///
    /// # Errors
    ///
    /// * If it exists and cannot be deleted
    pub fn forget(&self, signature: &str) -> io::Result<()> {
        match std::fs::remove_file(self.path(signature)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    fn layout_path(&self) -> PathBuf {
        self.dir.join("layout.splits")
    }

    /// The dashboard's divider positions saved last (none when unsaved or unreadable).
    #[must_use]
    pub fn load_layout(&self) -> BTreeMap<String, u16> {
        std::fs::read(self.layout_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Save the dashboard's divider positions, for the next session.
    ///
    /// # Errors
    ///
    /// * If the directory or file cannot be written
    pub fn save_layout(&self, splits: &BTreeMap<String, u16>) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.layout_path();
        let staging = path.with_extension("splits.tmp");
        std::fs::write(
            &staging,
            serde_json::to_vec_pretty(splits).map_err(io::Error::other)?,
        )?;
        std::fs::rename(staging, path)
    }

    fn path(&self, signature: &str) -> PathBuf {
        self.dir.join(format!("{:016x}.json", fnv(signature)))
    }

    /// Save `finding`, replacing an earlier one with the same signature.
    ///
    /// # Errors
    ///
    /// * If the directory or file cannot be written
    pub fn save(&self, finding: &Finding) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.path(&finding.signature);
        let staging = path.with_extension("json.tmp");
        std::fs::write(
            &staging,
            serde_json::to_vec_pretty(finding).map_err(io::Error::other)?,
        )?;
        std::fs::rename(staging, path)
    }
}

/// The findings directory for `simulation`.
///
/// It is under `SIMULATOR_FINDINGS` when set, else `target/sim-findings/<simulation>` in the running executable's target directory when it can
/// be found, else under the working directory.
#[must_use]
pub fn default_dir(simulation: &str) -> PathBuf {
    if let Some(dir) = std::env::var_os("SIMULATOR_FINDINGS") {
        return PathBuf::from(dir).join(simulation);
    }
    let target = std::env::current_exe().ok().and_then(|exe| {
        exe.ancestors()
            .find(|dir| dir.file_name().is_some_and(|name| name == "target"))
            .map(Path::to_path_buf)
    });
    target
        .unwrap_or_else(|| PathBuf::from("target"))
        .join("sim-findings")
        .join(simulation)
}

/// An identity for the running build: seeds replay only on the same build. Derived from the
/// executable's size and modification time.
#[must_use]
pub fn build_identity() -> String {
    let Ok(metadata) = std::env::current_exe().and_then(std::fs::metadata) else {
        return "unknown".into();
    };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    format!("{:016x}", fnv(&format!("{}:{modified}", metadata.len())))
}

fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn findings_round_trip_and_replace_by_signature() {
        let dir = std::env::temp_dir().join(format!(
            "bmux-findings-{}-{}",
            std::process::id(),
            fnv(&format!("{:?}", Instant::now()))
        ));
        let findings = Findings::new(&dir);
        let mut finding = Finding {
            signature: "4. cleanup".into(),
            seed: 1,
            epoch_offset: 2,
            report: "r".into(),
            shape: vec!["personality:linux".into()],
            count: 1,
            build: "b".into(),
            reproduced: Some(true),
            schedule: None,
            shrunk: None,
            last_seen: 0,
        };
        findings.save(&finding).unwrap();
        finding.count = 5;
        findings.save(&finding).unwrap();
        assert_eq!(findings.load(), [finding.clone()]);
        let group = finding.group("other build", Instant::now());
        assert_eq!(group.reproduced, None);
        assert!(group.saved);
        assert_eq!(finding.group("b", Instant::now()).reproduced, Some(true));
        assert!(group.stale);
        assert!(!finding.group("b", Instant::now()).stale);
        findings.forget(&finding.signature).unwrap();
        assert_eq!(findings.load(), []);
        findings.forget(&finding.signature).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
