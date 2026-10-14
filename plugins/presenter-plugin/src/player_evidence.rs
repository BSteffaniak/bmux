//! Opt-in, presenter-private evidence projected from the successfully delivered scene.
use bmux_tui::presentation::PreparedPresentation;
use serde_json::{Value, json};
use std::{io::Write, path::PathBuf};

pub(crate) fn capture(prepared: &PreparedPresentation, demo: Option<Value>) -> Option<Value> {
    std::env::var_os("BMUX_PRESENTER_FRAME_EVIDENCE")?;
    let area = prepared.buffer().area();
    let rows = (area.y..area.y.saturating_add(area.height))
        .map(|y| prepared.buffer().row_symbols(y).unwrap_or_default())
        .collect::<Vec<_>>();
    let controls = prepared
        .hits()
        .regions()
        .iter()
        .filter_map(|region| {
            let input = crate::playbook_component::target_input(region.id.as_str())?;
            if region.role != bmux_tui::hit::HitRole::Action
                || !region.visible
                || region.area.is_empty()
                || region.focus_scope.is_none()
            {
                return None;
            }
            Some(
                json!({"id":region.id.as_str(),"input":input,"x":region.area.x,"y":region.area.y,
            "width":region.area.width,"height":region.area.height,"enabled":region.enabled,"pointer_events":region.pointer_events,"layer":region.layer}),
            )
        })
        .collect::<Vec<_>>();
    Some(
        json!({"revision":0,"width":area.width,"height":area.height,"rows":rows,"controls":controls,"demo":demo}),
    )
}

pub(crate) fn publish(evidence: &Value) -> anyhow::Result<()> {
    let Some(path) = std::env::var_os("BMUX_PRESENTER_FRAME_EVIDENCE") else {
        return Ok(());
    };
    let path = PathBuf::from(path);
    anyhow::ensure!(path.is_absolute(), "evidence path must be absolute");
    anyhow::ensure!(
        evidence["width"].as_u64().unwrap_or(0) * evidence["height"].as_u64().unwrap_or(0)
            <= 262_144,
        "evidence viewport exceeds budget"
    );
    let temp = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> anyhow::Result<()> {
        let mut file = options.open(&temp)?;
        file.write_all(&serde_json::to_vec(evidence)?)?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}
