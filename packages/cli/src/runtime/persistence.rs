use super::PaneRuntime;
use crate::pane::{LayoutNode, LayoutTree, PaneId, SplitDirection};
use anyhow::{Context, Result};
use bmux_config::ConfigPaths;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const RUNTIME_STATE_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub(super) struct PersistedPaneMeta {
    pub(super) title: String,
    pub(super) shell: String,
}

#[derive(Debug, Clone)]
pub(super) struct PersistedRuntimeState {
    pub(super) layout_tree: LayoutTree,
    pub(super) panes: BTreeMap<PaneId, PersistedPaneMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RuntimeStateFile {
    version: u32,
    focused_pane: u16,
    layout: PersistedLayoutNode,
    panes: Vec<PersistedPaneEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedPaneEntry {
    id: u16,
    title: String,
    shell: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PersistedLayoutNode {
    Leaf {
        pane_id: u16,
    },
    Split {
        direction: PersistedSplitDirection,
        ratio: f32,
        first: Box<PersistedLayoutNode>,
        second: Box<PersistedLayoutNode>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedSplitDirection {
    Vertical,
    Horizontal,
}

pub(super) fn load_persisted_runtime_state() -> Result<Option<PersistedRuntimeState>> {
    let path = ConfigPaths::default().runtime_layout_state_file();
    if !path.exists() {
        return Ok(None);
    }

    let bytes = std::fs::read(&path).with_context(|| {
        format!(
            "failed reading persisted runtime state at {}",
            path.display()
        )
    })?;
    let file: RuntimeStateFile = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "failed parsing persisted runtime state at {}",
            path.display()
        )
    })?;

    if file.version != RUNTIME_STATE_VERSION {
        return Ok(None);
    }

    let layout_root = deserialize_layout_node(&file.layout);
    let pane_order = {
        let mut ids = Vec::new();
        collect_leaf_ids(&layout_root, &mut ids);
        ids
    };
    if pane_order.is_empty() {
        return Ok(None);
    }

    let unique_ids: BTreeSet<PaneId> = pane_order.iter().copied().collect();
    if unique_ids.len() != pane_order.len() {
        return Ok(None);
    }

    let pane_meta_by_id: BTreeMap<PaneId, PersistedPaneMeta> = file
        .panes
        .into_iter()
        .map(|entry| {
            (
                PaneId(entry.id),
                PersistedPaneMeta {
                    title: entry.title,
                    shell: entry.shell,
                },
            )
        })
        .collect();

    if pane_order
        .iter()
        .any(|pane_id| !pane_meta_by_id.contains_key(pane_id))
    {
        return Ok(None);
    }

    let focused = PaneId(file.focused_pane);
    let focused = if unique_ids.contains(&focused) {
        focused
    } else {
        pane_order[0]
    };

    let panes = pane_order
        .iter()
        .filter_map(|pane_id| {
            pane_meta_by_id
                .get(pane_id)
                .cloned()
                .map(|meta| (*pane_id, meta))
        })
        .collect();

    Ok(Some(PersistedRuntimeState {
        layout_tree: LayoutTree {
            root: layout_root,
            focused,
        },
        panes,
    }))
}

pub(super) fn save_persisted_runtime_state(
    layout_tree: &LayoutTree,
    panes: &BTreeMap<PaneId, PaneRuntime>,
    focused_pane: PaneId,
) -> Result<()> {
    let path = ConfigPaths::default().runtime_layout_state_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed creating runtime state directory at {}",
                parent.display()
            )
        })?;
    }

    let pane_order = layout_tree.pane_order();
    if pane_order.is_empty() {
        return Ok(());
    }

    let state_file = RuntimeStateFile {
        version: RUNTIME_STATE_VERSION,
        focused_pane: focused_pane.0,
        layout: serialize_layout_node(&layout_tree.root),
        panes: pane_order
            .into_iter()
            .filter_map(|pane_id| panes.get(&pane_id).map(|pane| (pane_id, pane)))
            .map(|(pane_id, pane)| PersistedPaneEntry {
                id: pane_id.0,
                title: pane.title.clone(),
                shell: pane.shell.clone(),
            })
            .collect(),
    };

    let payload =
        serde_json::to_vec_pretty(&state_file).context("failed encoding runtime state")?;
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, payload).with_context(|| {
        format!(
            "failed writing temporary runtime state file at {}",
            tmp_path.display()
        )
    })?;
    std::fs::rename(&tmp_path, &path)
        .with_context(|| format!("failed replacing runtime state file at {}", path.display()))?;

    Ok(())
}

fn serialize_layout_node(node: &LayoutNode) -> PersistedLayoutNode {
    match node {
        LayoutNode::Leaf { pane_id } => PersistedLayoutNode::Leaf { pane_id: pane_id.0 },
        LayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } => PersistedLayoutNode::Split {
            direction: match direction {
                SplitDirection::Vertical => PersistedSplitDirection::Vertical,
                SplitDirection::Horizontal => PersistedSplitDirection::Horizontal,
            },
            ratio: *ratio,
            first: Box::new(serialize_layout_node(first)),
            second: Box::new(serialize_layout_node(second)),
        },
    }
}

fn deserialize_layout_node(node: &PersistedLayoutNode) -> LayoutNode {
    match node {
        PersistedLayoutNode::Leaf { pane_id } => LayoutNode::Leaf {
            pane_id: PaneId(*pane_id),
        },
        PersistedLayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } => LayoutNode::Split {
            direction: match direction {
                PersistedSplitDirection::Vertical => SplitDirection::Vertical,
                PersistedSplitDirection::Horizontal => SplitDirection::Horizontal,
            },
            ratio: *ratio,
            first: Box::new(deserialize_layout_node(first)),
            second: Box::new(deserialize_layout_node(second)),
        },
    }
}

fn collect_leaf_ids(node: &LayoutNode, out: &mut Vec<PaneId>) {
    match node {
        LayoutNode::Leaf { pane_id } => out.push(*pane_id),
        LayoutNode::Split { first, second, .. } => {
            collect_leaf_ids(first, out);
            collect_leaf_ids(second, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{deserialize_layout_node, serialize_layout_node};
    use crate::pane::{LayoutTree, PaneId, SplitDirection};

    #[test]
    fn layout_node_roundtrips() {
        let mut tree = LayoutTree::two_pane(PaneId(1), PaneId(2), SplitDirection::Vertical, 0.5);
        tree.focused = PaneId(2);
        assert!(tree.split_focused(SplitDirection::Horizontal, PaneId(3), 0.5));

        let serialized = serialize_layout_node(&tree.root);
        let deserialized = deserialize_layout_node(&serialized);

        assert_eq!(tree.root, deserialized);
    }
}
