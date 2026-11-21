//! Behaviour of processes run on a real pseudo-terminal that only the operating system can show:
//! descriptors and signal handling inherited from the caller. Everything else is checked
//! against both the operating system and the simulator in `conformance.rs`.
#![cfg(all(unix, not(feature = "simulator")))]

use std::time::Duration;

use bmux_pty::{PtyCommand, PtyEvent, PtySize};

fn sh(script: &str) -> PtyCommand {
    PtyCommand::new("/bin/sh").args(["-c", script])
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn programs_start_with_default_signal_handling_and_no_stray_descriptors() {
    use std::os::fd::AsRawFd as _;
    // An inheritable descriptor in the caller, and an ignored interrupt.
    let (reader, writer) = std::io::pipe().unwrap();
    // SAFETY: fcntl and signal on owned descriptors and a valid signal.
    unsafe {
        let flags = libc::fcntl(writer.as_raw_fd(), libc::F_GETFD);
        libc::fcntl(writer.as_raw_fd(), libc::F_SETFD, flags & !libc::FD_CLOEXEC);
    }
    let leaked = writer.as_raw_fd();
    let previous = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
    let mut process = sh(&format!(
        "if [ -e /dev/fd/{leaked} ]; then echo leaked; else echo closed; fi; echo ready; while :; do sleep 0.05; done"
    ))
    .spawn(PtySize::new(80, 24))
    .unwrap();
    unsafe { libc::signal(libc::SIGINT, previous) };
    let mut output = Vec::new();
    while !text(&output).contains("ready") {
        if let Some(PtyEvent::Output(bytes)) = process.next_event_timeout(Duration::from_secs(10)) {
            output.extend(bytes);
        }
    }
    assert!(text(&output).contains("closed"), "{:?}", text(&output));
    // Interrupting works although the caller ignores interrupts.
    process.interrupt().unwrap();
    let exit = process
        .events()
        .find_map(|event| match event {
            PtyEvent::Exited(exit) => Some(exit),
            _ => None,
        })
        .unwrap();
    assert_eq!(exit.signal(), Some(libc::SIGINT));
    drop((reader, writer));
}
