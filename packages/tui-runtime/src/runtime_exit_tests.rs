//! Lifecycle and default-stack regressions for the borrowing scheduler loop.

use std::sync::{Arc, Mutex};

use crate::{
    HeadlessPresenter, PresentReport, Presenter, Program, ResetReason, Runtime, RuntimeConfig,
    RuntimeError, RuntimeEvent, Update,
};

type Log = Arc<Mutex<Vec<&'static str>>>;

struct App {
    // Representative inline consumer state, deliberately not heap-indirected.
    state: [u8; 16_000],
    fail: bool,
    log: Log,
}

impl Program for App {
    type Message = ();
    type Error = &'static str;

    fn update(&mut self, _: RuntimeEvent<()>) -> Result<Update<()>, Self::Error> {
        self.log.lock().unwrap().push("update");
        if self.fail {
            Err("program")
        } else {
            self.state[15_999] = 73;
            Ok(Update::exit())
        }
    }
}

struct Output {
    fail_present: bool,
    fail_restore: bool,
    log: Log,
}

impl Presenter<App> for Output {
    type Error = &'static str;

    fn reset(&mut self, _: ResetReason) {}

    fn present(&mut self, _: &mut App) -> Result<PresentReport, Self::Error> {
        self.log.lock().unwrap().push("present");
        if self.fail_present {
            Err("presenter")
        } else {
            Ok(PresentReport::default())
        }
    }

    fn cancel_present(&mut self) {
        self.log.lock().unwrap().push("cancel");
    }

    fn restore(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> {
        futures_util::future::lazy(move |_| {
            self.log.lock().unwrap().push("restore");
            if self.fail_restore {
                Err("restore")
            } else {
                Ok(())
            }
        })
    }
}

#[tokio::test]
async fn finalization_preserves_original_and_cleanup_errors_and_ownership() {
    for (program_error, presentation_error, restore_error) in [
        (false, false, false),
        (false, false, true),
        (true, false, false),
        (true, false, true),
        (false, true, false),
        (false, true, true),
    ] {
        let log = Log::default();
        let (runtime, handle) = Runtime::new(
            App {
                state: [0; 16_000],
                fail: program_error,
                log: Arc::clone(&log),
            },
            Output {
                fail_present: presentation_error,
                fail_restore: restore_error,
                log: Arc::clone(&log),
            },
            RuntimeConfig::default(),
        );
        // Presentation errors must be exercised before an exit update bypasses painting.
        if !presentation_error {
            handle.try_send(()).unwrap();
        }
        let result = Box::pin(runtime.run()).await;
        let output = match result {
            Ok(output) => {
                assert!(!program_error && !presentation_error && !restore_error);
                output
            }
            Err(RuntimeError::Program {
                error,
                cleanup_error,
                output,
            }) => {
                assert!(program_error);
                assert_eq!(error, "program");
                assert_eq!(cleanup_error, restore_error.then_some("restore"));
                *output
            }
            Err(RuntimeError::Presenter {
                error,
                cleanup_error,
                output,
            }) => {
                assert!(!program_error);
                assert_eq!(
                    error,
                    if presentation_error {
                        "presenter"
                    } else {
                        "restore"
                    }
                );
                assert_eq!(
                    cleanup_error,
                    (presentation_error && restore_error).then_some("restore")
                );
                *output
            }
        };
        assert_eq!(
            output.program.state[15_999],
            if program_error || presentation_error {
                0
            } else {
                73
            }
        );
        assert_eq!(output.presenter.fail_restore, restore_error);
        let events = log.lock().unwrap().clone();
        assert_eq!(&events[events.len() - 2..], &["cancel", "restore"]);
        assert_eq!(
            events.iter().filter(|event| **event == "restore").count(),
            1
        );
    }
}

#[test]
fn inline_consumer_runs_on_a_512_kib_thread_stack() {
    std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(|| {
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            executor.block_on(async {
                let (runtime, handle) = Runtime::new(
                    App {
                        state: [0; 16_000],
                        fail: false,
                        log: Log::default(),
                    },
                    HeadlessPresenter::default(),
                    RuntimeConfig::default(),
                );
                handle.try_send(()).unwrap();
                let output = Box::pin(runtime.run())
                    .await
                    .unwrap_or_else(|_| panic!("runtime failed"));
                assert_eq!(output.program.state[15_999], 73);
            });
        })
        .unwrap()
        .join()
        .unwrap();
}

impl Drop for App {
    fn drop(&mut self) {
        self.log.lock().unwrap().push("drop-program");
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.log.lock().unwrap().push("drop-presenter");
    }
}

#[tokio::test]
async fn dropping_pending_runtime_releases_both_owners_and_closes_admission() {
    use std::future::Future;
    let log = Log::default();
    let (runtime, handle) = Runtime::new(
        App {
            state: [0; 16_000],
            fail: false,
            log: Arc::clone(&log),
        },
        Output {
            fail_present: false,
            fail_restore: false,
            log: Arc::clone(&log),
        },
        RuntimeConfig::default(),
    );
    let mut running = Box::pin(runtime.run());
    std::future::poll_fn(|cx| {
        assert!(running.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    drop(running);
    assert!(handle.try_send(()).is_err());
    let events = log.lock().unwrap().clone();
    assert_eq!(
        events
            .iter()
            .filter(|event| **event == "drop-program")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| **event == "drop-presenter")
            .count(),
        1
    );
    // As before, dropping the future relinquishes owners; it cannot run async restoration.
    assert!(!events.contains(&"restore"));
}
