use crate::error::{MobileCoreError, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TargetInput {
    pub source: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetTransport {
    Local,
    Ssh,
    Tls,
    Iroh,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TargetUri {
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CanonicalTarget {
    pub uri: TargetUri,
    pub transport: TargetTransport,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TargetRecord {
    pub id: Uuid,
    pub name: String,
    pub canonical_target: TargetUri,
    pub transport: TargetTransport,
    pub default_session: Option<String>,
}

/// Normalize a user-provided target string to a canonical URI and transport.
///
/// # Errors
///
/// Returns [`MobileCoreError::InvalidTarget`] when `source` is empty.
pub fn canonicalize_target(input: &TargetInput) -> Result<CanonicalTarget> {
    let source = input.source.trim();
    if source.is_empty() {
        return Err(MobileCoreError::InvalidTarget(
            "target source cannot be empty".to_string(),
        ));
    }

    if source == "local" {
        return Ok(CanonicalTarget {
            uri: TargetUri {
                value: "local".to_string(),
            },
            transport: TargetTransport::Local,
        });
    }

    if source.starts_with("bmux://") || source.starts_with("https://") {
        return Ok(CanonicalTarget {
            uri: TargetUri {
                value: source.to_string(),
            },
            transport: TargetTransport::Tls,
        });
    }

    if source.starts_with("iroh://") {
        return Ok(CanonicalTarget {
            uri: TargetUri {
                value: source.to_string(),
            },
            transport: TargetTransport::Iroh,
        });
    }

    if source.starts_with("tls://") {
        return Ok(CanonicalTarget {
            uri: TargetUri {
                value: source.to_string(),
            },
            transport: TargetTransport::Tls,
        });
    }

    if source.starts_with("ssh://") || source.contains('@') {
        return Ok(CanonicalTarget {
            uri: TargetUri {
                value: source.to_string(),
            },
            transport: TargetTransport::Ssh,
        });
    }

    if source.contains(':') {
        return Ok(CanonicalTarget {
            uri: TargetUri {
                value: format!("tls://{source}"),
            },
            transport: TargetTransport::Tls,
        });
    }

    Ok(CanonicalTarget {
        uri: TargetUri {
            value: format!("bmux://{source}"),
        },
        transport: TargetTransport::Tls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_iroh_target() {
        let result = canonicalize_target(&TargetInput {
            source: "iroh://node-123?relay=https://relay.example".to_string(),
            display_name: None,
        })
        .expect("iroh target should parse");

        assert_eq!(result.transport, TargetTransport::Iroh);
        assert_eq!(
            result.uri.value,
            "iroh://node-123?relay=https://relay.example"
        );
    }

    #[test]
    fn canonicalize_plain_name_to_bmux_share() {
        let result = canonicalize_target(&TargetInput {
            source: "team-prod".to_string(),
            display_name: None,
        })
        .expect("plain name should normalize");

        assert_eq!(result.transport, TargetTransport::Tls);
        assert_eq!(result.uri.value, "bmux://team-prod");
    }

    #[test]
    fn canonicalize_host_port_to_tls() {
        let result = canonicalize_target(&TargetInput {
            source: "10.0.0.22:7443".to_string(),
            display_name: None,
        })
        .expect("host:port should normalize");

        assert_eq!(result.transport, TargetTransport::Tls);
        assert_eq!(result.uri.value, "tls://10.0.0.22:7443");
    }
}
