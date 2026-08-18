use bmux_attach_layout_protocol::AttachRect;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HorizontalAlignment {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VerticalAlignment {
    #[default]
    Top,
    Center,
    Bottom,
}

impl HorizontalAlignment {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Center => "center",
            Self::Right => "right",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "left" => Ok(Self::Left),
            "center" => Ok(Self::Center),
            "right" => Ok(Self::Right),
            _ => Err(format!("invalid horizontal alignment '{value}'")),
        }
    }
}

impl VerticalAlignment {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Center => "center",
            Self::Bottom => "bottom",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "top" => Ok(Self::Top),
            "center" => Ok(Self::Center),
            "bottom" => Ok(Self::Bottom),
            _ => Err(format!("invalid vertical alignment '{value}'")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
enum ContentLimit {
    Cells(u16),
    Named(ContentLimitName),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ContentLimitName {
    None,
}

impl ContentLimit {
    const fn value(self) -> Option<u16> {
        match self {
            Self::Cells(cells) => Some(cells),
            Self::Named(ContentLimitName::None) => None,
        }
    }

    fn validate(self, field: &str) -> Result<(), String> {
        if matches!(self, Self::Cells(0)) {
            return Err(format!("{field} must be positive or 'none'"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct PanePaddingSpec {
    pub left: u16,
    pub right: u16,
    pub top: u16,
    pub bottom: u16,
    pub max_content_width: Option<u16>,
    pub max_content_height: Option<u16>,
    pub horizontal_alignment: HorizontalAlignment,
    pub vertical_alignment: VerticalAlignment,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PaddingDefaults {
    left: u16,
    right: u16,
    top: u16,
    bottom: u16,
    max_content_width: Option<ContentLimit>,
    max_content_height: Option<ContentLimit>,
    horizontal_alignment: HorizontalAlignment,
    vertical_alignment: VerticalAlignment,
    persist_runtime_overrides: Option<bool>,
    pane_rules: Vec<PanePaddingRule>,
}

impl PaddingDefaults {
    fn validate(&self) -> Result<(), String> {
        if let Some(limit) = self.max_content_width {
            limit.validate("padding.max_content_width")?;
        }
        if let Some(limit) = self.max_content_height {
            limit.validate("padding.max_content_height")?;
        }
        for (index, rule) in self.pane_rules.iter().enumerate() {
            rule.validate(index)?;
        }
        Ok(())
    }

    fn resolved_spec(&self) -> PanePaddingSpec {
        PanePaddingSpec {
            left: self.left,
            right: self.right,
            top: self.top,
            bottom: self.bottom,
            max_content_width: self.max_content_width.and_then(ContentLimit::value),
            max_content_height: self.max_content_height.and_then(ContentLimit::value),
            horizontal_alignment: self.horizontal_alignment,
            vertical_alignment: self.vertical_alignment,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PanePaddingRule {
    match_command: Option<String>,
    match_name: Option<String>,
    match_shell: Option<String>,
    min_width: Option<u16>,
    max_width: Option<u16>,
    min_height: Option<u16>,
    max_height: Option<u16>,
    left: Option<u16>,
    right: Option<u16>,
    top: Option<u16>,
    bottom: Option<u16>,
    max_content_width: Option<ContentLimit>,
    max_content_height: Option<ContentLimit>,
    horizontal_alignment: Option<HorizontalAlignment>,
    vertical_alignment: Option<VerticalAlignment>,
}

impl PanePaddingRule {
    fn validate(&self, index: usize) -> Result<(), String> {
        if self
            .min_width
            .zip(self.max_width)
            .is_some_and(|(min, max)| min > max)
        {
            return Err(format!(
                "padding.pane_rules[{index}].min_width must not exceed max_width"
            ));
        }
        if self
            .min_height
            .zip(self.max_height)
            .is_some_and(|(min, max)| min > max)
        {
            return Err(format!(
                "padding.pane_rules[{index}].min_height must not exceed max_height"
            ));
        }
        if let Some(limit) = self.max_content_width {
            limit.validate(&format!("padding.pane_rules[{index}].max_content_width"))?;
        }
        if let Some(limit) = self.max_content_height {
            limit.validate(&format!("padding.pane_rules[{index}].max_content_height"))?;
        }
        Ok(())
    }

    fn matches(&self, metadata: PanePaddingMetadata<'_>, base: AttachRect) -> bool {
        self.match_command.as_ref().is_none_or(|pattern| {
            metadata
                .active_command
                .is_some_and(|value| wildcard_matches(pattern, value))
        }) && self.match_name.as_ref().is_none_or(|pattern| {
            metadata
                .name
                .is_some_and(|value| wildcard_matches(pattern, value))
        }) && self
            .match_shell
            .as_ref()
            .is_none_or(|pattern| wildcard_matches(pattern, metadata.shell))
            && self.min_width.is_none_or(|minimum| base.w >= minimum)
            && self.max_width.is_none_or(|maximum| base.w <= maximum)
            && self.min_height.is_none_or(|minimum| base.h >= minimum)
            && self.max_height.is_none_or(|maximum| base.h <= maximum)
    }

    fn apply_to(&self, mut spec: PanePaddingSpec) -> PanePaddingSpec {
        spec.left = self.left.unwrap_or(spec.left);
        spec.right = self.right.unwrap_or(spec.right);
        spec.top = self.top.unwrap_or(spec.top);
        spec.bottom = self.bottom.unwrap_or(spec.bottom);
        if let Some(limit) = self.max_content_width {
            spec.max_content_width = limit.value();
        }
        if let Some(limit) = self.max_content_height {
            spec.max_content_height = limit.value();
        }
        spec.horizontal_alignment = self
            .horizontal_alignment
            .unwrap_or(spec.horizontal_alignment);
        spec.vertical_alignment = self.vertical_alignment.unwrap_or(spec.vertical_alignment);
        spec
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PaneRuntimeSettings {
    padding: PaddingDefaults,
}

#[derive(Debug, Clone)]
pub(crate) struct PanePaddingConfig {
    defaults: PanePaddingSpec,
    rules: Vec<PanePaddingRule>,
    pub persist_runtime_overrides: bool,
}

impl Default for PanePaddingConfig {
    fn default() -> Self {
        Self {
            defaults: PanePaddingSpec::default(),
            rules: Vec::new(),
            persist_runtime_overrides: true,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PanePaddingMetadata<'a> {
    pub name: Option<&'a str>,
    pub shell: &'a str,
    pub active_command: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolvedPanePadding {
    pub spec: PanePaddingSpec,
    pub matched_rule_index: Option<usize>,
}

impl PanePaddingConfig {
    pub fn parse(settings: Option<&toml::Value>) -> Result<Self, String> {
        let Some(settings) = settings else {
            return Ok(Self::default());
        };
        let parsed = settings
            .clone()
            .try_into::<PaneRuntimeSettings>()
            .map_err(|error| format!("failed to parse bmux.pane_runtime settings: {error}"))?;
        parsed.padding.validate()?;
        Ok(Self {
            defaults: parsed.padding.resolved_spec(),
            rules: parsed.padding.pane_rules,
            persist_runtime_overrides: parsed.padding.persist_runtime_overrides.unwrap_or(true),
        })
    }

    pub fn resolve(
        &self,
        metadata: PanePaddingMetadata<'_>,
        base: AttachRect,
        runtime_override: Option<PanePaddingSpec>,
    ) -> ResolvedPanePadding {
        if let Some(spec) = runtime_override {
            return ResolvedPanePadding {
                spec,
                matched_rule_index: None,
            };
        }
        let matched = self
            .rules
            .iter()
            .enumerate()
            .find(|(_, rule)| rule.matches(metadata, base));
        ResolvedPanePadding {
            spec: matched.map_or(self.defaults, |(_, rule)| rule.apply_to(self.defaults)),
            matched_rule_index: matched.map(|(index, _)| index),
        }
    }
}

/// Apply bmux's standard pane-decoration inset before user-configured padding.
pub(crate) const fn base_content_rect(rect: AttachRect) -> AttachRect {
    if rect.w < 2 || rect.h < 2 {
        return rect;
    }
    AttachRect {
        x: rect.x + 1,
        y: rect.y + 1,
        w: rect.w - 2,
        h: rect.h - 2,
    }
}

pub(crate) fn validate_spec(spec: PanePaddingSpec) -> Result<(), String> {
    if spec.max_content_width == Some(0) {
        return Err("max_content_width must be positive or absent".to_string());
    }
    if spec.max_content_height == Some(0) {
        return Err("max_content_height must be positive or absent".to_string());
    }
    Ok(())
}

pub(crate) fn padded_content_rect(base: AttachRect, spec: PanePaddingSpec) -> AttachRect {
    let left = spec.left.min(base.w.saturating_sub(1));
    let after_left = base.w.saturating_sub(left);
    let right = spec.right.min(after_left.saturating_sub(1));
    let top = spec.top.min(base.h.saturating_sub(1));
    let after_top = base.h.saturating_sub(top);
    let bottom = spec.bottom.min(after_top.saturating_sub(1));

    let available_w = after_left.saturating_sub(right).max(1);
    let available_h = after_top.saturating_sub(bottom).max(1);
    let content_w = spec
        .max_content_width
        .map_or(available_w, |maximum| available_w.min(maximum.max(1)));
    let content_h = spec
        .max_content_height
        .map_or(available_h, |maximum| available_h.min(maximum.max(1)));
    let surplus_w = available_w.saturating_sub(content_w);
    let surplus_h = available_h.saturating_sub(content_h);
    let align_x = match spec.horizontal_alignment {
        HorizontalAlignment::Left => 0,
        HorizontalAlignment::Center => surplus_w / 2,
        HorizontalAlignment::Right => surplus_w,
    };
    let align_y = match spec.vertical_alignment {
        VerticalAlignment::Top => 0,
        VerticalAlignment::Center => surplus_h / 2,
        VerticalAlignment::Bottom => surplus_h,
    };

    AttachRect {
        x: base.x.saturating_add(left).saturating_add(align_x),
        y: base.y.saturating_add(top).saturating_add(align_y),
        w: content_w,
        h: content_h,
    }
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let mut previous = vec![false; value.len() + 1];
    previous[0] = true;
    for token in pattern {
        let mut current = vec![false; value.len() + 1];
        if *token == b'*' {
            current[0] = previous[0];
        }
        for index in 1..=value.len() {
            current[index] = match *token {
                b'*' => previous[index] || current[index - 1],
                b'?' => previous[index - 1],
                literal => previous[index - 1] && literal == value[index - 1],
            };
        }
        previous = current;
    }
    previous[value.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: u16, h: u16) -> AttachRect {
        AttachRect { x: 10, y: 5, w, h }
    }

    fn metadata<'a>(
        name: Option<&'a str>,
        shell: &'a str,
        command: Option<&'a str>,
    ) -> PanePaddingMetadata<'a> {
        PanePaddingMetadata {
            name,
            shell,
            active_command: command,
        }
    }

    fn parse(value: toml::Table) -> Result<PanePaddingConfig, String> {
        PanePaddingConfig::parse(Some(&toml::Value::Table(value)))
    }

    #[test]
    fn missing_settings_preserve_current_geometry() {
        let config = PanePaddingConfig::parse(None).unwrap();
        let outer = rect(80, 24);
        let base = base_content_rect(outer);
        let resolved = config.resolve(metadata(None, "/bin/sh", None), base, None);
        assert_eq!(padded_content_rect(base, resolved.spec), base);
        assert!(config.persist_runtime_overrides);
    }

    #[test]
    fn parses_global_padding_and_centered_maximum() {
        let config = parse(toml::toml! {
            [padding]
            left = 2
            right = 3
            max_content_width = 40
            horizontal_alignment = "center"
            persist_runtime_overrides = false
        })
        .unwrap();
        let base = rect(100, 20);
        let resolved = config.resolve(metadata(None, "/bin/sh", None), base, None);
        assert_eq!(resolved.spec.left, 2);
        assert_eq!(resolved.spec.right, 3);
        assert_eq!(resolved.spec.max_content_width, Some(40));
        assert_eq!(
            resolved.spec.horizontal_alignment,
            HorizontalAlignment::Center
        );
        assert!(!config.persist_runtime_overrides);
        assert_eq!(
            padded_content_rect(base, resolved.spec),
            rect_at(39, 5, 40, 20)
        );
    }

    #[test]
    fn first_matching_rule_uses_and_semantics_and_can_clear_limit() {
        let config = parse(toml::toml! {
            [padding]
            max_content_width = 100

            [[padding.pane_rules]]
            match_command = "journal*"
            match_name = "other*"
            max_content_width = 60

            [[padding.pane_rules]]
            match_command = "journal*"
            match_name = "logs*"
            match_shell = "*/fish"
            max_content_width = "none"
            left = 4

            [[padding.pane_rules]]
            left = 9
        })
        .unwrap();
        let resolved = config.resolve(
            metadata(Some("logs-main"), "/opt/fish", Some("journalctl -f")),
            rect(180, 30),
            None,
        );
        assert_eq!(resolved.matched_rule_index, Some(1));
        assert_eq!(resolved.spec.max_content_width, None);
        assert_eq!(resolved.spec.left, 4);
    }

    #[test]
    fn geometry_rules_use_inclusive_base_dimensions() {
        let config = parse(toml::toml! {
            [padding]
            [[padding.pane_rules]]
            min_width = 100
            max_width = 120
            min_height = 20
            max_height = 30
            left = 7
        })
        .unwrap();
        for base in [rect(100, 20), rect(120, 30)] {
            assert_eq!(
                config
                    .resolve(metadata(None, "sh", None), base, None)
                    .spec
                    .left,
                7
            );
        }
        assert_eq!(
            config
                .resolve(metadata(None, "sh", None), rect(99, 20), None)
                .spec
                .left,
            0
        );
    }

    #[test]
    fn runtime_override_has_highest_precedence() {
        let config = parse(toml::toml! {
            [padding]
            left = 2
            [[padding.pane_rules]]
            left = 4
        })
        .unwrap();
        let override_spec = PanePaddingSpec {
            right: 11,
            ..PanePaddingSpec::default()
        };
        let resolved = config.resolve(
            metadata(None, "sh", None),
            rect(80, 24),
            Some(override_spec),
        );
        assert_eq!(resolved.spec, override_spec);
        assert_eq!(resolved.matched_rule_index, None);
    }

    #[test]
    fn rejects_unknown_fields_zero_limits_and_invalid_bounds() {
        assert!(parse(toml::toml! { [padding] mystery = 1 }).is_err());
        assert!(parse(toml::toml! { [padding] max_content_width = 0 }).is_err());
        assert!(
            parse(toml::toml! {
                [padding]
                [[padding.pane_rules]]
                min_width = 20
                max_width = 10
            })
            .is_err()
        );
    }

    #[test]
    fn wildcard_matching_supports_star_and_question_mark() {
        assert!(wildcard_matches("journal*", "journalctl -f"));
        assert!(wildcard_matches("logs-?", "logs-a"));
        assert!(!wildcard_matches("logs-?", "logs-main"));
    }

    #[test]
    fn applies_alignment_and_odd_surplus_deterministically() {
        let spec = PanePaddingSpec {
            max_content_width: Some(120),
            horizontal_alignment: HorizontalAlignment::Center,
            ..PanePaddingSpec::default()
        };
        assert_eq!(
            padded_content_rect(rect(161, 20), spec),
            rect_at(30, 5, 120, 20)
        );
        let right = PanePaddingSpec {
            horizontal_alignment: HorizontalAlignment::Right,
            ..spec
        };
        assert_eq!(
            padded_content_rect(rect(161, 20), right),
            rect_at(51, 5, 120, 20)
        );
    }

    #[test]
    fn geometry_supports_each_edge_axis_alignment_and_combination() {
        let base = rect_at(10, 5, 21, 15);
        for (spec, expected) in [
            (
                PanePaddingSpec {
                    left: 2,
                    ..PanePaddingSpec::default()
                },
                rect_at(12, 5, 19, 15),
            ),
            (
                PanePaddingSpec {
                    right: 3,
                    ..PanePaddingSpec::default()
                },
                rect_at(10, 5, 18, 15),
            ),
            (
                PanePaddingSpec {
                    top: 2,
                    ..PanePaddingSpec::default()
                },
                rect_at(10, 7, 21, 13),
            ),
            (
                PanePaddingSpec {
                    bottom: 4,
                    ..PanePaddingSpec::default()
                },
                rect_at(10, 5, 21, 11),
            ),
            (
                PanePaddingSpec {
                    left: 2,
                    right: 2,
                    ..PanePaddingSpec::default()
                },
                rect_at(12, 5, 17, 15),
            ),
            (
                PanePaddingSpec {
                    top: 2,
                    bottom: 2,
                    ..PanePaddingSpec::default()
                },
                rect_at(10, 7, 21, 11),
            ),
            (
                PanePaddingSpec {
                    max_content_width: Some(9),
                    ..PanePaddingSpec::default()
                },
                rect_at(10, 5, 9, 15),
            ),
            (
                PanePaddingSpec {
                    max_content_width: Some(9),
                    horizontal_alignment: HorizontalAlignment::Right,
                    ..PanePaddingSpec::default()
                },
                rect_at(22, 5, 9, 15),
            ),
            (
                PanePaddingSpec {
                    max_content_height: Some(7),
                    ..PanePaddingSpec::default()
                },
                rect_at(10, 5, 21, 7),
            ),
            (
                PanePaddingSpec {
                    max_content_height: Some(7),
                    vertical_alignment: VerticalAlignment::Center,
                    ..PanePaddingSpec::default()
                },
                rect_at(10, 9, 21, 7),
            ),
            (
                PanePaddingSpec {
                    max_content_height: Some(7),
                    vertical_alignment: VerticalAlignment::Bottom,
                    ..PanePaddingSpec::default()
                },
                rect_at(10, 13, 21, 7),
            ),
            (
                PanePaddingSpec {
                    left: 1,
                    right: 2,
                    top: 3,
                    bottom: 1,
                    max_content_width: Some(9),
                    max_content_height: Some(5),
                    horizontal_alignment: HorizontalAlignment::Center,
                    vertical_alignment: VerticalAlignment::Center,
                },
                rect_at(15, 11, 9, 5),
            ),
        ] {
            assert_eq!(padded_content_rect(base, spec), expected);
        }
    }

    #[test]
    fn metadata_matchers_require_values_and_matcher_free_rule_is_catch_all() {
        let config = parse(toml::toml! {
            [padding]
            [[padding.pane_rules]]
            match_name = "logs*"
            left = 3
            [[padding.pane_rules]]
            right = 4
        })
        .unwrap();
        let resolved = config.resolve(metadata(None, "sh", None), rect(80, 24), None);
        assert_eq!(resolved.matched_rule_index, Some(1));
        assert_eq!(resolved.spec.left, 0);
        assert_eq!(resolved.spec.right, 4);
    }

    #[test]
    fn clamps_excessive_edges_to_one_cell_with_leading_priority() {
        let spec = PanePaddingSpec {
            left: 50,
            right: 50,
            top: 50,
            bottom: 50,
            ..PanePaddingSpec::default()
        };
        assert_eq!(padded_content_rect(rect(10, 4), spec), rect_at(19, 8, 1, 1));
    }

    #[test]
    fn base_inset_preserves_tiny_rectangles() {
        assert_eq!(base_content_rect(rect(1, 1)), rect(1, 1));
        assert_eq!(base_content_rect(rect(2, 2)), rect_at(11, 6, 0, 0));
    }

    const fn rect_at(x: u16, y: u16, w: u16, h: u16) -> AttachRect {
        AttachRect { x, y, w, h }
    }
}
