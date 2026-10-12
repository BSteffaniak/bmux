#[cfg(test)]
mod tests {
    use super::*;
    use bmux_recording_protocol::{
        RecordingEventEnvelope, RecordingEventKind, RecordingPayload, write_frame,
    };
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn synchronous_process_dispatch_hands_off_original_runtime_timer_driver() {
        // A single worker makes scheduler handoff necessary, not probabilistic.
        // Run dispatch on that worker (rather than the test's block_on thread).
        let owner = tokio::runtime::Handle::current();
        tokio::spawn(async move {
            let progressed = drive_process_operation(move || async move {
                let (sender, receiver) = tokio::sync::oneshot::channel();
                let timer = owner.spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    let _ = sender.send(());
                });
                // This watchdog uses the independent dispatch runtime. Without
                // handoff the original runtime cannot drive the timer; the test
                // fails in bounded time instead of hanging like the CLI fixture.
                let progressed = tokio::time::timeout(std::time::Duration::from_secs(2), receiver)
                    .await
                    .is_ok_and(|result| result.is_ok());
                timer.abort();
                progressed
            })
            .unwrap();
            assert!(
                progressed,
                "original runtime timer starved by synchronous dispatch"
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn synchronous_process_dispatch_accepts_current_thread_caller() {
        let caller_thread = std::thread::current().id();
        let result = drive_process_operation(move || async move {
            assert_ne!(std::thread::current().id(), caller_thread);
            assert_eq!(
                tokio::runtime::Handle::current().runtime_flavor(),
                tokio::runtime::RuntimeFlavor::CurrentThread,
            );
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            42
        })
        .unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn synchronous_process_dispatch_accepts_no_runtime_caller_and_reports_panic() {
        assert!(tokio::runtime::Handle::try_current().is_err());
        let caller_thread = std::thread::current().id();
        let result = drive_process_operation(move || async move {
            assert_ne!(std::thread::current().id(), caller_thread);
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            42
        })
        .unwrap();
        assert_eq!(result, 42);
        let failed: Result<()> =
            drive_process_operation(|| async { panic!("dispatch operation panic") });
        assert!(
            failed
                .unwrap_err()
                .to_string()
                .contains("process service dispatch panicked")
        );
    }

    #[tokio::test]
    async fn durable_writer_ack_survives_stop_without_changing_original_closure() {
        let root = std::env::temp_dir().join(format!("bmux-writer-ack-{}", Uuid::new_v4()));
        let mut runtime = crate::recording_runtime::RecordingRuntime::new(root.clone(), 64, 30);
        runtime.enable_process_capture();
        let summary = runtime
            .start(
                None,
                true,
                None,
                bmux_recording_protocol::RecordingProfile::Full,
                vec![RecordingEventKind::PaneOutputRaw],
            )
            .unwrap();
        let acknowledgment = runtime.writer_ack(summary.id).unwrap();
        runtime
            .record(
                RecordingEventKind::PaneOutputRaw,
                RecordingPayload::Bytes {
                    data: b"durable".to_vec(),
                },
                bmux_recording_runtime::RecordMeta::default(),
            )
            .unwrap();
        crate::recording_runtime::writer_ack::wait_for_watermark(
            acknowledgment.clone(),
            7,
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert!(!acknowledgment.borrow().closed);
        let directory = root.join(summary.id.to_string());
        assert_eq!(
            std::fs::read(directory.join("output.live")).unwrap(),
            b"durable"
        );
        let stopped = runtime.stop(Some(summary.id)).unwrap();
        assert!(stopped.ended_epoch_ms.is_some());
        assert_eq!(stopped.event_count, 1);
        assert!(acknowledgment.borrow().closed);
        crate::recording_runtime::writer_ack::wait_for_watermark(
            runtime.writer_ack(summary.id).unwrap(),
            7,
            tokio::time::Instant::now(),
        )
        .await
        .unwrap();
        let mut value = ProcessService::initial_status(
            Uuid::new_v4(),
            Uuid::new_v4(),
            summary.id,
            "owned".into(),
        );
        value.execution = Some(process::ExecutionRef {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            execution_token: Uuid::new_v4(),
        });
        value.output_watermark = 7;
        let closure = closed_original_events(&directory, &value, 7).unwrap();
        crate::durable::write_atomic(
            &directory.join("original-writer-closure.json"),
            &serde_json::to_vec(&closure).unwrap(),
        )
        .unwrap();
        assert!(original_writer_closed(&directory, &value).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn original_writer_closure_survives_derived_repair_but_rejects_event_corruption() {
        let root = std::env::temp_dir().join(format!("bmux-writer-closure-{}", Uuid::new_v4()));
        let mut runtime = crate::recording_runtime::RecordingRuntime::new(root.clone(), 64, 30);
        let summary = runtime
            .start(
                None,
                true,
                None,
                bmux_recording_protocol::RecordingProfile::Full,
                vec![RecordingEventKind::PaneOutputRaw],
            )
            .unwrap();
        runtime
            .record(
                RecordingEventKind::PaneOutputRaw,
                RecordingPayload::Bytes {
                    data: b"original".to_vec(),
                },
                bmux_recording_runtime::RecordMeta::default(),
            )
            .unwrap();
        runtime.stop(Some(summary.id)).unwrap();
        let mut value = ProcessService::initial_status(
            Uuid::new_v4(),
            Uuid::new_v4(),
            summary.id,
            "owned".into(),
        );
        value.execution = Some(process::ExecutionRef {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            execution_token: Uuid::new_v4(),
        });
        value.output_watermark = 8;
        let directory = root.join(summary.id.to_string());
        assert!(!original_writer_closed(&directory, &value).unwrap());
        let closure = closed_original_events(&directory, &value, 8).unwrap();
        crate::durable::write_atomic(
            &directory.join("original-writer-closure.json"),
            &serde_json::to_vec(&closure).unwrap(),
        )
        .unwrap();
        std::fs::write(directory.join("output.live"), b"damaged").unwrap();
        assert!(original_writer_closed(&directory, &value).unwrap());
        std::fs::write(directory.join("output.live"), b"original").unwrap();
        assert!(original_writer_closed(&directory, &value).unwrap());
        std::fs::write(directory.join("events_0.bin"), b"truncated").unwrap();
        assert!(original_writer_closed(&directory, &value).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn launch_root_alias_preserves_admitted_identity_and_rejects_changed_root() {
        let parent = std::env::temp_dir().join(format!("bmux-launch-alias-{}", Uuid::new_v4()));
        let owner = parent.join("owned");
        let other = parent.join("other");
        let alias = parent.join("alias");
        std::fs::create_dir_all(&owner).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::os::unix::fs::symlink(&owner, &alias).unwrap();
        let admitted = owner.to_str().unwrap();
        let verified = verified_launch_root(alias.to_str().unwrap(), admitted).unwrap();
        assert_eq!(verified, admitted);
        let canonical = std::fs::canonicalize(&owner).unwrap();
        assert_eq!(
            verified_launch_root(canonical.to_str().unwrap(), admitted).unwrap(),
            admitted
        );
        let store = ProcessStore::new(&parent.join("recordings")).unwrap();
        let id = Uuid::new_v4();
        let mutation = Uuid::new_v4();
        let original = serde_json::to_vec(&(id, mutation, "same options", admitted)).unwrap();
        let replay = serde_json::to_vec(&(id, mutation, "same options", verified)).unwrap();
        assert!(!store.admit_launch(id, mutation, &original).unwrap());
        assert!(store.admit_launch(id, mutation, &replay).unwrap());
        assert!(verified_launch_root(other.to_str().unwrap(), admitted).is_err());
        assert!(verified_launch_root(parent.join("missing").to_str().unwrap(), admitted).is_err());
        assert!(
            store
                .admit_launch(id, mutation, b"changed fingerprint")
                .is_err()
        );
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn acknowledged_input_publication_is_profile_filtered_and_key_deduplicated() {
        for (profile, expected) in [
            (
                bmux_recording_plugin_api::recording_types::RecordingProfile::Full,
                1,
            ),
            (
                bmux_recording_plugin_api::recording_types::RecordingProfile::Functional,
                1,
            ),
            (
                bmux_recording_plugin_api::recording_types::RecordingProfile::Visual,
                0,
            ),
        ] {
            let root = std::env::temp_dir().join(format!("bmux-input-profile-{}", Uuid::new_v4()));
            let store = ProcessStore::new(&root).unwrap();
            let mut runtime = crate::recording_runtime::RecordingRuntime::new(root.clone(), 64, 30);
            runtime
                .start(
                    None,
                    true,
                    None,
                    profile.into(),
                    crate::commands::default_event_kinds_for_profile(profile, true)
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                )
                .unwrap();
            let run = Uuid::new_v4();
            let mutation = Uuid::new_v4();
            for _ in 0..2 {
                store
                    .record_input_once(run, mutation, || {
                        runtime.record(
                            RecordingEventKind::PaneInputRaw,
                            RecordingPayload::Bytes {
                                data: b"input\r".to_vec(),
                            },
                            bmux_recording_runtime::RecordMeta {
                                session_id: Some(Uuid::new_v4()),
                                pane_id: Some(Uuid::new_v4()),
                                client_id: None,
                            },
                        )?;
                        Ok(())
                    })
                    .unwrap();
            }
            assert_eq!(runtime.stop(None).unwrap().event_count, expected);
            // An interrupted original event cannot be replayed with new timing.
            let interrupted = Uuid::new_v4();
            assert!(
                store
                    .record_input_once(run, interrupted, || anyhow::bail!(
                        "interrupted original owner"
                    ))
                    .is_err()
            );
            assert!(
                store
                    .record_input_once(run, interrupted, || panic!(
                        "must not invent recovered input event"
                    ))
                    .is_err()
            );
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    struct CaptureLimitClient;
    impl bmux_plugin_sdk::TypedDispatchClient for CaptureLimitClient {
        async fn invoke_service_raw(
            &mut self,
            _capability: &str,
            _kind: bmux_ipc::InvokeServiceKind,
            interface: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> bmux_plugin_sdk::TypedDispatchClientResult<Vec<u8>> {
            assert_eq!(interface, pane::INTERFACE_ID.as_str());
            assert_eq!(operation, "capture-resume");
            let request: pane::client::CaptureResumeRequest =
                bmux_plugin_sdk::decode_service_message(&payload).unwrap();
            assert_eq!(request.max_bytes, 262_144);
            let result: Result<pane::TerminalCaptureResponse, pane::TerminalRecoveryError> =
                Ok(pane::TerminalCaptureResponse::Batch {
                    value: pane::TerminalCaptureBatch {
                        cursor: request.cursor.clone(),
                        continuation: request.cursor,
                        data: vec![],
                        complete: true,
                    },
                });
            Ok(bmux_plugin_sdk::encode_service_message(&result).unwrap())
        }
    }
    #[test]
    fn repair_and_verification_capture_requests_respect_authority_read_limit() {
        let cursor = pane::TerminalCaptureCursor {
            execution: pane::TerminalExecutionRef {
                session_id: Uuid::new_v4(),
                pane_id: Uuid::new_v4(),
                execution_token: Uuid::new_v4(),
            },
            spool_generation: Uuid::new_v4(),
            offset: 0,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .block_on(capture_resume(&mut CaptureLimitClient, cursor))
            .unwrap();
    }

    #[test]
    fn byte_cursor_is_bounded_and_preserves_binary_output() {
        let root = std::env::temp_dir().join(format!("bmux-process-read-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let session_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let value = process::RunStatus {
            schema_version: 4,
            run_id: Uuid::new_v4(),
            revision: 1,
            launch_mutation_id: Uuid::new_v4(),
            recording_id: Uuid::new_v4(),
            execution: Some(process::ExecutionRef {
                session_id,
                pane_id,
                execution_token: Uuid::new_v4(),
            }),
            runtime_root: "/isolated/runtime".into(),
            phase: process::RunPhase::Finalized,
            exit_code: Some(0),
            target_exited: true,
            terminal_session_ended: true,
            capture_eof: true,
            capture_watermark: 4,
            recording_watermark: 4,
            durable_recording_complete: true,
            timing_complete: true,
            cleanup_complete: false,
            recovery_required: false,
            output_drained: true,
            output_watermark: 4,
            capture_finalized: true,
            errors: vec![],
        };
        let event: RecordingEventEnvelope<bmux_ipc::Event, bmux_ipc::ErrorCode> =
            RecordingEventEnvelope {
                seq: 1,
                mono_ns: 0,
                wall_epoch_ms: 0,
                session_id: Some(session_id),
                pane_id: Some(pane_id),
                client_id: None,
                kind: RecordingEventKind::PaneOutputRaw,
                payload: RecordingPayload::Bytes {
                    data: vec![0, 255, 1, 2],
                },
            };
        let mut file = std::fs::File::create(root.join("events_0.bin")).unwrap();
        write_frame(&mut file, &event).unwrap();
        std::fs::write(root.join("output.live"), [0, 255, 1, 2]).unwrap();
        let first = read_output(&root, &value, 0, 2).unwrap();
        assert_eq!(first.data, [0, 255]);
        assert_eq!(first.next_cursor, 2);
        let next = read_output(&root, &value, first.next_cursor, 8).unwrap();
        assert_eq!(next.data, [1, 2]);
        assert!(next.complete);
        assert!(read_output(&root, &value, 5, 8).is_err());
        assert!(read_output(&root, &value, 0, 0).is_err());
        let bounded = read_output_through(&root, &value, 0, 8, Some(2)).unwrap();
        assert_eq!(bounded.data, [0, 255]);
        assert!(!bounded.complete);
        assert!(read_output_through(&root, &value, 3, 8, Some(2)).is_err());
        assert!(read_output_through(&root, &value, 0, 8, Some(5)).is_err());
        // A drained execution watermark is exact, not a lower bound that
        // permits unrelated or corrupt suffix bytes to appear complete.
        std::fs::write(root.join("output.live"), [0, 255, 1, 2, 3]).unwrap();
        assert!(read_output(&root, &value, 0, 8).is_err());
        // A pending writer suffix is explicitly incomplete until caught up.
        std::fs::write(root.join("output.live"), [0, 255]).unwrap();
        assert!(!read_output(&root, &value, 0, 8).unwrap().complete);
        std::fs::remove_dir_all(root).unwrap();
    }
}
