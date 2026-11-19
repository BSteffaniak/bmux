//! Print the frozen `semver` fixture for `bmux_plugin::dependency_version`.
fn main() {
    print!("{}", plugin_version_oracle::fixture());
}
