//! Explicit native callback authority contract v1.
//!
//! Hosts and components negotiate the complete tail before authority projection.
//! All callbacks are bounded, thread-safe and panic-contained. No Rust runtime
//! values, closure layouts, wakers or serialized ancestry cross this boundary.

use super::{AbiStatus, NativeInvocationPortsV3RetainedHostV2};

pub type NativeCallbackAuthorityOpenV1 =
    unsafe extern "C" fn(u64, u16, *mut NativeCallbackAuthorityV1) -> AbiStatus;

pub const CAP_CALLBACK_AUTHORITY_V1: u64 = 1024;
pub const CALLBACK_AUTHORITY_VERSION: u16 = 1;
pub const MAX_AUTHORITY_CANCEL_SUBSCRIPTIONS: u32 = 32;

/// Notification only signals component-local wake state; never invokes authors.
/// The opaque userdata key is component-owned, never host authority.
pub type NativeCallbackCancelNotifyV1 = unsafe extern "C" fn(u64);

/// An independently retained actual admitted invocation authority.
///
/// Open transfers one reference on OK, none on failure. Version/size must match
/// this exact table before calling any operation. Enter mints a nonzero lease
/// for the host-bound actual instance and authenticated ancestry, not supplied
/// IDs. Leave releases that lease only after author execution has settled.
/// Failure/uncertainty never establishes successful cleanup or safe unloading.
///
/// Subscribe atomically registers or observes cancellation/closure and delivers
/// exactly one notification, possibly before subscribe returns. Subscription
/// output is nonzero on OK. The component installs local wake state first. The
/// host pins callback code, userdata and authority through fenced unsubscribe.
/// Unsubscribe closes notification admission: OK proves no current/future
/// callback; BUSY consumes nothing and permits retry of unsubscribe only.
/// Reentrant unsubscribe returns BUSY. No userdata/code may be freed on error.
/// Per-authority limit is 32 plus the actual owning instance's bounded budget.
///
/// Release cannot consume the final reference with outstanding entry leases,
/// subscriptions or callbacks. BUSY retains ownership; recovery pins survive
/// observer loss. No lock may span an external notification or author callback.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NativeCallbackAuthorityV1 {
    pub size: u32,
    pub version: u32,
    pub handle: u64,
    pub retain: unsafe extern "C" fn(u64) -> AbiStatus,
    pub release: unsafe extern "C" fn(u64) -> AbiStatus,
    pub check_live: unsafe extern "C" fn(u64) -> AbiStatus,
    pub enter: unsafe extern "C" fn(u64, *mut u64) -> AbiStatus,
    pub leave: unsafe extern "C" fn(u64, u64) -> AbiStatus,
    pub subscribe_cancel:
        unsafe extern "C" fn(u64, NativeCallbackCancelNotifyV1, u64, *mut u64) -> AbiStatus,
    pub unsubscribe_cancel: unsafe extern "C" fn(u64, u64) -> AbiStatus,
    pub bind_entry_ports: unsafe extern "C" fn(
        u64,
        u64,
        *mut NativeInvocationPortsV3CallbackAuthorityV1,
    ) -> AbiStatus,
}

/// Complete invocation tail for explicitly negotiated callback authority.
///
/// The version remains invocation-port v3; every earlier prefix is frozen.
/// Require bit 1024 and this complete allocation before reading the tail. Open
/// version 1 writes exactly the independent authority-v1 output allocation.
/// A selected open failure never triggers downgrade or another authority path.
/// `bind_entry_ports` writes exactly this allocation and transfers one retained
/// invocation-port reference on OK, none on failure. Its handle is host-minted
/// and bound to the actual entry lease, so nested services retain authenticated
/// ancestry without TLS. The original invocation ports must never substitute.
/// Bound ports use the existing scoped retain/release/drain semantics; lease
/// leave cannot retire authority while admitted descendants remain unsettled.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NativeInvocationPortsV3CallbackAuthorityV1 {
    pub base: NativeInvocationPortsV3RetainedHostV2,
    pub callback_authority_open:
        unsafe extern "C" fn(u64, u16, *mut NativeCallbackAuthorityV1) -> AbiStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_authority_layout_preserves_existing_prefix() {
        assert_eq!(
            std::mem::offset_of!(NativeInvocationPortsV3CallbackAuthorityV1, base),
            0
        );
        assert_eq!(
            std::mem::offset_of!(
                NativeInvocationPortsV3CallbackAuthorityV1,
                callback_authority_open
            ),
            std::mem::size_of::<NativeInvocationPortsV3RetainedHostV2>(),
        );
        if usize::BITS == 64 {
            assert_eq!(std::mem::size_of::<NativeCallbackAuthorityV1>(), 80);
            assert_eq!(
                std::mem::size_of::<NativeInvocationPortsV3CallbackAuthorityV1>(),
                120
            );
        }
        assert_eq!(
            CAP_CALLBACK_AUTHORITY_V1 & super::super::SCOPED_CAPABILITIES_V3_RECOVERY_V2,
            0
        );
    }
}
