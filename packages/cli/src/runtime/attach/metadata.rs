//! Project caller-owned descriptions through the same retained visibility authority.
use bmux_attach_pipeline::RetainedCompositor;
use bmux_tui::{geometry::Rect, selection::SelectionScene, semantic::SemanticScene};
use std::collections::BTreeMap;

pub(super) struct MetadataProjection {
    pub selection: SelectionScene,
    pub semantics: SemanticScene,
    pub text: BTreeMap<bmux_tui::selection::SelectionContentId, String>,
    /// Exact root-cell bounds. Cell-only scenes are conservative envelopes;
    /// native fractional hit testing must use these, not overlapping envelopes.
    pub selection_bounds:
        BTreeMap<bmux_tui::selection::SelectionFragmentId, bmux_tui::semantic::ProjectedBounds>,
    pub semantic_bounds: BTreeMap<String, bmux_tui::semantic::ProjectedBounds>,
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

#[cfg(test)]
pub(super) fn project(
    graph: &RetainedCompositor,
    area: Rect,
) -> anyhow::Result<MetadataProjection> {
    project_with_sources(graph, area, &BTreeMap::new())
}

/// Supply metadata captured from the same reflow/source revision as each local
/// grid. Entries are surface-local projected cells, never raw plugin root cells.
/// Stage the returned value with the output; install only on complete success.
// Keep admission, namespacing and the corresponding source payload atomic.
#[allow(clippy::too_many_lines)]
pub(super) fn project_with_sources(
    graph: &RetainedCompositor,
    area: Rect,
    projected: &BTreeMap<uuid::Uuid, bmux_plugin::surface::PluginSurfaceMetadata>,
) -> anyhow::Result<MetadataProjection> {
    let mut result = MetadataProjection {
        selection: SelectionScene::new(),
        semantics: SemanticScene::new(),
        text: BTreeMap::new(),
        selection_bounds: BTreeMap::new(),
        semantic_bounds: BTreeMap::new(),
    };
    let mut work = 0_usize;
    let mut source_work = 0_usize;
    let mut fragment_work = 0_usize;
    let mut text_bytes = 0_usize;
    for surface in graph.ordered_surfaces() {
        let metadata = if surface.content_transform.is_some() {
            projected.get(&surface.id).ok_or_else(|| {
                anyhow::anyhow!(
                    "transformed surface {} requires projection-aware source metadata",
                    surface.id
                )
            })?
        } else if let Some(metadata) = &surface.metadata {
            metadata
        } else {
            continue;
        };
        if let Some(scene) = &metadata.selection {
            fragment_work = fragment_work.saturating_add(scene.fragments().len());
            if scene.scopes().len() > 256 || fragment_work > 65_536 {
                anyhow::bail!("source selection entry budget exceeded");
            }
            scene
                .validate()
                .map_err(|error| anyhow::anyhow!("invalid source selection: {error:?}"))?;
        }
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
        let transform = surface.content_transform.unwrap_or_else(|| {
            bmux_tui::grid_fragment::GridTransform::new(
                bmux_tui::geometry::Point::new(surface.rect.x, surface.rect.y),
                1000,
            )
            .expect("unit scale is valid")
        });
        let bounds = |local| bmux_tui::semantic::ProjectedBounds::new(local, transform);
        let absolute = |local: Rect| {
            surface.content_transform.map_or_else(
                || {
                    Rect::new(
                        surface.rect.x.saturating_add(local.x),
                        surface.rect.y.saturating_add(local.y),
                        local.width,
                        local.height,
                    )
                },
                |transform| {
                    bmux_tui::semantic::ProjectedBounds::new(local, transform).map_or(
                        Rect::new(0, 0, 0, 0),
                        bmux_tui::semantic::ProjectedBounds::cell_cover,
                    )
                },
            )
        };
        if let Some(scene) = &metadata.selection {
            use unicode_segmentation::UnicodeSegmentation as _;
            let mut boundaries = BTreeMap::new();
            for (id, text) in &metadata.text {
                source_work = source_work.saturating_add(text.len());
                if text.len() > 65_536 || source_work > 4_194_304 {
                    anyhow::bail!("source boundary validation budget exceeded");
                }
                let mut offsets: std::collections::BTreeSet<_> = text
                    .grapheme_indices(true)
                    .map(|(start, _)| start)
                    .collect();
                offsets.insert(text.len());
                boundaries.insert(id, offsets);
            }
            let mut surviving = std::collections::BTreeSet::new();
            for fragment in scene.fragments() {
                if let Some(offsets) = boundaries.get(&fragment.content_id) {
                    let valid = |range: &std::ops::Range<usize>| {
                        range.start <= range.end
                            && offsets.contains(&range.start)
                            && offsets.contains(&range.end)
                    };
                    let expansion = |range: &std::ops::Range<usize>| {
                        valid(range)
                            && range.start <= fragment.source_range.start
                            && range.end >= fragment.source_range.end
                    };
                    if !valid(&fragment.source_range)
                        || fragment
                            .word_range
                            .as_ref()
                            .is_some_and(|range| !expansion(range))
                        || fragment
                            .line_range
                            .as_ref()
                            .is_some_and(|range| !expansion(range))
                    {
                        anyhow::bail!("selection range is not aligned to source graphemes");
                    }
                } else if surface.content_transform.is_some() {
                    anyhow::bail!("projected selection requires matching source text");
                }
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
                let exact = bounds(fragment.area);
                let mut fragment = fragment.clone();
                surviving.insert(fragment.scope_id.clone());
                fragment.id = namespace(fragment.id.as_str()).into();
                fragment.scope_id = namespace(fragment.scope_id.as_str()).into();
                fragment.content_id = namespace(fragment.content_id.as_str()).into();
                fragment.area = rect;
                if let Some(exact) = exact {
                    result.selection_bounds.insert(fragment.id.clone(), exact);
                }
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
            if scene.regions().len() > 4096 {
                anyhow::bail!("semantic entry budget exceeded");
            }
            for region in scene.regions() {
                let exact = bounds(region.area);
                let modal_excluded = graph.has_modal_scope()
                    && !surface.interactive_regions.iter().any(|interactive| {
                        interactive.id.as_ref().is_some_and(|id| {
                            id.region_local_id == region.id && graph.focus_allowed(id)
                        })
                    });
                let rect = absolute(region.area);
                let strips = visible_strips(graph, surface.id, rect.intersection(area), &mut work)?;
                // Text describes one complete source row. A geometric strip cannot
                // claim that entire row when any part is occluded or offscreen.
                if region
                    .accessibility
                    .as_ref()
                    .is_some_and(|metadata| metadata.text.is_some())
                    && (strips
                        .iter()
                        .map(|strip| usize::from(strip.width) * usize::from(strip.height))
                        .sum::<usize>()
                        != usize::from(rect.width) * usize::from(rect.height)
                        || exact.is_some_and(|bounds| {
                            bounds.min[0] < f64::from(area.x)
                                || bounds.min[1] < f64::from(area.y)
                                || bounds.max[0] > f64::from(area.right())
                                || bounds.max[1] > f64::from(area.bottom())
                        }))
                {
                    continue;
                }
                for (index, strip) in strips.into_iter().enumerate() {
                    let exact = exact.and_then(|bounds| bounds.clipped(strip));
                    let mut region = region.clone();
                    region.id = format!("{}:{index}", namespace(&region.id));
                    region.area = strip;
                    if modal_excluded && let Some(accessibility) = &mut region.accessibility {
                        accessibility.focused = false;
                        accessibility.actions.clear();
                        accessibility.disabled = true;
                    }
                    if let Some(exact) = exact {
                        result.semantic_bounds.insert(region.id.clone(), exact);
                    }
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
    fn transformed_metadata_requires_matching_projection_source() {
        let area = Rect::new(0, 0, 8, 2);
        let mut selection = SelectionScene::new();
        selection.push_scope(SelectionScope::new("label", area));
        let mut semantics = SemanticScene::new();
        semantics.push(SemanticRegion::new("label", area, "label"));
        let mut surface = RetainedSurface::new(
            Uuid::new_v4(),
            DamageRect::new(0, 0, 8, 2),
            0,
            0,
            false,
            Vec::new(),
        );
        surface.metadata = Some(PluginSurfaceMetadata {
            selection: Some(selection),
            semantics: Some(semantics),
            text: BTreeMap::new(),
        });
        surface = surface.with_content_transform(
            bmux_tui::grid_fragment::GridTransform::new(bmux_tui::geometry::Point::new(0, 0), 1500)
                .unwrap(),
        );
        let mut graph = RetainedCompositor::new();
        graph.replace_surfaces(
            vec![surface],
            DamageRect::new(0, 0, 8, 2),
            DamageCoalescingPolicy::default(),
        );
        assert!(project(&graph, area).is_err());
    }

    #[test]
    fn fractional_source_projection_occlusion_and_failed_staging() {
        use bmux_tui::{geometry::Point, grid_fragment::GridTransform};
        let area = Rect::new(0, 0, 16, 4);
        for scale in [875, 1375] {
            let mut selection = SelectionScene::new();
            selection.push_scope(SelectionScope::new("scope", Rect::new(0, 0, 3, 1)).revision(7));
            for fragment in
                plain_text_fragments("scope", "source", Rect::new(0, 0, 3, 1), 0, "A界", 0, 7)
            {
                selection.push_fragment(fragment);
            }
            let mut semantics = SemanticScene::new();
            semantics.push(SemanticRegion::new("label", Rect::new(0, 0, 3, 1), "label"));
            let id = Uuid::new_v4();
            let source =
                RetainedSurface::new(id, DamageRect::new(2, 0, 8, 3), 0, 0, false, Vec::new())
                    .with_content_transform(GridTransform::new(Point::new(2, 0), scale).unwrap());
            let sources = [(
                id,
                PluginSurfaceMetadata {
                    selection: Some(selection),
                    semantics: Some(semantics),
                    text: [("source".into(), "A界".into())].into(),
                },
            )]
            .into();
            let mut graph = RetainedCompositor::new();
            graph.replace_surfaces(
                vec![source.clone()],
                DamageRect::new(0, 0, 16, 4),
                DamageCoalescingPolicy::default(),
            );
            let committed = project_with_sources(&graph, area, &sources).unwrap();
            assert_eq!(committed.selection.fragments().len(), 2);
            let wide = &committed.selection.fragments()[1];
            assert_eq!(wide.source_range, 1..4);
            let exact = committed.selection_bounds[&wide.id];
            assert_eq!(exact.min[0], 2.0 + f64::from(scale) / 1000.0);
            assert_eq!(committed.text[&wide.content_id], "A界");
            let cover = RetainedSurface::new(
                Uuid::new_v4(),
                DamageRect::new(4, 0, 1, 2),
                10,
                0,
                true,
                Vec::new(),
            );
            graph.replace_surfaces(
                vec![source, cover],
                DamageRect::new(0, 0, 16, 4),
                DamageCoalescingPolicy::default(),
            );
            let clipped = project_with_sources(&graph, area, &sources).unwrap();
            assert!(
                clipped
                    .selection
                    .fragments()
                    .iter()
                    .all(|fragment| fragment.source_range != (1..4))
            );
            // Projection is staged data: a failed later attempt cannot mutate the
            // previously committed metadata or its source identity.
            assert!(project(&graph, area).is_err());
            assert_eq!(committed.selection.fragments()[1].id, wide.id);
            assert_eq!(committed.text[&wide.content_id], "A界");
        }
    }

    #[test]
    fn unrelated_modal_suppresses_background_accessibility_focus_and_actions() {
        use bmux_tui::semantic::{SemanticAccessibility, SemanticAction};
        let area = Rect::new(0, 0, 8, 2);
        let mut region = SemanticRegion::new("input", Rect::new(0, 0, 2, 1), "textbox");
        region.accessibility = Some(SemanticAccessibility {
            focused: true,
            actions: vec![SemanticAction::Focus],
            ..Default::default()
        });
        let mut semantics = SemanticScene::new();
        semantics.push(region);
        let mut source = RetainedSurface::new(
            Uuid::new_v4(),
            DamageRect::new(0, 0, 8, 2),
            0,
            0,
            false,
            Vec::new(),
        );
        source.metadata = Some(PluginSurfaceMetadata {
            selection: None,
            semantics: Some(semantics),
            text: BTreeMap::new(),
        });
        let mut modal = RetainedSurface::new(
            Uuid::new_v4(),
            DamageRect::new(6, 0, 2, 1),
            10,
            0,
            true,
            Vec::new(),
        );
        modal.modal = true;
        let mut graph = RetainedCompositor::new();
        graph.replace_surfaces(
            vec![source, modal],
            DamageRect::new(0, 0, 8, 2),
            DamageCoalescingPolicy::default(),
        );
        let result = project(&graph, area).unwrap();
        let accessibility = result.semantics.regions()[0]
            .accessibility
            .as_ref()
            .unwrap();
        assert!(!accessibility.focused);
        assert!(accessibility.disabled);
        assert_eq!(accessibility.actions.len(), 0);
    }

    #[test]
    fn partially_occluded_accessible_text_is_not_repeated_in_visible_strips() {
        use bmux_attach_pipeline::{DamageCoalescingPolicy, DamageRect, RetainedSurface};
        use bmux_plugin::surface::PluginSurfaceMetadata;
        use bmux_tui::semantic::{SemanticAccessibility, SemanticRegion};
        let area = Rect::new(0, 0, 17, 1);
        let mut semantics = SemanticScene::new();
        let mut region = SemanticRegion::new("row", area, "terminal-row");
        region.accessibility = Some(SemanticAccessibility {
            text: Some("LEFT SECRET RIGHT".into()),
            ..SemanticAccessibility::default()
        });
        semantics.push(region);
        let mut source = RetainedSurface::new(
            uuid::Uuid::new_v4(),
            DamageRect::new(0, 0, 17, 1),
            0,
            0,
            false,
            Vec::new(),
        );
        source.metadata = Some(PluginSurfaceMetadata {
            selection: None,
            semantics: Some(semantics),
            text: BTreeMap::new(),
        });
        let cover = RetainedSurface::new(
            uuid::Uuid::new_v4(),
            DamageRect::new(5, 0, 6, 1),
            10,
            0,
            true,
            Vec::new(),
        );
        let mut graph = RetainedCompositor::new();
        graph.replace_surfaces(
            vec![source, cover],
            DamageRect::new(0, 0, 17, 1),
            DamageCoalescingPolicy::default(),
        );
        assert_eq!(project(&graph, area).unwrap().semantics.regions().len(), 0);
    }

    #[test]
    fn full_terminal_metadata_validates_shared_logical_text_once() {
        use bmux_attach_pipeline::{DamageCoalescingPolicy, DamageRect, RetainedSurface};
        use bmux_plugin::surface::PluginSurfaceMetadata;
        use bmux_tui::selection::{SelectionFragment, SelectionScope};
        let area = Rect::new(0, 0, 80, 60);
        let mut selection = SelectionScene::new();
        selection.push_scope(SelectionScope::new("scope", area));
        for i in 0..4800 {
            selection.push_fragment(SelectionFragment::new(
                "scope",
                "line",
                Rect::new(
                    u16::try_from(i % 80).unwrap(),
                    u16::try_from(i / 80).unwrap(),
                    1,
                    1,
                ),
                u64::try_from(i).unwrap(),
                i..i + 1,
            ));
        }
        let mut surface = RetainedSurface::new(
            uuid::Uuid::new_v4(),
            DamageRect::new(0, 0, 80, 60),
            0,
            0,
            false,
            Vec::new(),
        );
        surface.metadata = Some(PluginSurfaceMetadata {
            selection: Some(selection),
            semantics: None,
            text: [("line".into(), "x".repeat(4800))].into(),
        });
        let mut graph = RetainedCompositor::new();
        graph.replace_surfaces(
            vec![surface],
            DamageRect::new(0, 0, 80, 60),
            DamageCoalescingPolicy::default(),
        );
        assert_eq!(
            project(&graph, area).unwrap().selection.fragments().len(),
            4800
        );
    }

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
