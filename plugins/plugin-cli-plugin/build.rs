use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=plugin.toml");

    let manifest_path = PathBuf::from("plugin.toml");
    let manifest = fs::read_to_string(&manifest_path).expect("plugin.toml should be readable");
    let commands = parse_command_mappings(&manifest);
    let mut match_arms = String::new();
    for (name, path_segments) in commands {
        if path_segments.first().copied() == Some("plugin") {
            continue;
        }
        let path_literal = path_segments
            .iter()
            .map(|segment| format!("\"{segment}\""))
            .collect::<Vec<_>>()
            .join(", ");
        match_arms.push_str(&format!("        \"{name}\" => Some(&[{path_literal}]),\n"));
    }

    let generated = format!(
        "fn core_proxy_command_path(command_name: &str) -> Option<&'static [&'static str]> {{\n    match command_name {{\n{match_arms}        _ => None,\n    }}\n}}\n"
    );
    let out_dir = env::var_os("OUT_DIR").expect("OUT_DIR should be set");
    let out_file = PathBuf::from(out_dir).join("core_proxy_commands.rs");
    fs::write(out_file, generated).expect("generated proxy mapping should be written");
}

fn parse_command_mappings(manifest: &str) -> Vec<(String, Vec<&str>)> {
    let mut commands = Vec::new();
    let mut in_command = false;
    let mut current_name: Option<String> = None;
    let mut current_path: Option<Vec<&str>> = None;

    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed == "[[commands]]" {
            maybe_push_command(&mut commands, &mut current_name, &mut current_path);
            in_command = true;
            continue;
        }
        if trimmed.starts_with("[[commands.") {
            maybe_push_command(&mut commands, &mut current_name, &mut current_path);
            in_command = false;
            continue;
        }
        if !in_command {
            continue;
        }

        if let Some(value) = parse_quoted_value(trimmed, "name") {
            current_name = Some(value.to_string());
            continue;
        }
        if let Some(value) = parse_array_values(trimmed, "path") {
            current_path = Some(value);
        }
    }

    maybe_push_command(&mut commands, &mut current_name, &mut current_path);
    commands
}

fn maybe_push_command<'a>(
    commands: &mut Vec<(String, Vec<&'a str>)>,
    current_name: &mut Option<String>,
    current_path: &mut Option<Vec<&'a str>>,
) {
    if let (Some(name), Some(path)) = (current_name.take(), current_path.take()) {
        commands.push((name, path));
    }
}

fn parse_quoted_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let (left, right) = line.split_once('=')?;
    if left.trim() != key {
        return None;
    }
    let value = right.trim();
    if !value.starts_with('"') || !value.ends_with('"') || value.len() < 2 {
        return None;
    }
    Some(&value[1..value.len() - 1])
}

fn parse_array_values<'a>(line: &'a str, key: &str) -> Option<Vec<&'a str>> {
    let (left, right) = line.split_once('=')?;
    if left.trim() != key {
        return None;
    }
    let value = right.trim();
    if !value.starts_with('[') || !value.ends_with(']') {
        return None;
    }

    let inner = &value[1..value.len() - 1];
    let mut values = Vec::new();
    for part in inner.split(',') {
        let token = part.trim();
        if !token.starts_with('"') || !token.ends_with('"') || token.len() < 2 {
            return None;
        }
        values.push(&token[1..token.len() - 1]);
    }
    Some(values)
}
