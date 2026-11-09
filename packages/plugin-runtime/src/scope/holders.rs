//! Cheap, always-on attribution of everything that keeps a scope alive.
//!
//! Every admission, child scope and resource records the creating call site
//! (`#[track_caller]` + [`Location`], never a captured backtrace) and an
//! optional label. Bookkeeping lives inside the tree's single metadata lock, so
//! attribution adds no lock ordering of its own.
use std::{fmt, panic::Location, sync::Arc};

use crate::{
    LifecycleState, PluginActivationId, PluginInstanceId, PluginRuntimeScopeId, RuntimeInvocationId,
};

/// Creating call site of an admission, child scope or resource.
pub type HolderSite = &'static Location<'static>;

/// Identifies one scope node inside an instance tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScopeKey {
    Instance,
    Activation(PluginActivationId),
    Invocation(RuntimeInvocationId),
}

impl fmt::Display for ScopeKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Instance => formatter.write_str("instance"),
            Self::Activation(id) => write!(formatter, "activation {id}"),
            Self::Invocation(id) => write!(formatter, "invocation {id}"),
        }
    }
}

/// What a holder is retaining.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HolderKind {
    /// A live [`crate::AdmissionGuard`] (including a spawned task's admission).
    Admission,
    /// A child scope that has not been finished.
    ChildScope,
    /// A registered resource that has not been released.
    Resource,
    /// A background task handle that has not been reaped.
    Task,
    /// The scope's task collection has not been drained since its last spawn.
    UndrainedTasks,
}

impl fmt::Display for HolderKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Admission => "admission",
            Self::ChildScope => "unfinished child scope",
            Self::Resource => "unreleased resource",
            Self::Task => "background task",
            Self::UndrainedTasks => "undrained task collection",
        })
    }
}

/// One outstanding holder of a scope subtree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutstandingHolder {
    pub kind: HolderKind,
    /// The node that owns this holder.
    pub scope: ScopeKey,
    /// The owning node's label, if any.
    pub scope_label: Option<Arc<str>>,
    /// Where the holder was created, when known.
    pub site: Option<HolderSite>,
    /// Holder label: admission/operation label, resource type, or task name.
    pub label: Option<Arc<str>>,
}

impl fmt::Display for OutstandingHolder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.kind)?;
        if let Some(label) = &self.label {
            write!(formatter, " '{label}'")?;
        }
        if let Some(site) = self.site {
            write!(formatter, " at {site}")?;
        }
        write!(formatter, " in {}", self.scope)?;
        if let Some(label) = &self.scope_label {
            write!(formatter, " ({label})")?;
        }
        Ok(())
    }
}

/// Holders listed in a [`fmt::Display`] report before the remainder is counted.
const DISPLAYED_HOLDERS: usize = 32;

/// Outstanding holders of one scope subtree, attributed by owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutstandingHolders {
    pub runtime: PluginRuntimeScopeId,
    pub instance: PluginInstanceId,
    /// The instance's label (normally the plugin id), if any.
    pub instance_label: Option<Arc<str>>,
    /// The scope that was queried.
    pub scope: ScopeKey,
    /// Phase of the queried scope when the snapshot was taken.
    pub phase: LifecycleState,
    pub holders: Vec<OutstandingHolder>,
}

impl OutstandingHolders {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.holders.is_empty()
    }
}

impl fmt::Display for OutstandingHolders {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.instance_label {
            Some(label) => write!(formatter, "'{label}' ")?,
            None => formatter.write_str("unlabelled ")?,
        }
        write!(
            formatter,
            "instance {} {} ({:?}): ",
            self.instance, self.scope, self.phase
        )?;
        if self.holders.is_empty() {
            return formatter.write_str("no attributable holders");
        }
        write!(formatter, "{} outstanding holder(s): ", self.holders.len())?;
        for (index, holder) in self.holders.iter().take(DISPLAYED_HOLDERS).enumerate() {
            if index > 0 {
                formatter.write_str("; ")?;
            }
            write!(formatter, "{holder}")?;
        }
        if self.holders.len() > DISPLAYED_HOLDERS {
            write!(
                formatter,
                "; and {} more",
                self.holders.len() - DISPLAYED_HOLDERS
            )?;
        }
        Ok(())
    }
}
