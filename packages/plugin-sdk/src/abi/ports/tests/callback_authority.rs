use super::*;
use crate::abi::{NativeCallbackAuthorityV1, NativeInvocationPortsV3CallbackAuthorityV1};

unsafe extern "C" fn kernel(_: u64, _: *const u8, _: usize, _: *mut u64) -> AbiStatus {
    UNSUPPORTED
}
unsafe extern "C" fn available(_: u64, _: u16, _: *const u8, _: usize, _: *mut u64) -> AbiStatus {
    UNSUPPORTED
}
unsafe extern "C" fn open_v1(
    _: u64,
    _: u16,
    _: *mut crate::abi::retained::NativeRetainedHostPortV1,
) -> AbiStatus {
    panic!("must not downgrade")
}
unsafe extern "C" fn open_v2(
    _: u64,
    _: u16,
    _: *mut crate::abi::retained::NativeRetainedHostPortV2,
) -> AbiStatus {
    panic!("must not downgrade")
}
unsafe extern "C" fn reject(_: u64, _: u16, _: *mut NativeCallbackAuthorityV1) -> AbiStatus {
    BUSY
}
unsafe extern "C" fn bad(_: u64, _: u16, output: *mut NativeCallbackAuthorityV1) -> AbiStatus {
    unsafe {
        output.cast::<crate::abi::NativeDescriptorHeader>().write(
            crate::abi::NativeDescriptorHeader {
                size: 8,
                version: 99,
            },
        );
    }
    OK
}
fn extended(
    counts: &FixtureCounts,
    open: unsafe extern "C" fn(u64, u16, *mut NativeCallbackAuthorityV1) -> AbiStatus,
) -> NativeInvocationPortsV3CallbackAuthorityV1 {
    let mut base = table(counts);
    base.version = PORTS_VERSION_V3;
    base.size = u32::try_from(std::mem::size_of::<
        NativeInvocationPortsV3CallbackAuthorityV1,
    >())
    .unwrap();
    base.capabilities = crate::abi::CAP_CALLBACK_AUTHORITY_V1;
    NativeInvocationPortsV3CallbackAuthorityV1 {
        base: NativeInvocationPortsV3RetainedHostV2 {
            base: NativeInvocationPortsV3RetainedHost {
                base: NativeInvocationPortsV3Availability {
                    base: NativeInvocationPortsV3 {
                        base,
                        kernel_begin: kernel,
                    },
                    availability_begin: available,
                },
                retained_host_open: open_v1,
            },
            retained_host_open_v2: open_v2,
        },
        callback_authority_open: open,
    }
}
#[test]
fn callback_short_tail_rejects_without_reading_callback_or_entering_author() {
    let counts = FixtureCounts::default();
    let mut table = extended(&counts, reject);
    table.base.base.base.base.base.size =
        u32::try_from(std::mem::size_of::<NativeInvocationPortsV3RetainedHostV2>()).unwrap();
    assert!(matches!(
        unsafe { InvocationPorts::acquire((&raw const table).cast()) },
        Err(UNSUPPORTED)
    ));
}
#[test]
fn callback_selected_rejection_and_bad_header_never_downgrade() {
    let counts = FixtureCounts::default();
    let table = extended(&counts, reject);
    let ports = unsafe { InvocationPorts::acquire((&raw const table).cast()) }.unwrap();
    assert!(matches!(ports.open_callback_authority(), Err(BUSY)));
    let table = extended(&counts, bad);
    let ports = unsafe { InvocationPorts::acquire((&raw const table).cast()) }.unwrap();
    assert!(matches!(
        ports.open_callback_authority(),
        Err(INVALID_ARGUMENT)
    ));
}
