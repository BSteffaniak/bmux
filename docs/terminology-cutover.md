# Canonical terminology cutover

The terminology decision is a clean break: tabs (`bmux.tabs`) and the tab bar
(`bmux.tab_bar`) replace the former domain/plugin names. `concepts.md` owns the
vocabulary. This changes names, not ownership, resource identity, or lifetime.
The foundational plugin boundary formerly naming windows now names tabs.

## Upgrade

Stop old clients and servers before upgrading. Install the new executable and
all bundled plugins together; update declarative configuration before starting.
Old command aliases are not retained. The IPC wire epoch advances to 4 so an old
executable cannot connect and interpret the renamed contracts accidentally.
Do not operate a mixed-epoch federation; epoch negotiation rejects it.

The tabs implementation migrates its local storage directory on activation:

- `plugin-storage/bmux.windows` becomes `plugin-storage/bmux.tabs`.
- Files beginning `windows.` become `tabs.` without changing their bytes.
- Renames are synchronized. A restart resumes partially renamed files.
- Conflicting old/new authorities fail explicitly; neither is overwritten.
- Other files are preserved. Resource IDs and order are unchanged.

Migration requires the old runtime to be stopped. Automatic filesystem migration
currently requires directory synchronization support on Unix. Other platforms
return an explicit recovery error rather than claiming successful durability.

Federated control storage uses positional binary codecs. Renaming Rust/BPDL
identifiers does not change their tags, field order, schema versions, or bytes;
existing recovery state remains canonical rather than being reconstructed from
a presentation. The wire epoch separates incompatible public naming contracts.

The Nix configuration source is `configs/bmux/bmux.toml`, consumed by its existing
Home Manager module. Its source pin must point to a revision containing this
cutover before deploying that configuration; do not switch the configuration
against an old executable.
