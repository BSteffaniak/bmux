use crate::{Error, ErrorKind, Result};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone)]
/// Caller-owned typed profile patch and ordered parent identifiers; Debug is redacted.
pub struct Profile<T> {
    /// Parent names applied left-to-right before this profile value.
    pub parents: Vec<String>,
    /// Explicit trusted-access configuration contents.
    pub value: T,
}
impl<T> std::fmt::Debug for Profile<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Profile(<redacted>)")
    }
}
#[derive(Clone, Copy, Debug)]
/// Explicit iterative traversal bounds with no hidden recursion ceiling.
pub struct ProfileLimits {
    /// Maximum active profile chain length, counting the selected profile itself.
    pub max_depth: usize,
    /// Maximum evaluations: repeated visits in `resolve_profile`, but distinct
    /// cache misses in `resolve_profile_cached`. Cache hits do not consume this bound.
    pub max_expansions: usize,
}
impl Default for ProfileLimits {
    fn default() -> Self {
        Self {
            max_depth: 32,
            max_expansions: 1024,
        }
    }
}
/// Resolve ordered parent branches, then the child, without recursive traversal.
/// Repeated diamond ancestors are deliberately expanded once per branch.
pub fn resolve_profile<T: Clone>(
    name: &str,
    profiles: &BTreeMap<String, Profile<T>>,
    limits: ProfileLimits,
    merge: impl FnMut(&mut T, T) -> Result<()>,
) -> Result<T> {
    resolve_profile_impl(name, profiles, limits, merge, false)
}

/// Resolve profiles with per-call memoization of each fully resolved profile.
///
/// Opt in only when merge is deterministic and pure with respect to its arguments:
/// cached reuse deliberately omits callback executions from repeated parent subgraphs.
/// Recursive replacement consumers can pass `ConfigValue::overlay_replace` wrapped
/// in `Ok(())`. Parent ordering and repeated patch application remain unchanged;
/// this does not deduplicate parent edges or change the uncached resolver.
///
/// `max_expansions` bounds distinct profile evaluations, not cache hits. `max_depth`
/// bounds every inherited path, including cached subgraph heights. Traversal and
/// callback count are O(V + E) for reachable profiles/parent edges (ordered maps add
/// lookup factors); cloning and merge costs depend on caller-owned patch sizes.
/// Cache lifetime is one invocation; no state or process reads persist across calls.
///
/// # Errors
/// Returns UnknownProfile, ProfileCycle or Limit on graph validation failures,
/// or propagates a secret-safe error from the caller's merge callback.
pub fn resolve_profile_cached<T: Clone>(
    name: &str,
    profiles: &BTreeMap<String, Profile<T>>,
    limits: ProfileLimits,
    merge: impl FnMut(&mut T, T) -> Result<()>,
) -> Result<T> {
    resolve_profile_impl(name, profiles, limits, merge, true)
}

fn resolve_profile_impl<T: Clone>(
    name: &str,
    profiles: &BTreeMap<String, Profile<T>>,
    limits: ProfileLimits,
    mut merge: impl FnMut(&mut T, T) -> Result<()>,
    memoize: bool,
) -> Result<T> {
    struct Frame<'a, T> {
        name: &'a str,
        profile: &'a Profile<T>,
        next_parent: usize,
        result: Option<T>,
        height: usize,
    }
    if limits.max_depth == 0 || limits.max_expansions == 0 {
        return Err(Error::new(ErrorKind::Limit));
    }
    let profile = profiles
        .get(name)
        .ok_or(Error::new(ErrorKind::UnknownProfile))?;
    let mut stack = vec![Frame {
        name,
        profile,
        next_parent: 0,
        result: None,
        height: 1,
    }];
    let mut cache: BTreeMap<&str, (T, usize)> = BTreeMap::new();
    let mut active = BTreeSet::from([name]);
    let mut expansions = 1usize;
    while let Some(frame) = stack.last_mut() {
        if let Some(parent) = frame.profile.parents.get(frame.next_parent) {
            frame.next_parent += 1;
            if active.contains(parent.as_str()) {
                return Err(Error::new(ErrorKind::ProfileCycle));
            }
            if let Some((value, height)) = cache.get(parent.as_str()) {
                if stack
                    .len()
                    .checked_add(*height)
                    .is_none_or(|depth| depth > limits.max_depth)
                {
                    return Err(Error::new(ErrorKind::Limit));
                }
                let frame = stack
                    .last_mut()
                    .ok_or(Error::new(ErrorKind::InvalidPolicy))?;
                frame.height = frame.height.max(height.saturating_add(1));
                if let Some(result) = &mut frame.result {
                    merge(result, value.clone())?;
                } else {
                    frame.result = Some(value.clone());
                }
                continue;
            }
            expansions = expansions
                .checked_add(1)
                .ok_or(Error::new(ErrorKind::Limit))?;
            if stack.len() >= limits.max_depth || expansions > limits.max_expansions {
                return Err(Error::new(ErrorKind::Limit));
            }
            let profile = profiles
                .get(parent)
                .ok_or(Error::new(ErrorKind::UnknownProfile))?;
            active.insert(parent.as_str());
            stack.push(Frame {
                name: parent,
                profile,
                next_parent: 0,
                result: None,
                height: 1,
            });
        } else {
            // last_mut proved the stack nonempty; this branch handles the completed frame.
            let Some(frame) = stack.pop() else {
                return Err(Error::new(ErrorKind::InvalidPolicy));
            };
            active.remove(frame.name);
            let value = if let Some(mut result) = frame.result {
                merge(&mut result, frame.profile.value.clone())?;
                result
            } else {
                frame.profile.value.clone()
            };
            if memoize {
                cache.insert(frame.name, (value.clone(), frame.height));
            }
            if let Some(parent) = stack.last_mut() {
                parent.height = parent.height.max(frame.height.saturating_add(1));
                if let Some(result) = &mut parent.result {
                    merge(result, value)?;
                } else {
                    parent.result = Some(value);
                }
            } else {
                return Ok(value);
            }
        }
    }
    Err(Error::new(ErrorKind::InvalidPolicy))
}
