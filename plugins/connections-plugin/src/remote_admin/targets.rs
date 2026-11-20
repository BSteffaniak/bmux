use anyhow::Result;
use bmux_config::BmuxConfig;
pub(super) fn run_remote_complete_targets() -> Result<u8> {
    let config = BmuxConfig::load()?;
    let mut names = config
        .connections
        .targets
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    names.sort();
    names.sort_by_key(|name| {
        config
            .connections
            .recent_targets
            .iter()
            .position(|value| value == name)
            .unwrap_or(usize::MAX)
    });
    for name in names {
        println!("{name}");
    }
    Ok(0)
}
