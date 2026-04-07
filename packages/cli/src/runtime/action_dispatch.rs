//! Action dispatch channel.
//!
//! This module re-exports the action dispatch system from
//! [`bmux_plugin::action_dispatch`], which provides the process-global
//! host channel.

pub use bmux_plugin::action_dispatch::{
    ActionDispatchError, ActionDispatchRequest, dispatch, register_host,
};
