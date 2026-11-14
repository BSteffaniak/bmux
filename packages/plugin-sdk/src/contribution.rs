use crate::{
    HostScope, PluginCommand, PluginEventPublication, PluginEventSubscription, PluginService,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Portable identity of a declared registration contract. Versions are exact;
/// adapters must negotiate rather than reinterpret an unsupported version.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RegistrationContract {
    pub kind: RegistrationKind,
    pub interface: String,
    pub version: u32,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationKind {
    Service,
    Event,
    RetainedState,
    Readiness,
    Extension,
    Stateful,
}

impl RegistrationContract {
    /// Reject unbounded or ambiguous identifiers before retaining a registration.
    ///
    /// # Errors
    /// Rejects zero versions and invalid or oversized identifiers.
    pub fn validate(&self) -> Result<(), String> {
        if self.version == 0 {
            return Err("registration contract version must be nonzero".into());
        }
        for value in [&self.interface, &self.name] {
            if value.is_empty()
                || value.len() > 256
                || value.trim() != value.as_str()
                || value.chars().any(char::is_control)
            {
                return Err("registration identifiers must be 1..=256 bytes without surrounding whitespace or control characters".into());
            }
        }
        Ok(())
    }
}

/// Generic plugin contribution declared either statically in a manifest or by a
/// plugin activation hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PluginContribution {
    Command {
        id: String,
        command: PluginCommand,
    },
    Service {
        id: String,
        service: PluginService,
    },
    EventSubscription {
        id: String,
        subscription: PluginEventSubscription,
    },
    EventPublication {
        id: String,
        publication: PluginEventPublication,
    },
    Capability {
        id: String,
        capability: HostScope,
    },
    Extension {
        id: String,
        extension_point: String,
        payload: Vec<u8>,
    },
}

impl PluginContribution {
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Command { id, .. }
            | Self::Service { id, .. }
            | Self::EventSubscription { id, .. }
            | Self::EventPublication { id, .. }
            | Self::Capability { id, .. }
            | Self::Extension { id, .. } => id,
        }
    }

    #[must_use]
    pub fn command(command: PluginCommand) -> Self {
        Self::Command {
            id: format!("command:{}", command.name),
            command,
        }
    }
}

/// Collector for activation-time contributions.
#[derive(Debug, Default)]
pub struct ContributionRegistrar {
    ids: BTreeSet<String>,
    contributions: Vec<PluginContribution>,
}

impl ContributionRegistrar {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a contribution.
    ///
    /// # Errors
    ///
    /// Returns an error when a contribution ID is duplicated.
    pub fn register(&mut self, contribution: PluginContribution) -> Result<(), String> {
        let id = contribution.id().to_string();
        if !self.ids.insert(id.clone()) {
            return Err(format!("duplicate plugin contribution id '{id}'"));
        }
        self.contributions.push(contribution);
        Ok(())
    }

    #[must_use]
    pub fn into_contributions(self) -> Vec<PluginContribution> {
        self.contributions
    }
}

#[cfg(test)]
mod tests {
    use super::{ContributionRegistrar, PluginContribution};
    use crate::{CommandExecutionKind, PluginCommand};

    #[test]
    fn registration_contract_requires_explicit_version_and_bounded_identifiers() {
        let mut contract = super::RegistrationContract {
            kind: super::RegistrationKind::Service,
            interface: "example/v1".into(),
            version: 1,
            name: "get".into(),
        };
        assert!(contract.validate().is_ok());
        contract.version = 0;
        assert!(contract.validate().is_err());
        contract.version = 1;
        contract.name = " trailing ".into();
        assert!(contract.validate().is_err());
        contract.name = "x".repeat(257);
        assert!(contract.validate().is_err());
    }

    #[test]
    fn manifest_command_maps_to_command_contribution() {
        let command = PluginCommand {
            name: "hello".to_string(),
            path: Vec::new(),
            aliases: Vec::new(),
            summary: "say hello".to_string(),
            description: None,
            arguments: Vec::new(),
            execution: CommandExecutionKind::ProviderExec,
            expose_in_cli: false,
            accepts_repeat: false,
        };
        let contribution = PluginContribution::command(command);
        assert_eq!(contribution.id(), "command:hello");
    }

    #[test]
    fn duplicate_contribution_ids_are_rejected() {
        let mut registrar = ContributionRegistrar::new();
        let command = PluginCommand::new("hello", "say hello");
        registrar
            .register(PluginContribution::command(command.clone()))
            .expect("first contribution should register");
        let error = registrar
            .register(PluginContribution::command(command))
            .expect_err("duplicate contribution should fail");
        assert!(error.contains("duplicate plugin contribution id"));
    }
}
