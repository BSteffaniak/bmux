//! Window-lifetime AccessKit bridge for committed retained semantics.
//!
//! Only explicit visible semantic text is exported, never terminal history. The
//! host must commit after successful presentation and invalidate on resize/loss.
//! Actions are bounded, generation-fenced intents, not application mutations.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use accesskit::{
    Action, ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler, Node, NodeId,
    Role, TreeId, TreeInfo, TreeUpdate,
};
use bmux_tui::geometry::Rect;
use bmux_tui::semantic::{SemanticAction, SemanticScene};

const ROOT: NodeId = NodeId(0);
const MAX_REGIONS: usize = 4096;
const MAX_TEXT_BYTES: usize = 1_048_576;
const MAX_ACTIONS: usize = 64;

/// Mapping from committed presentation cells to physical window pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AccessibilityGeometry {
    /// Committed visible cell clip, not execution-grid dimensions.
    pub clip: Rect,
    pub origin_x: f64,
    pub origin_y: f64,
    pub cell_width: f64,
    pub cell_height: f64,
}

/// Intent to route through existing interaction handling after revision checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessibilityAction {
    pub generation: u64,
    pub semantic_id: String,
    pub action: SemanticAction,
}

/// Rejection is explicit; rejected updates leave the previous tree intact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessibilityError {
    InvalidGeometry,
    StaleGeneration,
    DuplicateIdentity,
    BudgetExceeded,
    ActionQueueOverflow,
}

struct Target {
    id: String,
    actions: Vec<SemanticAction>,
}

struct State {
    generation: u64,
    next_id: u64,
    targets: BTreeMap<NodeId, Target>,
    tree: TreeUpdate,
    queue: VecDeque<AccessibilityAction>,
    overflow: bool,
}

fn empty_tree() -> TreeUpdate {
    TreeUpdate {
        nodes: vec![(ROOT, Node::new(Role::Window))],
        tree: Some(TreeInfo::new(ROOT)),
        tree_id: TreeId::ROOT,
        focus: ROOT,
    }
}

impl State {
    fn new() -> Self {
        Self {
            generation: 0,
            next_id: 1,
            targets: BTreeMap::new(),
            tree: empty_tree(),
            queue: VecDeque::new(),
            overflow: false,
        }
    }

    fn commit(
        &mut self,
        generation: u64,
        scene: &SemanticScene,
        geometry: AccessibilityGeometry,
    ) -> Result<(), AccessibilityError> {
        if generation <= self.generation {
            return Err(AccessibilityError::StaleGeneration);
        }
        let g = geometry;
        if ![g.origin_x, g.origin_y, g.cell_width, g.cell_height]
            .iter()
            .all(|v| v.is_finite())
            || g.cell_width <= 0.0
            || g.cell_height <= 0.0
        {
            return Err(AccessibilityError::InvalidGeometry);
        }
        let right = g.origin_x + (f64::from(g.clip.x) + f64::from(g.clip.width)) * g.cell_width;
        let bottom = g.origin_y + (f64::from(g.clip.y) + f64::from(g.clip.height)) * g.cell_height;
        if !right.is_finite() || !bottom.is_finite() {
            return Err(AccessibilityError::InvalidGeometry);
        }
        if scene.regions().len() > MAX_REGIONS {
            return Err(AccessibilityError::BudgetExceeded);
        }
        let mut bytes = 0usize;
        let mut seen = BTreeMap::new();
        for region in scene.regions() {
            bytes = bytes
                .saturating_add(region.id.len())
                .saturating_add(region.role.len());
            if let Some(a) = &region.accessibility {
                for text in [&a.label, &a.description, &a.text].into_iter().flatten() {
                    bytes = bytes.saturating_add(text.len());
                }
                if a.actions.len() > 3 {
                    return Err(AccessibilityError::BudgetExceeded);
                }
            }
            if seen.insert(&region.id, ()).is_some() {
                return Err(AccessibilityError::DuplicateIdentity);
            }
        }
        if bytes > MAX_TEXT_BYTES {
            return Err(AccessibilityError::BudgetExceeded);
        }
        let mut nodes = Vec::new();
        let mut children = Vec::new();
        let mut targets = BTreeMap::new();
        let mut next_id = self.next_id;
        let mut focus = ROOT;
        for region in scene.regions() {
            let area = region.area.intersection(g.clip);
            if area.is_empty() {
                continue;
            }
            // Never recycle a platform node ID across commits: delayed OS actions
            // for an old tree must not acquire the newest frame's authority.
            let id = NodeId(next_id);
            next_id = next_id
                .checked_add(1)
                .ok_or(AccessibilityError::BudgetExceeded)?;
            let mut node = Node::new(match region.role.as_str() {
                "button" => Role::Button,
                "text" | "label" => Role::Label,
                "textbox" | "text_input" => Role::TextInput,
                "tab" => Role::Tab,
                "terminal" => Role::Terminal,
                _ => Role::GenericContainer,
            });
            let x = g.origin_x + f64::from(area.x) * g.cell_width;
            let y = g.origin_y + f64::from(area.y) * g.cell_height;
            node.set_bounds(accesskit::Rect::new(
                x,
                y,
                x + f64::from(area.width) * g.cell_width,
                y + f64::from(area.height) * g.cell_height,
            ));
            if let Some(a) = &region.accessibility {
                if !a.sensitive {
                    if let Some(label) = &a.label {
                        node.set_label(label.clone());
                    }
                    if let Some(description) = &a.description {
                        node.set_description(description.clone());
                    }
                    if let Some(text) = &a.text {
                        node.set_value(text.clone());
                    }
                }
                if a.disabled {
                    node.set_disabled();
                }
                if let Some(selected) = a.selected {
                    node.set_selected(selected);
                }
                if !a.disabled {
                    for action in &a.actions {
                        node.add_action(to_accesskit(*action));
                    }
                    if a.focused {
                        focus = id;
                    }
                    targets.insert(
                        id,
                        Target {
                            id: region.id.clone(),
                            actions: a.actions.clone(),
                        },
                    );
                }
            }
            children.push(id);
            nodes.push((id, node));
        }
        let mut root = Node::new(Role::Window);
        root.set_children(children);
        nodes.push((ROOT, root));
        self.tree = TreeUpdate {
            nodes,
            tree: Some(TreeInfo::new(ROOT)),
            tree_id: TreeId::ROOT,
            focus,
        };
        self.generation = generation;
        self.next_id = next_id;
        self.targets = targets;
        self.queue.clear();
        self.overflow = false;
        Ok(())
    }

    fn invalidate(&mut self) {
        self.tree = empty_tree();
        self.targets.clear();
        self.queue.clear();
        self.overflow = false;
    }

    fn action(&mut self, request: ActionRequest) {
        if request.target_tree != TreeId::ROOT {
            return;
        }
        let action = match request.action {
            Action::Focus => SemanticAction::Focus,
            Action::Click => SemanticAction::Activate,
            Action::ShowContextMenu => SemanticAction::ShowContextMenu,
            _ => return,
        };
        let Some(target) = self.targets.get(&request.target_node) else {
            return;
        };
        if !target.actions.contains(&action) {
            return;
        }
        if self.queue.len() == MAX_ACTIONS {
            self.overflow = true;
            return;
        }
        self.queue.push_back(AccessibilityAction {
            generation: self.generation,
            semantic_id: target.id.clone(),
            action,
        });
    }
}

fn to_accesskit(action: SemanticAction) -> Action {
    match action {
        SemanticAction::Focus => Action::Focus,
        SemanticAction::Activate => Action::Click,
        SemanticAction::ShowContextMenu => Action::ShowContextMenu,
    }
}

struct Activation(Arc<Mutex<State>>);
impl ActivationHandler for Activation {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        self.0.lock().ok().map(|s| s.tree.clone())
    }
}
struct Actions {
    state: Arc<Mutex<State>>,
    wake: Arc<dyn Fn() + Send + Sync>,
}
impl ActionHandler for Actions {
    fn do_action(&mut self, request: ActionRequest) {
        if let Ok(mut state) = self.state.lock() {
            state.action(request);
        }
        (self.wake)();
    }
}
struct Deactivation;
impl DeactivationHandler for Deactivation {
    fn deactivate_accessibility(&mut self) {}
}

/// Private third-party bridge. Must be dropped before its window.
pub(crate) struct AccessibilityAdapter {
    adapter: accesskit_winit::Adapter,
    state: Arc<Mutex<State>>,
}

impl AccessibilityAdapter {
    /// Construct on the event-loop thread before the window is first shown.
    pub(crate) fn new(
        event_loop: &winit::event_loop::ActiveEventLoop,
        window: &winit::window::Window,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let state = Arc::new(Mutex::new(State::new()));
        let adapter = accesskit_winit::Adapter::with_direct_handlers(
            event_loop,
            window,
            Activation(state.clone()),
            Actions {
                state: state.clone(),
                wake,
            },
            Deactivation,
        );
        Self { adapter, state }
    }

    pub(crate) fn process_event(
        &mut self,
        window: &winit::window::Window,
        event: &winit::event::WindowEvent,
    ) {
        self.adapter.process_event(window, event);
    }

    pub(crate) fn commit(
        &mut self,
        generation: u64,
        scene: &SemanticScene,
        geometry: AccessibilityGeometry,
    ) -> Result<(), AccessibilityError> {
        let tree = {
            let mut state = self.state.lock().expect("accessibility state poisoned");
            state.commit(generation, scene, geometry)?;
            state.tree.clone()
        };
        self.adapter.update_if_active(|| tree);
        Ok(())
    }

    /// Clear authority on surface loss or uncommitted geometry change.
    pub(crate) fn invalidate(&mut self) {
        let tree = {
            let mut state = self.state.lock().expect("accessibility state poisoned");
            state.invalidate();
            state.tree.clone()
        };
        self.adapter.update_if_active(|| tree);
    }

    pub(crate) fn take_actions(&mut self) -> Result<Vec<AccessibilityAction>, AccessibilityError> {
        let mut state = self.state.lock().expect("accessibility state poisoned");
        if state.overflow {
            state.queue.clear();
            state.overflow = false;
            return Err(AccessibilityError::ActionQueueOverflow);
        }
        Ok(state.queue.drain(..).collect())
    }
}

impl Drop for AccessibilityAdapter {
    fn drop(&mut self) {
        // Revoke callback authority before the platform adapter tears down its
        // async accessibility service and releases its shared state handles.
        if let Ok(mut state) = self.state.lock() {
            state.invalidate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::semantic::{SemanticAccessibility, SemanticRegion};

    fn geometry() -> AccessibilityGeometry {
        AccessibilityGeometry {
            clip: Rect::new(2, 1, 8, 4),
            origin_x: 3.0,
            origin_y: 5.0,
            cell_width: 8.0,
            cell_height: 16.0,
        }
    }

    fn scene(sensitive: bool) -> SemanticScene {
        let mut scene = SemanticScene::new();
        let mut region = SemanticRegion::new("stable", Rect::new(0, 0, 5, 3), "button");
        region.accessibility = Some(SemanticAccessibility {
            label: Some("private name".into()),
            description: Some("private description".into()),
            text: Some("private text".into()),
            sensitive,
            focused: true,
            actions: vec![SemanticAction::Activate],
            ..Default::default()
        });
        scene.push(region);
        scene.push(SemanticRegion::new(
            "offscreen",
            Rect::new(30, 30, 4, 2),
            "button",
        ));
        scene
    }

    fn click(id: NodeId) -> ActionRequest {
        ActionRequest {
            action: Action::Click,
            target_tree: TreeId::ROOT,
            target_node: id,
            data: None,
        }
    }

    #[test]
    fn actions_for_another_tree_do_not_target_root_nodes() {
        let mut state = State::new();
        state.commit(1, &scene(false), geometry()).unwrap();
        assert_eq!(state.tree.tree_id, TreeId::ROOT);
        let id = state.tree.nodes[0].0;
        let mut request = click(id);
        request.target_tree = TreeId(accesskit::Uuid::from_u128(1));
        state.action(request);
        assert_eq!(state.queue.len(), 0);
        state.action(click(id));
        assert_eq!(state.queue.len(), 1);
    }

    #[test]
    fn projection_clips_using_committed_physical_geometry() {
        let mut state = State::new();
        state.commit(1, &scene(false), geometry()).unwrap();
        assert_eq!(state.tree.nodes.len(), 2);
        let node = &state.tree.nodes[0].1;
        assert_eq!(
            node.bounds(),
            Some(accesskit::Rect::new(19.0, 21.0, 43.0, 53.0))
        );
        assert_eq!(node.label(), Some("private name"));
        assert_ne!(state.tree.focus, ROOT);
    }

    #[test]
    fn sensitive_content_never_enters_platform_tree() {
        let mut state = State::new();
        state.commit(1, &scene(true), geometry()).unwrap();
        let node = &state.tree.nodes[0].1;
        assert_eq!(node.label(), None);
        assert_eq!(node.description(), None);
        assert_eq!(node.value(), None);
    }

    #[test]
    fn delayed_actions_cannot_acquire_new_frame_authority() {
        let mut state = State::new();
        state.commit(1, &scene(false), geometry()).unwrap();
        let old = state.tree.nodes[0].0;
        state.action(click(old));
        assert_eq!(state.queue.len(), 1);
        state.commit(2, &scene(false), geometry()).unwrap();
        assert!(state.queue.is_empty());
        state.action(click(old));
        assert!(state.queue.is_empty());
        let current = state.tree.nodes[0].0;
        state.action(click(current));
        assert_eq!(
            state.queue[0],
            AccessibilityAction {
                generation: 2,
                semantic_id: "stable".into(),
                action: SemanticAction::Activate
            }
        );
    }

    #[test]
    fn invalidation_revokes_retained_and_queued_callback_authority() {
        let mut state = State::new();
        state.commit(1, &scene(false), geometry()).unwrap();
        let old = state.tree.nodes[0].0;
        state.action(click(old));
        state.invalidate();
        state.action(click(old));
        assert!(state.queue.is_empty());
        assert!(state.targets.is_empty());
        assert_eq!(state.tree.focus, ROOT);
        assert_eq!(state.tree.nodes.len(), 1);
        assert_eq!(state.generation, 1);
        assert_eq!(
            state.commit(1, &scene(false), geometry()),
            Err(AccessibilityError::StaleGeneration)
        );
        state.commit(2, &scene(false), geometry()).unwrap();
        assert_ne!(state.tree.nodes[0].0, old);
    }

    #[test]
    fn rejected_projection_does_not_advance_authority() {
        let mut state = State::new();
        state.commit(1, &scene(false), geometry()).unwrap();
        let prior = state.tree.clone();
        assert_eq!(
            state.commit(1, &scene(false), geometry()),
            Err(AccessibilityError::StaleGeneration)
        );
        let mut invalid = geometry();
        invalid.cell_width = f64::NAN;
        assert_eq!(
            state.commit(2, &scene(false), invalid),
            Err(AccessibilityError::InvalidGeometry)
        );
        let mut duplicate = scene(false);
        duplicate.push(duplicate.regions()[0].clone());
        assert_eq!(
            state.commit(2, &duplicate, geometry()),
            Err(AccessibilityError::DuplicateIdentity)
        );
        assert_eq!(state.tree, prior);
        assert_eq!(state.generation, 1);
    }

    #[test]
    fn disabled_and_unadvertised_actions_are_rejected_and_queue_is_bounded() {
        let mut state = State::new();
        let mut scene = scene(false);
        let mut disabled = scene.regions()[0].clone();
        disabled.id = "disabled".into();
        disabled.accessibility.as_mut().unwrap().disabled = true;
        scene.push(disabled);
        state.commit(1, &scene, geometry()).unwrap();
        let enabled = state.tree.nodes[0].0;
        let disabled = state.tree.nodes[1].0;
        state.action(click(disabled));
        state.action(ActionRequest {
            action: Action::Focus,
            target_tree: TreeId::ROOT,
            target_node: enabled,
            data: None,
        });
        assert!(state.queue.is_empty());
        for _ in 0..MAX_ACTIONS + 1 {
            state.action(click(enabled));
        }
        assert_eq!(state.queue.len(), MAX_ACTIONS);
        assert!(state.overflow);
    }
}
