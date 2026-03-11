use crate::{
    DEFAULT_NATIVE_ENTRY_SYMBOL, HostScope, NativeDescriptor, PluginCommand, PluginDependency,
    PluginEntrypoint, PluginEventSubscription, PluginFeature, PluginLifecycle,
    PluginManifestCompatibility, PluginService, Result,
};
use std::collections::BTreeSet;

pub struct PluginBuilder {
    descriptor: NativeDescriptor,
}

impl PluginBuilder {
    #[must_use]
    pub fn new(id: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            descriptor: NativeDescriptor {
                id: id.into(),
                display_name: display_name.into(),
                plugin_version: "0.1.0".to_string(),
                plugin_api: PluginManifestCompatibility {
                    minimum: crate::CURRENT_PLUGIN_API_VERSION.to_string(),
                    maximum: None,
                },
                native_abi: PluginManifestCompatibility {
                    minimum: crate::CURRENT_PLUGIN_ABI_VERSION.to_string(),
                    maximum: None,
                },
                description: None,
                homepage: None,
                provider_priority: 0,
                required_capabilities: BTreeSet::new(),
                provided_capabilities: BTreeSet::new(),
                provided_features: BTreeSet::new(),
                services: Vec::new(),
                commands: Vec::new(),
                event_subscriptions: Vec::new(),
                dependencies: Vec::new(),
                lifecycle: PluginLifecycle::default(),
            },
        }
    }

    #[must_use]
    pub fn plugin_version(mut self, version: impl Into<String>) -> Self {
        self.descriptor.plugin_version = version.into();
        self
    }

    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.descriptor.description = Some(description.into());
        self
    }

    #[must_use]
    pub fn homepage(mut self, homepage: impl Into<String>) -> Self {
        self.descriptor.homepage = Some(homepage.into());
        self
    }

    #[must_use]
    pub const fn provider_priority(mut self, priority: i32) -> Self {
        self.descriptor.provider_priority = priority;
        self
    }

    #[must_use]
    pub fn plugin_api(mut self, minimum: impl Into<String>, maximum: Option<String>) -> Self {
        self.descriptor.plugin_api = PluginManifestCompatibility {
            minimum: minimum.into(),
            maximum,
        };
        self
    }

    #[must_use]
    pub fn native_abi(mut self, minimum: impl Into<String>, maximum: Option<String>) -> Self {
        self.descriptor.native_abi = PluginManifestCompatibility {
            minimum: minimum.into(),
            maximum,
        };
        self
    }

    pub fn require_capability(mut self, capability: impl Into<String>) -> Result<Self> {
        self.descriptor
            .required_capabilities
            .insert(HostScope::new(capability)?);
        Ok(self)
    }

    pub fn provide_capability(mut self, capability: impl Into<String>) -> Result<Self> {
        self.descriptor
            .provided_capabilities
            .insert(HostScope::new(capability)?);
        Ok(self)
    }

    pub fn provide_feature(mut self, feature: impl Into<String>) -> Result<Self> {
        self.descriptor
            .provided_features
            .insert(PluginFeature::new(feature)?);
        Ok(self)
    }

    #[must_use]
    pub fn service(mut self, service: PluginService) -> Self {
        self.descriptor.services.push(service);
        self
    }

    #[must_use]
    pub fn command(mut self, command: PluginCommand) -> Self {
        self.descriptor.commands.push(command);
        self
    }

    #[must_use]
    pub fn event_subscription(mut self, event_subscription: PluginEventSubscription) -> Self {
        self.descriptor.event_subscriptions.push(event_subscription);
        self
    }

    #[must_use]
    pub fn dependency(mut self, dependency: PluginDependency) -> Self {
        self.descriptor.dependencies.push(dependency);
        self
    }

    #[must_use]
    pub fn lifecycle(mut self, lifecycle: PluginLifecycle) -> Self {
        self.descriptor.lifecycle = lifecycle;
        self
    }

    pub fn build(self) -> Result<NativeDescriptor> {
        let entrypoint = PluginEntrypoint::Native {
            symbol: DEFAULT_NATIVE_ENTRY_SYMBOL.to_string(),
        };
        self.descriptor.clone().into_declaration(entrypoint)?;
        Ok(self.descriptor)
    }
}
