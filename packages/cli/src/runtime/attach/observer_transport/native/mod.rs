//! Raw output rendezvous selection; framing and attachment policy stay shared.
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(unix)]
pub(super) use unix::connect;
#[cfg(windows)]
pub(super) use windows::connect;

#[cfg(test)]
mod boundary_tests {
    #[test]
    fn native_output_connectors_remain_product_neutral() {
        for source in [include_str!("unix.rs"), include_str!("windows.rs")] {
            for forbidden in [
                "bmux_broadcast",
                "broadcast_control",
                "broadcast_process",
                "pane_runtime",
                "InvokeService",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "native transport leaked {forbidden}"
                );
            }
        }
    }

    #[test]
    fn stream_negotiation_and_deadlines_remain_shared() {
        let shared = include_str!("../../observer_transport.rs");
        assert!(shared.contains("timeout(IO_DEADLINE, native::connect(socket_path))"));
        assert!(!shared.contains("tokio::net::UnixStream"));
        assert!(!shared.contains("named_pipe::"));
        assert!(!shared.contains("#[cfg(unix)]"));
        assert!(!shared.contains("#[cfg(windows)]"));
    }
}
