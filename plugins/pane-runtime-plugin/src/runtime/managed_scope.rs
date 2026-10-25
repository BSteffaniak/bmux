//! Activation-captured managed execution ownership; never route by receipt contents.
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Clone)]
pub(crate) struct Scope {
    pub(crate) target: String,
    pub(crate) identity: String,
}

static SCOPE: OnceLock<Result<Option<Scope>, String>> = OnceLock::new();

pub(crate) fn activate() {
    let identity = std::env::var("BMUX_MANAGED_TARGET_IDENTITY");
    let target = std::env::var("BMUX_MANAGED_TARGET_ID");
    let result = match (identity, target) {
        (Err(std::env::VarError::NotPresent), Err(std::env::VarError::NotPresent)) => Ok(None),
        (Ok(identity), Ok(target)) => {
            validate(&identity, &target).map(|()| Some(Scope { target, identity }))
        }
        _ => Err("managed launch requires both target identity and target ID".into()),
    };
    let _ = SCOPE.set(result);
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

pub(crate) fn selected() -> Result<Option<&'static Scope>, String> {
    SCOPE.get().map_or(Ok(None), |scope| {
        scope.as_ref().map(Option::as_ref).map_err(Clone::clone)
    })
}

pub(crate) fn execution_root(domain: &Path) -> Result<PathBuf, String> {
    selected().map(|scope| {
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
