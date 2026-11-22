//! Activation-captured managed execution ownership; never route by receipt contents.
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// One activation's managed-target scope, captured from its launch markers.
#[derive(Default)]
pub(crate) struct ManagedScope(OnceLock<Result<Option<Scope>, String>>);

#[derive(Clone)]
pub(crate) struct Scope {
    pub(crate) target: String,
    pub(crate) identity: String,
}

pub(crate) fn activate(host: &crate::runtime::PaneRuntimeHost) {
    // A variable that is present but not Unicode is set, so it is not treated as absent.
    let read = |name| {
        switchy_env::var_os(name).map(|value| value.into_string().map_err(|_| ()))
    };
    let identity = read("BMUX_MANAGED_TARGET_IDENTITY");
    let target = read("BMUX_MANAGED_TARGET_ID");
    let result = match (identity, target) {
        (None, None) => Ok(None),
        (Some(Ok(identity)), Some(Ok(target))) => {
            validate(&identity, &target).map(|()| Some(Scope { target, identity }))
        }
        _ => Err("managed launch requires both target identity and target ID".into()),
    };
    let _ = host.managed_scope().0.set(result);
}

fn validate(identity: &str, target: &str) -> Result<(), String> {
    if identity.len() > 65_536 {
        return Err("managed target identity exceeds launch marker limit".into());
    }
    if target.len() != 64
        || !target
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("managed target ID must be a lowercase SHA-256 digest".into());
    }
    let value: serde_json::Value = serde_json::from_str(identity)
        .map_err(|error| format!("invalid managed target identity: {error}"))?;
    if !value.is_object() {
        return Err("managed target identity must be an object".into());
    }
    Ok(())
}

pub(crate) fn selected(host: &crate::runtime::PaneRuntimeHost) -> Result<Option<Scope>, String> {
    host.managed_scope().0.get().map_or(Ok(None), Clone::clone)
}

pub(crate) fn execution_root(
    host: &crate::runtime::PaneRuntimeHost,
    domain: &Path,
) -> Result<PathBuf, String> {
    selected(host).map(|scope| {
        scope.map_or_else(
            || domain.to_path_buf(),
            |scope| domain.join("targets").join(&scope.target),
        )
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn rejects_path_components_and_non_identity_markers() {
        assert!(super::validate("{}", &"a".repeat(64)).is_ok());
        for target in ["../other".to_owned(), "A".repeat(64), "a".repeat(63)] {
            assert!(super::validate("{}", &target).is_err());
        }
        assert!(super::validate("null", &"a".repeat(64)).is_err());
        assert!(super::validate("not-json", &"a".repeat(64)).is_err());
    }
}
