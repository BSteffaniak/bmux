//! Application-owned engine; plugin deployments wrap these methods with generated services.
mod engine;
pub mod loader;
mod mirror_component;
mod projection;
pub use mirror_component::MirrorComponent;
pub mod mirror;
pub mod protocol;
pub mod transport;

pub use engine::{Asset, Credential, Host, HostConfig, Limits, Owner, OwnerContext, OwnerUpdate};
pub use protocol::{
    Acknowledgment, Action, AssetInfo, Cursor, HostError, InteractionMode, PROTOCOL_VERSION, Poll,
    Request, Response, Scopes, Snapshot, Submission, SurfaceInput,
};
