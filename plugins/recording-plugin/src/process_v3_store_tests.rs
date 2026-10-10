#[cfg(test)]
mod tests {
    use super::*;
    use bmux_recording_plugin_api::{
        recording_process_recovery_v2::CaptureState, recording_types::RecordingProfile,
    };
    fn intent() -> ProcessRecord {
        ProcessRecord {
            capture_owner_epoch: None,
            status: contract::ProcessStatus {
                schema_version: 3,
                run_id: Uuid::new_v4(),
                revision: 1,
                launch_mutation_id: Uuid::new_v4(),
                recording_id: None,
                runtime_root: "/isolated".into(),
                policy: contract::LifetimePolicy::ConventionalTerminalSession,
                reservation: None,
                authority: None,
                terminal_session_ended: false,
                source_evidence_complete: false,
                cursor: None,
                capture: CaptureState::Pending,
                recovery_required: true,
                errors: vec![],
            },
            options: process::ProgramOptions {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "printf ready".into()],
                cwd: None,
                env: std::collections::BTreeMap::new(),
                cols: 80,
                rows: 24,
                profile: RecordingProfile::Full,
            },
        }
    }
    #[test]
    fn versioned_intent_survives_restart_and_conflicts_do_not_mutate() {
        let root = std::env::temp_dir().join(format!("bmux-process-v3-{}", Uuid::new_v4()));
        let store = ProcessStore::new(&root).unwrap();
        let mut value = intent();
        store.admit(&value).unwrap();
        let reopened = ProcessStore::new(&root).unwrap();
        assert_eq!(
            reopened.admit(&value).unwrap().status.launch_mutation_id,
            value.status.launch_mutation_id
        );
        value.options.args.push("different".into());
        assert!(reopened.admit(&value).is_err());
        assert_eq!(
            reopened
                .load(value.status.run_id)
                .unwrap()
                .options
                .args
                .len(),
            2
        );
        value.options.args.pop();
        value.status.capture = CaptureState::Gap;
        reopened.replace(1, &mut value).unwrap();
        value.status.capture = CaptureState::Contiguous;
        assert!(reopened.replace(2, &mut value).is_err());
        assert!(ProcessStore::require_finalizable(&value).is_err());
        assert!(root.join("process-v3").is_dir());
        assert!(!root.join("process-recovery-v2").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn raw_recovery_cannot_be_claimed_as_full_profile_recording() {
        let mut value = intent();
        value.status.terminal_session_ended = true;
        assert!(ProcessStore::require_finalizable(&value).is_err());
        value.status.source_evidence_complete = true;
        assert!(
            ProcessStore::require_finalizable(&value).is_err(),
            "missing authoritative spool cannot finalize"
        );
    }
}
