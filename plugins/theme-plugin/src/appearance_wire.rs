//! Exhaustive neutral-state to generated wire conversion at the service boundary.
use crate::control_contract::theme_state as wire;
use bmux_appearance as runtime;

macro_rules! record_conversion {
    ($name:ident, $source:ident { $($field:ident),* $(,)? }) => {
        impl From<runtime::$source> for wire::$name {
            fn from(value: runtime::$source) -> Self {
                let runtime::$source { $($field),* } = value;
                Self { $($field),* }
            }
        }
    };
}

record_conversion!(
    BorderAppearance,
    RuntimeBorderAppearance { active, inactive }
);
record_conversion!(
    BorderAppearancePatch,
    RuntimeBorderAppearancePatch { active, inactive }
);
record_conversion!(
    StatusAppearance,
    RuntimeStatusAppearance {
        background,
        foreground,
        active_window,
        mode_indicator,
    }
);
record_conversion!(
    StatusAppearancePatch,
    RuntimeStatusAppearancePatch {
        background,
        foreground,
        active_window,
        mode_indicator,
    }
);
record_conversion!(
    ContentBlend,
    RuntimeContentBlend {
        color,
        amount_permille
    }
);
record_conversion!(
    ContentBlendPatch,
    RuntimeContentBlendPatch {
        color,
        amount_permille
    }
);

impl From<runtime::RuntimeContentEffectScope> for wire::ContentEffectScope {
    fn from(value: runtime::RuntimeContentEffectScope) -> Self {
        match value {
            runtime::RuntimeContentEffectScope::Cells => Self::Cells,
        }
    }
}

impl From<runtime::RuntimeContentEffectBgPredicate> for wire::ContentEffectBgPredicate {
    fn from(value: runtime::RuntimeContentEffectBgPredicate) -> Self {
        match value {
            runtime::RuntimeContentEffectBgPredicate::Default => Self::Default,
        }
    }
}

impl From<runtime::RuntimeContentEffect> for wire::ContentEffect {
    fn from(value: runtime::RuntimeContentEffect) -> Self {
        let runtime::RuntimeContentEffect {
            enabled,
            scope,
            when_bg,
            background_blend,
        } = value;
        Self {
            enabled,
            scope: scope.into(),
            when_bg: when_bg.into(),
            background_blend: background_blend.map(Into::into),
        }
    }
}

impl From<runtime::RuntimeContentEffectPatch> for wire::ContentEffectPatch {
    fn from(value: runtime::RuntimeContentEffectPatch) -> Self {
        let runtime::RuntimeContentEffectPatch {
            enabled,
            scope,
            when_bg,
            background_blend,
        } = value;
        Self {
            enabled,
            scope: scope.map(Into::into),
            when_bg: when_bg.map(Into::into),
            background_blend: background_blend.into(),
        }
    }
}

impl From<runtime::RuntimeAppearancePatch> for wire::AppearancePatch {
    fn from(value: runtime::RuntimeAppearancePatch) -> Self {
        let runtime::RuntimeAppearancePatch {
            foreground,
            background,
            cursor,
            selection_background,
            border,
            status,
            content_effects,
        } = value;
        Self {
            foreground,
            background,
            cursor,
            selection_background,
            border: border.into(),
            status: status.into(),
            content_effects: content_effects
                .into_iter()
                .map(|(key, value)| (key, value.into()))
                .collect(),
        }
    }
}

impl From<runtime::RuntimeAppearance> for wire::Appearance {
    fn from(value: runtime::RuntimeAppearance) -> Self {
        let runtime::RuntimeAppearance {
            foreground,
            background,
            cursor,
            selection_background,
            border,
            status,
            content_effects,
            modes,
        } = value;
        Self {
            foreground,
            background,
            cursor,
            selection_background,
            border: border.into(),
            status: status.into(),
            content_effects: content_effects
                .into_iter()
                .map(|(key, value)| (key, value.into()))
                .collect(),
            modes: modes
                .into_iter()
                .map(|(key, value)| (key, value.into()))
                .collect(),
        }
    }
}
