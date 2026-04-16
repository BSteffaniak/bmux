//! `bmux slot ...` subcommand handlers.

use anyhow::Result;
use bmux_cli_schema::SlotOutputFormat;
use bmux_slots::{Slot, SlotManifest, validate_slot_name};

use std::io::Write;

/// Print all declared slots.
pub(super) fn run_slot_list(format: SlotOutputFormat) -> Result<u8> {
    let manifest = load_manifest()?;
    let mut stdout = std::io::stdout().lock();
    match format {
        SlotOutputFormat::Toml => emit_toml_list(&mut stdout, &manifest)?,
        SlotOutputFormat::Json => emit_json_list(&mut stdout, &manifest)?,
        SlotOutputFormat::Nix => emit_nix_list(&mut stdout, &manifest)?,
    }
    Ok(0)
}

/// Show a single slot's detail.
pub(super) fn run_slot_show(name: Option<&str>, format: SlotOutputFormat) -> Result<u8> {
    let manifest = load_manifest()?;
    let slot = resolve_slot_name(&manifest, name)?;
    let mut stdout = std::io::stdout().lock();
    match format {
        SlotOutputFormat::Toml => {
            writeln!(stdout, "[slots.{}]", slot.name)?;
            emit_slot_toml_body(&mut stdout, slot)?;
        }
        SlotOutputFormat::Json => {
            let value = slot_to_json(slot);
            serde_json::to_writer_pretty(&mut stdout, &value)?;
            writeln!(stdout)?;
        }
        SlotOutputFormat::Nix => {
            emit_slot_nix(&mut stdout, slot)?;
        }
    }
    Ok(0)
}

/// Print resolved paths for the given slot.
pub(super) fn run_slot_paths(name: Option<&str>) -> Result<u8> {
    let manifest = load_manifest()?;
    let slot = resolve_slot_name(&manifest, name)?;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "slot         = {}", slot.name)?;
    writeln!(stdout, "binary       = {}", slot.binary.display())?;
    writeln!(stdout, "config_dir   = {}", slot.config_dir.display())?;
    writeln!(stdout, "runtime_dir  = {}", slot.runtime_dir.display())?;
    writeln!(stdout, "data_dir     = {}", slot.data_dir.display())?;
    writeln!(stdout, "state_dir    = {}", slot.state_dir.display())?;
    writeln!(stdout, "log_dir      = {}", slot.log_dir.display())?;
    writeln!(stdout, "inherit_base = {}", slot.inherit_base)?;
    Ok(0)
}

/// Validate the manifest.
pub(super) fn run_slot_doctor() -> Result<u8> {
    let manifest = match SlotManifest::load_default() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("doctor: failed to load manifest: {e}");
            return Ok(1);
        }
    };
    let mut any_fail = false;
    let mut stdout = std::io::stdout().lock();
    if manifest.slots.is_empty() {
        writeln!(
            stdout,
            "doctor: no slots declared (legacy single-install mode)"
        )?;
        return Ok(0);
    }
    writeln!(stdout, "doctor: {} slot(s) declared", manifest.slots.len())?;
    for (name, slot) in &manifest.slots {
        writeln!(stdout, "  slot {name}:")?;
        if validate_slot_name(name).is_err() {
            writeln!(stdout, "    ✗ invalid name")?;
            any_fail = true;
        } else {
            writeln!(stdout, "    ✓ name valid")?;
        }
        if slot.binary.exists() {
            writeln!(stdout, "    ✓ binary present")?;
        } else {
            writeln!(
                stdout,
                "    ✗ binary {} does not exist",
                slot.binary.display()
            )?;
            any_fail = true;
        }
        if slot.inherit_base {
            let base = bmux_slots::default_base_config_path();
            if base.exists() {
                writeln!(stdout, "    ✓ base.toml present at {}", base.display())?;
            } else {
                writeln!(
                    stdout,
                    "    ⚠ inherit_base = true but base.toml missing at {}",
                    base.display()
                )?;
            }
        }
    }
    if any_fail { Ok(1) } else { Ok(0) }
}

/// Emit a TOML/JSON/Nix block for installing a new slot.
pub(super) fn run_slot_install(
    name: &str,
    binary: &str,
    inherit_base: bool,
    format: SlotOutputFormat,
) -> Result<u8> {
    if let Err(e) = validate_slot_name(name) {
        eprintln!("invalid name: {e}");
        return Ok(2);
    }
    let mut stdout = std::io::stdout().lock();
    match format {
        SlotOutputFormat::Toml => {
            writeln!(stdout, "# Paste this block into your slots.toml:")?;
            writeln!(stdout, "[slots.{name}]")?;
            writeln!(stdout, "binary = {}", toml_string(binary))?;
            writeln!(stdout, "inherit_base = {inherit_base}")?;
        }
        SlotOutputFormat::Json => {
            let value = serde_json::json!({
                "name": name,
                "binary": binary,
                "inherit_base": inherit_base,
            });
            serde_json::to_writer_pretty(&mut stdout, &value)?;
            writeln!(stdout)?;
        }
        SlotOutputFormat::Nix => {
            writeln!(
                stdout,
                "# Paste this attrset into your Home Manager config:"
            )?;
            writeln!(stdout, "slots.{name} = {{")?;
            writeln!(stdout, "  binary = {};", nix_string(binary))?;
            writeln!(stdout, "  inheritBase = {inherit_base};")?;
            writeln!(stdout, "}};")?;
        }
    }

    // Read-only manifest advisory.
    let manifest_path = bmux_slots::default_manifest_path();
    if bmux_slots::is_read_only_manifest(&manifest_path) {
        eprintln!(
            "note: {} is managed declaratively (read-only) — this command \
             printed the block for you to insert manually.",
            manifest_path.display()
        );
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn load_manifest() -> Result<SlotManifest> {
    SlotManifest::load_default().map_err(|e| anyhow::anyhow!("failed loading slot manifest: {e}"))
}

fn resolve_slot_name<'a>(manifest: &'a SlotManifest, name: Option<&str>) -> Result<&'a Slot> {
    if let Some(n) = name {
        return manifest.get(n).map_err(|e| anyhow::anyhow!("{e}"));
    }
    // Active slot, else presentational default, else error.
    if let crate::runtime::slot::ActiveSlotState::Resolved { slot, .. } =
        crate::runtime::slot::active_slot()
    {
        return manifest.get(&slot.name).map_err(|e| anyhow::anyhow!("{e}"));
    }
    if let Some(d) = manifest.resolved_default() {
        return Ok(d);
    }
    anyhow::bail!(
        "no slot specified and no active or default slot is set (manifest has {} slots)",
        manifest.slots.len()
    )
}

fn emit_toml_list<W: Write>(w: &mut W, m: &SlotManifest) -> std::io::Result<()> {
    if let Some(ref d) = m.default {
        writeln!(w, "default = {}", toml_string(d))?;
        writeln!(w)?;
    }
    for (name, slot) in &m.slots {
        writeln!(w, "[slots.{name}]")?;
        emit_slot_toml_body(w, slot)?;
        writeln!(w)?;
    }
    Ok(())
}

fn emit_slot_toml_body<W: Write>(w: &mut W, slot: &Slot) -> std::io::Result<()> {
    writeln!(
        w,
        "binary       = {}",
        toml_string(&slot.binary.to_string_lossy())
    )?;
    writeln!(w, "inherit_base = {}", slot.inherit_base)?;
    writeln!(
        w,
        "config_dir   = {}",
        toml_string(&slot.config_dir.to_string_lossy())
    )?;
    writeln!(
        w,
        "runtime_dir  = {}",
        toml_string(&slot.runtime_dir.to_string_lossy())
    )?;
    writeln!(
        w,
        "data_dir     = {}",
        toml_string(&slot.data_dir.to_string_lossy())
    )?;
    writeln!(
        w,
        "state_dir    = {}",
        toml_string(&slot.state_dir.to_string_lossy())
    )?;
    writeln!(
        w,
        "log_dir      = {}",
        toml_string(&slot.log_dir.to_string_lossy())
    )?;
    Ok(())
}

fn emit_json_list<W: Write>(w: &mut W, m: &SlotManifest) -> std::io::Result<()> {
    let value = serde_json::json!({
        "default": m.default,
        "slots": m.slots.values().map(slot_to_json).collect::<Vec<_>>(),
    });
    serde_json::to_writer_pretty(&mut *w, &value)?;
    writeln!(w)
}

fn slot_to_json(slot: &Slot) -> serde_json::Value {
    serde_json::json!({
        "name": slot.name,
        "binary": slot.binary.to_string_lossy(),
        "inherit_base": slot.inherit_base,
        "config_dir": slot.config_dir.to_string_lossy(),
        "runtime_dir": slot.runtime_dir.to_string_lossy(),
        "data_dir": slot.data_dir.to_string_lossy(),
        "state_dir": slot.state_dir.to_string_lossy(),
        "log_dir": slot.log_dir.to_string_lossy(),
    })
}

fn emit_nix_list<W: Write>(w: &mut W, m: &SlotManifest) -> std::io::Result<()> {
    writeln!(w, "{{")?;
    if let Some(ref d) = m.default {
        writeln!(w, "  default = {};", nix_string(d))?;
    }
    writeln!(w, "  slots = {{")?;
    for (name, slot) in &m.slots {
        writeln!(w, "    {name} = {{")?;
        writeln!(
            w,
            "      binary = {};",
            nix_string(&slot.binary.to_string_lossy())
        )?;
        writeln!(w, "      inheritBase = {};", slot.inherit_base)?;
        writeln!(
            w,
            "      configDir = {};",
            nix_string(&slot.config_dir.to_string_lossy())
        )?;
        writeln!(
            w,
            "      runtimeDir = {};",
            nix_string(&slot.runtime_dir.to_string_lossy())
        )?;
        writeln!(
            w,
            "      dataDir = {};",
            nix_string(&slot.data_dir.to_string_lossy())
        )?;
        writeln!(
            w,
            "      stateDir = {};",
            nix_string(&slot.state_dir.to_string_lossy())
        )?;
        writeln!(
            w,
            "      logDir = {};",
            nix_string(&slot.log_dir.to_string_lossy())
        )?;
        writeln!(w, "    }};")?;
    }
    writeln!(w, "  }};")?;
    writeln!(w, "}}")?;
    Ok(())
}

fn emit_slot_nix<W: Write>(w: &mut W, slot: &Slot) -> std::io::Result<()> {
    writeln!(w, "{{")?;
    writeln!(w, "  name = {};", nix_string(&slot.name))?;
    writeln!(
        w,
        "  binary = {};",
        nix_string(&slot.binary.to_string_lossy())
    )?;
    writeln!(w, "  inheritBase = {};", slot.inherit_base)?;
    writeln!(
        w,
        "  configDir = {};",
        nix_string(&slot.config_dir.to_string_lossy())
    )?;
    writeln!(
        w,
        "  runtimeDir = {};",
        nix_string(&slot.runtime_dir.to_string_lossy())
    )?;
    writeln!(
        w,
        "  dataDir = {};",
        nix_string(&slot.data_dir.to_string_lossy())
    )?;
    writeln!(
        w,
        "  stateDir = {};",
        nix_string(&slot.state_dir.to_string_lossy())
    )?;
    writeln!(
        w,
        "  logDir = {};",
        nix_string(&slot.log_dir.to_string_lossy())
    )?;
    writeln!(w, "}}")?;
    Ok(())
}

fn toml_string(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str(r#"\""#),
            '\\' => out.push_str(r"\\"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            c if (c as u32) < 0x20 => {
                // `write!` into String: infallible.
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn nix_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_string_escapes_quotes_and_backslashes() {
        assert_eq!(toml_string(r#"a"b"#), r#""a\"b""#);
        assert_eq!(toml_string(r"a\b"), r#""a\\b""#);
    }

    #[test]
    fn nix_string_escapes_quotes() {
        assert_eq!(nix_string(r#"a"b"#), r#""a\"b""#);
    }
}
