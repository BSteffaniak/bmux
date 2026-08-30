use bmux_plugin_sdk::PluginCommandError;

use super::{
    ActiveAlignment, ColorSettings, Density, HintPolicy, OverflowStyle, Placement, Preset,
    SeparatorSet, Settings, TabOrder, TabScope,
};

#[allow(clippy::too_many_lines)] // Parsing mirrors the intentionally rich legacy-compatible settings surface.
pub fn parse_settings(value: Option<&toml::Value>) -> Result<Settings, PluginCommandError> {
    let mut settings = Settings::default();
    let Some(table) = value.and_then(toml::Value::as_table) else {
        return Ok(settings);
    };

    settings.placement = parse_enum(
        table,
        "placement",
        settings.placement,
        |value| match value {
            "top" => Some(Placement::Top),
            "bottom" => Some(Placement::Bottom),
            _ => None,
        },
    )?;
    settings.preset = parse_enum(table, "preset", settings.preset, |value| match value {
        "tab_rail" => Some(Preset::TabRail),
        "minimal" => Some(Preset::Minimal),
        "classic" => Some(Preset::Classic),
        _ => None,
    })?;
    settings.tab_scope = parse_enum(
        table,
        "tab_scope",
        settings.tab_scope,
        |value| match value {
            "all_contexts" => Some(TabScope::AllContexts),
            "session_contexts" => Some(TabScope::SessionContexts),
            "mru" => Some(TabScope::Mru),
            _ => None,
        },
    )?;
    settings.tab_order = parse_enum(
        table,
        "tab_order",
        settings.tab_order,
        |value| match value {
            "stable" => Some(TabOrder::Stable),
            "mru" => Some(TabOrder::Mru),
            _ => None,
        },
    )?;
    settings.hint_policy =
        parse_enum(
            table,
            "hint_policy",
            settings.hint_policy,
            |value| match value {
                "always" => Some(HintPolicy::Always),
                "scroll_only" => Some(HintPolicy::ScrollOnly),
                "never" => Some(HintPolicy::Never),
                _ => None,
            },
        )?;

    settings.height = parse_bounded_u16(table, "height", settings.height, 1, 4)?;
    settings.order = parse_i32(table, "order", settings.order)?;

    if let Some(template) = table
        .get("tab_template")
        .or_else(|| table.get("label_template"))
        .and_then(toml::Value::as_str)
    {
        settings.label_template = template.to_string();
    } else if table.get("show_tab_index").and_then(toml::Value::as_bool) == Some(true)
        || table.get("show_index").and_then(toml::Value::as_bool) == Some(true)
    {
        settings.label_template = "{index}:{name}".to_string();
    }
    settings.maximum_label_width = parse_bounded_u16_alias(
        table,
        "tab_label_max_width",
        "maximum_label_width",
        settings.maximum_label_width,
        1,
        u16::MAX,
    )?;
    settings.maximum_visible_tabs = parse_optional_count_alias(
        table,
        "max_tabs",
        "maximum_visible_tabs",
        settings.maximum_visible_tabs,
    )?;

    for (key, target) in [
        ("show_session_name", &mut settings.show_session_name),
        ("show_context_name", &mut settings.show_context_name),
        ("show_mode", &mut settings.show_mode),
        ("show_role", &mut settings.show_role),
        ("show_follow", &mut settings.show_follow),
        ("show_hint", &mut settings.show_hint),
        ("hover_highlight", &mut settings.hover_highlight),
        ("show_compact_facts", &mut settings.show_compact_facts),
    ] {
        if let Some(value) = table.get(key).and_then(toml::Value::as_bool) {
            *target = value;
        }
    }

    if let Some(layout) = table.get("layout").and_then(toml::Value::as_table) {
        settings.density = parse_enum(layout, "density", settings.density, |value| match value {
            "compact" => Some(Density::Compact),
            "cozy" => Some(Density::Cozy),
            _ => None,
        })?;
        settings.left_padding = parse_usize(layout, "left_padding", settings.left_padding)?;
        settings.right_padding = parse_usize(layout, "right_padding", settings.right_padding)?;
        settings.tab_gap = parse_usize(layout, "tab_gap", settings.tab_gap)?;
        settings.module_gap = parse_usize(layout, "module_gap", settings.module_gap)?;
        settings.overflow_style = parse_enum(
            layout,
            "overflow_style",
            settings.overflow_style,
            |value| match value {
                "count" => Some(OverflowStyle::Count),
                "arrows" => Some(OverflowStyle::Arrows),
                _ => None,
            },
        )?;
        settings.align_active = parse_enum(
            layout,
            "align_active",
            settings.align_active,
            |value| match value {
                "keep_visible" => Some(ActiveAlignment::KeepVisible),
                "focus_bias" => Some(ActiveAlignment::FocusBias),
                _ => None,
            },
        )?;
    }

    if let Some(style) = table.get("style").and_then(toml::Value::as_table) {
        settings.separator_set = parse_enum(
            style,
            "separator_set",
            settings.separator_set,
            |value| match value {
                "angled_segments" => Some(SeparatorSet::AngledSegments),
                "plain" => Some(SeparatorSet::Plain),
                "ascii" => Some(SeparatorSet::Ascii),
                _ => None,
            },
        )?;
        for (key, target) in [
            ("prefer_unicode", &mut settings.prefer_unicode),
            ("force_ascii", &mut settings.force_ascii),
            ("dim_inactive", &mut settings.dim_inactive),
            ("bold_active", &mut settings.bold_active),
            ("underline_active", &mut settings.underline_active),
        ] {
            if let Some(value) = style.get(key).and_then(toml::Value::as_bool) {
                *target = value;
            }
        }
    }

    if let Some(colors) = table.get("colors").and_then(toml::Value::as_table) {
        parse_colors(colors, &mut settings.colors)?;
    }

    Ok(settings)
}

fn parse_colors(
    table: &toml::map::Map<String, toml::Value>,
    colors: &mut ColorSettings,
) -> Result<(), PluginCommandError> {
    for (key, target) in [
        ("bar_bg", &mut colors.bar_bg),
        ("bar_fg", &mut colors.bar_fg),
        ("tab_active_bg", &mut colors.tab_active_bg),
        ("tab_active_fg", &mut colors.tab_active_fg),
        ("tab_inactive_bg", &mut colors.tab_inactive_bg),
        ("tab_inactive_fg", &mut colors.tab_inactive_fg),
        ("tab_hover_bg", &mut colors.tab_hover_bg),
        ("tab_hover_fg", &mut colors.tab_hover_fg),
        ("tab_active_hover_bg", &mut colors.tab_active_hover_bg),
        ("tab_active_hover_fg", &mut colors.tab_active_hover_fg),
        ("module_bg", &mut colors.module_bg),
        ("module_fg", &mut colors.module_fg),
        ("overflow_bg", &mut colors.overflow_bg),
        ("overflow_fg", &mut colors.overflow_fg),
    ] {
        let Some(value) = table.get(key) else {
            continue;
        };
        let text = value
            .as_str()
            .ok_or_else(|| invalid(key, "must be a string"))?;
        if !valid_hex_color(text) {
            return Err(invalid(key, "must be a #RRGGBB color"));
        }
        *target = Some(text.to_string());
    }
    Ok(())
}

fn parse_enum<T: Copy>(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
    default: T,
    parse: impl FnOnce(&str) -> Option<T>,
) -> Result<T, PluginCommandError> {
    let Some(value) = table.get(key) else {
        return Ok(default);
    };
    let text = value
        .as_str()
        .ok_or_else(|| invalid(key, "must be a string"))?;
    parse(text).ok_or_else(|| invalid(key, &format!("has unsupported value {text:?}")))
}

fn parse_i32(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
    default: i32,
) -> Result<i32, PluginCommandError> {
    let Some(value) = table.get(key) else {
        return Ok(default);
    };
    i32::try_from(
        value
            .as_integer()
            .ok_or_else(|| invalid(key, "must be an integer"))?,
    )
    .map_err(|_| invalid(key, "must fit in an i32"))
}

fn parse_usize(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
    default: usize,
) -> Result<usize, PluginCommandError> {
    let Some(value) = table.get(key) else {
        return Ok(default);
    };
    usize::try_from(
        value
            .as_integer()
            .ok_or_else(|| invalid(key, "must be a non-negative integer"))?,
    )
    .map_err(|_| invalid(key, "must be a non-negative integer"))
}

fn parse_bounded_u16(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
    default: u16,
    minimum: u16,
    maximum: u16,
) -> Result<u16, PluginCommandError> {
    let Some(value) = table.get(key) else {
        return Ok(default);
    };
    u16::try_from(
        value
            .as_integer()
            .ok_or_else(|| invalid(key, "must be an integer"))?,
    )
    .ok()
    .filter(|value| (*value >= minimum) && (*value <= maximum))
    .ok_or_else(|| invalid(key, &format!("must be between {minimum} and {maximum}")))
}

fn parse_bounded_u16_alias(
    table: &toml::map::Map<String, toml::Value>,
    canonical: &str,
    alias: &str,
    default: u16,
    minimum: u16,
    maximum: u16,
) -> Result<u16, PluginCommandError> {
    if table.contains_key(canonical) {
        parse_bounded_u16(table, canonical, default, minimum, maximum)
    } else {
        parse_bounded_u16(table, alias, default, minimum, maximum)
    }
}

fn parse_optional_count_alias(
    table: &toml::map::Map<String, toml::Value>,
    canonical: &str,
    alias: &str,
    default: Option<usize>,
) -> Result<Option<usize>, PluginCommandError> {
    let entry = table
        .get(canonical)
        .map(|value| (canonical, value))
        .or_else(|| table.get(alias).map(|value| (alias, value)));
    let Some((key, value)) = entry else {
        return Ok(default);
    };
    let count = usize::try_from(
        value
            .as_integer()
            .ok_or_else(|| invalid(key, "must be a positive integer"))?,
    )
    .ok()
    .filter(|count| (1..=1_024).contains(count))
    .ok_or_else(|| invalid(key, "must be between 1 and 1024"))?;
    Ok(Some(count))
}

fn valid_hex_color(value: &str) -> bool {
    let hex = value.strip_prefix('#').unwrap_or(value);
    hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn invalid(key: &str, reason: &str) -> PluginCommandError {
    PluginCommandError::invalid_arguments(format!("bmux.tab_strip {key} {reason}"))
}
