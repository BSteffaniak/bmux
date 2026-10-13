//! Project caller-owned descriptions through the same retained visibility authority.
use bmux_attach_pipeline::RetainedCompositor;
use bmux_tui::{geometry::Rect, selection::SelectionScene, semantic::SemanticScene};
use std::collections::BTreeMap;

pub(super) struct MetadataProjection {
    pub selection: SelectionScene,
    pub semantics: SemanticScene,
    pub text: BTreeMap<bmux_tui::selection::SelectionContentId, String>,
}

fn visible_strips(
    graph: &RetainedCompositor,
    source: uuid::Uuid,
    rect: Rect,
    work: &mut usize,
) -> anyhow::Result<Vec<Rect>> {
    let mut strips = Vec::new();
    for y in rect.y..rect.bottom() {
        let mut start = None;
        for x in rect.x..rect.right() {
            *work = work.saturating_add(graph.surfaces().len().max(1));
            if *work > 4_194_304 {
                anyhow::bail!("metadata visibility work budget exceeded");
            }
            if graph.point_visible_from(source, x, y) {
                start.get_or_insert(x);
            } else if let Some(start) = start.take() {
                strips.push(Rect::new(start, y, x - start, 1));
            }
        }
        if let Some(start) = start {
            strips.push(Rect::new(start, y, rect.right() - start, 1));
        }
    }
    Ok(strips)
}

// Keep the visibility/identity transaction together: splitting would separate
// logical fragment admission from the matching owner-scoped source payload.
#[allow(clippy::too_many_lines)]
pub(super) fn project(
    graph: &RetainedCompositor,
    area: Rect,
) -> anyhow::Result<MetadataProjection> {
    let mut result = MetadataProjection {
        selection: SelectionScene::new(),
        semantics: SemanticScene::new(),
        text: BTreeMap::new(),
    };
    let mut work = 0_usize;
    let mut text_bytes = 0_usize;
    for surface in graph.ordered_surfaces() {
        let Some(metadata) = &surface.metadata else {
            continue;
        };
        let prefix = surface.owner.as_ref().map_or_else(
            || surface.id.to_string(),
            |owner| {
                format!(
                    "{}:{}:{}:{}:{}",
                    owner.owner_plugin_id.len(),
                    owner.owner_plugin_id,
                    owner.local_id.len(),
                    owner.local_id,
                    surface.id
                )
            },
        );
        let namespace = |id: &str| format!("{}:{}:{}", prefix, id.len(), id);
        let absolute = |local: Rect| {
            Rect::new(
                surface.rect.x.saturating_add(local.x),
                surface.rect.y.saturating_add(local.y),
                local.width,
                local.height,
            )
        };
        if let Some(scene) = &metadata.selection {
            let mut surviving = std::collections::BTreeSet::new();
            for fragment in scene.fragments() {
                let rect = absolute(fragment.area);
                let strips = visible_strips(graph, surface.id, rect.intersection(area), &mut work)?;
                // A partial grapheme cannot truthfully describe a logical source range.
                if strips
                    .iter()
                    .map(|strip| usize::from(strip.width))
                    .sum::<usize>()
                    != usize::from(rect.width) * usize::from(rect.height)
                {
                    continue;
                }
                let mut fragment = fragment.clone();
                surviving.insert(fragment.scope_id.clone());
                fragment.id = namespace(fragment.id.as_str()).into();
                fragment.scope_id = namespace(fragment.scope_id.as_str()).into();
                fragment.content_id = namespace(fragment.content_id.as_str()).into();
                fragment.area = rect;
                result.selection.push_fragment(fragment);
            }
            // Retain ancestors only for visible logical fragments. Container bounds
            // are descriptive, never hit targets: this path installs no controller.
            for fragment in scene.fragments() {
                if !surviving.contains(&fragment.scope_id) {
                    continue;
                }
                let mut ancestor = scene
                    .scopes()
                    .iter()
                    .find(|scope| scope.id == fragment.scope_id)
                    .and_then(|scope| scope.parent.as_ref());
                while let Some(id) = ancestor {
                    surviving.insert(id.clone());
                    ancestor = scene
                        .scopes()
                        .iter()
                        .find(|scope| &scope.id == id)
                        .and_then(|scope| scope.parent.as_ref());
                }
            }
            for scope in scene
                .scopes()
                .iter()
                .filter(|scope| surviving.contains(&scope.id))
            {
                let strips = visible_strips(
                    graph,
                    surface.id,
                    absolute(scope.area).intersection(area),
                    &mut work,
                )?;
                if strips.is_empty() {
                    continue;
                }
                let mut scope = scope.clone();
                let left = strips.iter().map(|strip| strip.x).min().unwrap_or(0);
                let top = strips.iter().map(|strip| strip.y).min().unwrap_or(0);
                let right = strips
                    .iter()
                    .map(|strip| strip.right())
                    .max()
                    .unwrap_or(left);
                let bottom = strips
                    .iter()
                    .map(|strip| strip.bottom())
                    .max()
                    .unwrap_or(top);
                scope.id = namespace(scope.id.as_str()).into();
                scope.parent = scope
                    .parent
                    .as_ref()
                    .map(|id| namespace(id.as_str()).into());
                scope.area = Rect::new(left, top, right - left, bottom - top);
                scope.initiation_area = Rect::new(left, top, 0, 0);
                scope.capture = bmux_tui::selection::SelectionCapture::Disabled;
                result.selection.push_scope(scope);
            }
        }
        if let Some(scene) = &metadata.semantics {
            for region in scene.regions() {
                let rect = absolute(region.area);
                for strip in visible_strips(graph, surface.id, rect.intersection(area), &mut work)?
                {
                    let mut region = region.clone();
                    region.id = format!("{}:{}:{}", namespace(&region.id), strip.y, strip.x);
                    region.area = strip;
                    result.semantics.push(region);
                }
            }
        }
        for (id, text) in &metadata.text {
            let id = namespace(id.as_str());
            // Do not transport source content which has no visible logical fragment.
            if !result
                .selection
                .fragments()
                .iter()
                .any(|fragment| fragment.content_id.as_str() == id)
            {
                continue;
            }
            text_bytes = text_bytes.saturating_add(text.len());
            if id.len() > 1024
                || text.len() > 65_536
                || result.text.len() >= 4096
                || text_bytes > 1_048_576
            {
                anyhow::bail!("logical source text budget exceeded");
            }
            result.text.insert(id.into(), text.clone());
        }
        if result
            .selection
            .scopes()
            .len()
            .saturating_add(result.selection.fragments().len())
            .saturating_add(result.semantics.regions().len())
            > 131_072
        {
            anyhow::bail!("metadata entry budget exceeded");
        }
    }
    result
        .selection
        .validate()
        .map_err(|error| anyhow::anyhow!("invalid projected logical scene: {error:?}"))?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_attach_pipeline::{DamageCoalescingPolicy, DamageRect, RetainedSurface};
    use bmux_plugin::surface::{PluginSurfaceId, PluginSurfaceMetadata};
    use bmux_tui::selection::{SelectionScope, plain_text_fragments};
    use bmux_tui::semantic::SemanticRegion;
    use uuid::Uuid;

    #[test]
    fn real_source_identity_survives_translation_occlusion_and_owner_removal() {
        let area = Rect::new(0, 0, 8, 2);
        let mut selection = SelectionScene::new();
        selection.push_scope(SelectionScope::new("label", Rect::new(0, 0, 3, 1)).revision(7));
        for fragment in
            plain_text_fragments("label", "content", Rect::new(0, 0, 3, 1), 0, "A界", 0, 7)
        {
            selection.push_fragment(fragment);
        }
        let mut semantics = SemanticScene::new();
        semantics.push(SemanticRegion::new("label", Rect::new(0, 0, 3, 1), "label"));
        let id = Uuid::new_v4();
        let mut source =
            RetainedSurface::new(id, DamageRect::new(2, 0, 3, 1), 0, 0, false, Vec::new());
        source.owner = Some(PluginSurfaceId::new("producer", "label", id));
        source.metadata = Some(PluginSurfaceMetadata {
            selection: Some(selection),
            semantics: Some(semantics),
            text: [("content".into(), "A界".to_owned())].into(),
        });
        let mut graph = RetainedCompositor::new();
        graph.replace_surfaces(
            vec![source.clone()],
            DamageRect::new(0, 0, 8, 2),
            DamageCoalescingPolicy::default(),
        );
        let first = project(&graph, area).unwrap();
        assert_eq!(first.selection.fragments().len(), 2);
        assert_eq!(first.selection.fragments()[1].area, Rect::new(3, 0, 2, 1));
        let key = first.selection.fragments()[0].content_id.clone();
        assert_eq!(first.text[&key], "A界");
        let cover = RetainedSurface::new(
            Uuid::new_v4(),
            DamageRect::new(4, 0, 1, 1),
            10,
            0,
            true,
            Vec::new(),
        );
        graph.replace_surfaces(
            vec![source, cover],
            DamageRect::new(0, 0, 8, 2),
            DamageCoalescingPolicy::default(),
        );
        let clipped = project(&graph, area).unwrap();
        assert_eq!(clipped.selection.fragments().len(), 1);
        assert_eq!(clipped.selection.fragments()[0].content_id, key);
        assert!(
            clipped
                .semantics
                .regions()
                .iter()
                .all(|region| region.area.right() <= 4)
        );
        graph.replace_surfaces(
            Vec::new(),
            DamageRect::new(0, 0, 8, 2),
            DamageCoalescingPolicy::default(),
        );
        assert!(project(&graph, area).unwrap().text.is_empty());
    }
}
