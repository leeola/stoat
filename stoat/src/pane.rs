use crate::{editor_state::EditorId, jumplist::JumpList, run::RunId, term_session::TermId};
use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};
use slotmap::{new_key_type, SlotMap};

new_key_type! {
    pub struct PaneId;
    pub struct DockId;
    pub(crate) struct NodeId;
}

/// Minimum cells a pane keeps on either side of a divider being dragged, so a
/// resize can never collapse a neighbor to nothing.
const MIN_PANE_EXTENT: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Axis {
    /// Children stacked top-to-bottom (horizontal divider line).
    Horizontal,
    /// Children side-by-side left-to-right (vertical divider line).
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DividerOrientation {
    Vertical,
    Horizontal,
}

/// A single-line segment painted in the 1-cell gap between two adjacent
/// children of a [`Split`]. `(x, y)` is the top-left; for a
/// [`DividerOrientation::Vertical`] segment the line extends downward for
/// `len` rows, for [`DividerOrientation::Horizontal`] it extends rightward
/// for `len` columns. `touches_focus` is true when the focused pane lives
/// on either side of this gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Divider {
    pub orientation: DividerOrientation,
    pub x: u16,
    pub y: u16,
    pub len: u16,
    pub touches_focus: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum View {
    Label(String),
    Editor(EditorId),
    Run(RunId),
    Agent(TermId),
    Terminal(TermId),
}

/// How a pane is presented on screen.
///
/// Only [`Placement::Split`] is implemented now. The other variants establish
/// the type structure for future modal, floating status, and docked panes
/// without requiring a redesign.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Placement {
    /// Managed by the split tree layout.
    Split,
    /// Centered overlay that blocks input to panes underneath.
    Modal,
    /// Small status-oriented overlay (terminal output, chat status, progress)
    /// pinned to a corner or edge. Does not block input to split panes.
    Float,
    /// Pinned to a window edge.
    Dock,
    /// Exists but not rendered.
    Hidden,
    /// Detached into stoatty aux window `N`, outside the split tree. The pane
    /// stays in the slotmap and paints into its own OS window. Its `area` holds
    /// window-relative coordinates rather than a slot in the split layout.
    Window(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DockSide {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DockVisibility {
    Open { width: u16 },
    Minimized,
    Hidden,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DockPanel {
    pub view: View,
    pub side: DockSide,
    pub visibility: DockVisibility,
    pub default_width: u16,
    #[serde(skip)]
    pub area: Rect,
}

impl DockPanel {
    pub fn effective_width(&self) -> u16 {
        match self.visibility {
            DockVisibility::Open { width } => width,
            DockVisibility::Minimized => 1,
            DockVisibility::Hidden => 0,
        }
    }
}

/// Which of a workspace's focusable panels holds focus.
///
/// `SplitPane` is a unit variant on purpose. The focused split pane is always
/// [`PaneTree::focus`], so carrying a `PaneId` here would just be a second copy
/// that no pane-close path updates, leaving a dangling key that panics on the
/// next lookup. Resolve the live pane through [`PaneTree::focus`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusTarget {
    SplitPane,
    Dock(DockId),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Pane {
    pub view: View,
    /// View this pane showed before a terminal replaced it, restored when
    /// that terminal exits in the last split pane (which cannot close).
    ///
    /// `serde(skip)`: a restored terminal respawns as a fresh shell, so a
    /// restored session has no meaningful prior view to return to.
    #[serde(skip)]
    pub prev_view: Option<View>,
    pub placement: Placement,
    #[serde(skip)]
    pub area: Rect,
    pub index: u32,
    /// Cross-buffer jump history for this pane, surviving the `EditorState`
    /// swaps a cross-file open performs.
    ///
    /// `serde(skip)`: navigation scratch rather than persisted layout, so a
    /// restored session starts with an empty history.
    #[serde(skip)]
    pub(crate) jumplist: JumpList,
}

#[derive(Debug, Serialize, Deserialize)]
struct Split {
    axis: Axis,
    children: Vec<NodeId>,
    /// Per-child fractions of the split's usable extent, one entry per child.
    /// Empty (the default) means divide evenly. A membership change clears it
    /// back to even. Rides serde so a resized layout survives restart. Old
    /// sessions without the field deserialize to empty.
    #[serde(default)]
    weights: Vec<f32>,
    #[serde(skip)]
    area: Rect,
}

#[derive(Debug, Serialize, Deserialize)]
enum NodeContent {
    Leaf(PaneId),
    Split(Split),
}

#[derive(Debug, Serialize, Deserialize)]
struct Node {
    parent: NodeId,
    content: NodeContent,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PaneTree {
    root: NodeId,
    focus: PaneId,
    #[serde(skip)]
    area: Rect,
    panes: SlotMap<PaneId, Pane>,
    nodes: SlotMap<NodeId, Node>,
    next_index: u32,
    #[serde(skip)]
    stack: Vec<(NodeId, Rect)>,
}

impl PaneTree {
    pub fn new(area: Rect) -> Self {
        let mut panes = SlotMap::with_key();
        let mut nodes = SlotMap::with_key();

        let pane_id = panes.insert(Pane {
            view: View::Label("Pane 0".into()),
            prev_view: None,
            placement: Placement::Split,
            area,
            index: 0,
            jumplist: JumpList::default(),
        });

        let root_id = nodes.insert(Node {
            parent: NodeId::default(),
            content: NodeContent::Leaf(pane_id),
        });
        nodes[root_id].parent = root_id;

        Self {
            root: root_id,
            focus: pane_id,
            area,
            panes,
            nodes,
            next_index: 1,
            stack: Vec::new(),
        }
    }

    pub fn focus(&self) -> PaneId {
        self.focus
    }

    pub fn set_focus(&mut self, id: PaneId) {
        if self.panes.contains_key(id) {
            self.focus = id;
        }
    }

    pub fn pane(&self, id: PaneId) -> &Pane {
        &self.panes[id]
    }

    pub fn pane_mut(&mut self, id: PaneId) -> &mut Pane {
        &mut self.panes[id]
    }

    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    /// Whether `id` still names a live pane, for callers holding a `PaneId`
    /// across an await that a split or close could invalidate.
    pub fn contains(&self, id: PaneId) -> bool {
        self.panes.contains_key(id)
    }

    /// Splits the focused pane along `axis`, creating a new pane.
    ///
    /// If the parent split has the same axis, the new pane is inserted adjacent.
    /// Otherwise a new nested split is created. Focus moves to the new pane.
    pub fn split(&mut self, axis: Axis) -> PaneId {
        let focused_view = self.panes[self.focus_anchor()].view.clone();
        let new_pane_id = self.panes.insert(Pane {
            view: focused_view,
            prev_view: None,
            placement: Placement::Split,
            area: Rect::default(),
            index: self.next_index,
            jumplist: JumpList::default(),
        });
        self.next_index += 1;

        self.insert_pane_leaf(new_pane_id, axis);
        self.focus = new_pane_id;
        self.recalculate();
        new_pane_id
    }

    /// Inserts `pane_id` as a new leaf beside the focused split pane along
    /// `axis`, reusing the focused pane's parent split when its axis matches or
    /// nesting a fresh split otherwise. Anchors through [`Self::focus_anchor`] so
    /// it holds even when focus sits on a windowed pane. Neither moves focus nor
    /// recalculates, both of which the caller does.
    fn insert_pane_leaf(&mut self, pane_id: PaneId, axis: Axis) {
        let focused_node = self.node_for_pane(self.focus_anchor());

        let new_leaf = self.nodes.insert(Node {
            parent: NodeId::default(),
            content: NodeContent::Leaf(pane_id),
        });

        let parent_id = self.nodes[focused_node].parent;

        let insert_adjacent = if parent_id != focused_node {
            if let NodeContent::Split(split) = &self.nodes[parent_id].content {
                split.axis == axis
            } else {
                false
            }
        } else {
            false
        };

        if insert_adjacent {
            let pos = if let NodeContent::Split(split) = &self.nodes[parent_id].content {
                split
                    .children
                    .iter()
                    .position(|&c| c == focused_node)
                    .expect("focused node not found in parent")
                    + 1
            } else {
                unreachable!()
            };

            if let NodeContent::Split(split) = &mut self.nodes[parent_id].content {
                split.children.insert(pos, new_leaf);
                split.weights.clear();
            }
            self.nodes[new_leaf].parent = parent_id;
        } else {
            let split_node_id = self.nodes.insert(Node {
                parent: NodeId::default(),
                content: NodeContent::Split(Split {
                    axis,
                    children: vec![focused_node, new_leaf],
                    weights: Vec::new(),
                    area: Rect::default(),
                }),
            });

            self.nodes[focused_node].parent = split_node_id;
            self.nodes[new_leaf].parent = split_node_id;

            if parent_id == focused_node {
                // focused_node was the root
                self.nodes[split_node_id].parent = split_node_id;
                self.root = split_node_id;
            } else {
                self.nodes[split_node_id].parent = parent_id;
                if let NodeContent::Split(split) = &mut self.nodes[parent_id].content {
                    let pos = split
                        .children
                        .iter()
                        .position(|&c| c == focused_node)
                        .expect("focused node not found in parent");
                    split.children[pos] = split_node_id;
                }
            }
        }
    }

    /// Removes a pane. Returns `false` if it's the last split pane.
    ///
    /// Collapses single-child splits. Moves focus to a neighbor if the
    /// closed pane was focused.
    pub fn close(&mut self, id: PaneId) -> bool {
        if self.split_pane_count() <= 1 {
            return false;
        }

        let Some(node_id) = self.leaf_node(id) else {
            return false;
        };

        if self.focus == id {
            self.focus = self.next_split_pane(id);
        }

        self.detach_node(node_id);
        self.panes.remove(id);
        self.recalculate();
        true
    }

    /// Detaches the pane into stoatty aux window `window`, keeping it in the
    /// slotmap outside the split tree. Returns `false` when it is the last split
    /// pane or has no tree node.
    ///
    /// Only the pane's placement and its standing in the tree change. Its backing
    /// view state is untouched, so a later [`Self::attach`] restores it. Focus
    /// moves to a neighbor split pane, matching [`Self::close`], so the tree keeps
    /// a valid focus. The caller may then refocus the detached pane.
    pub fn detach(&mut self, id: PaneId, window: u32) -> bool {
        if self.split_pane_count() <= 1 {
            return false;
        }

        let Some(node_id) = self.leaf_node(id) else {
            return false;
        };

        if self.focus == id {
            self.focus = self.next_split_pane(id);
        }

        self.detach_node(node_id);
        self.panes[id].placement = Placement::Window(window);
        self.recalculate();
        true
    }

    /// Reattaches windowed pane `id` into the split tree as a leaf beside the
    /// current split focus, resetting its placement to [`Placement::Split`] and
    /// focusing it.
    ///
    /// The reattached pane has no tree node, so the insertion anchors on the
    /// focused split pane, or on any split pane when focus is the windowed pane
    /// itself (see [`Self::focus_anchor`]). Inserts along [`Axis::Vertical`].
    pub fn attach(&mut self, id: PaneId) {
        self.panes[id].placement = Placement::Split;
        self.insert_pane_leaf(id, Axis::Vertical);
        self.focus = id;
        self.recalculate();
    }

    /// Removes leaf `node_id` from the tree and collapses a resulting
    /// single-child split into its parent, leaving the backing pane in the
    /// slotmap for the caller to remove or repurpose.
    fn detach_node(&mut self, node_id: NodeId) {
        let parent_id = self.nodes[node_id].parent;
        self.nodes.remove(node_id);

        if let NodeContent::Split(split) = &mut self.nodes[parent_id].content {
            split.children.retain(|&c| c != node_id);
            split.weights.clear();

            if split.children.len() == 1 && parent_id != self.root {
                let remaining = split.children[0];
                let grandparent = self.nodes[parent_id].parent;

                if let NodeContent::Split(gp_split) = &mut self.nodes[grandparent].content {
                    let pos = gp_split
                        .children
                        .iter()
                        .position(|&c| c == parent_id)
                        .expect("parent not found in grandparent");
                    gp_split.children[pos] = remaining;
                }
                self.nodes[remaining].parent = grandparent;
                self.nodes.remove(parent_id);
            } else if split.children.len() == 1 && parent_id == self.root {
                let remaining = split.children[0];
                self.nodes[remaining].parent = remaining;
                self.root = remaining;
                self.nodes.remove(parent_id);
            }
        }
    }

    pub fn resize(&mut self, area: Rect) {
        self.area = area;
        self.recalculate();
    }

    /// Moves focus to the adjacent split pane in `direction`.
    /// Returns whether focus actually changed.
    pub fn focus_direction(&mut self, direction: Direction) -> bool {
        let current_node = self.node_for_pane(self.focus_anchor());
        if let Some(target) = self.find_split_in_direction(current_node, direction)
            && let NodeContent::Leaf(pane_id) = self.nodes[target].content
            && pane_id != self.focus
        {
            self.focus = pane_id;
            return true;
        }
        false
    }

    pub fn focus_next(&mut self) {
        self.focus = self.next_split_pane(self.focus);
    }

    pub fn focus_prev(&mut self) {
        self.focus = self.prev_split_pane(self.focus);
    }

    pub fn split_panes(&self) -> Traverse<'_> {
        Traverse {
            tree: self,
            stack: vec![self.root],
        }
    }

    /// Collect the ids of every leaf pane. Convenience for iteration that needs
    /// mutable access to panes (via [`Self::pane_mut`]) without holding a
    /// borrow of `self` across the loop.
    pub fn split_pane_ids(&self) -> Vec<PaneId> {
        self.split_panes().map(|(id, _)| id).collect()
    }

    /// Every detached pane paired with the aux window it renders into, ordered by
    /// [`Pane::index`] so window assignment is stable across calls.
    pub fn windowed_panes(&self) -> Vec<(PaneId, u32)> {
        let mut out: Vec<(PaneId, u32, u32)> = self
            .panes
            .iter()
            .filter_map(|(id, pane)| match pane.placement {
                Placement::Window(window) => Some((id, window, pane.index)),
                _ => None,
            })
            .collect();
        out.sort_by_key(|&(_, _, index)| index);
        out.into_iter()
            .map(|(id, window, _)| (id, window))
            .collect()
    }

    /// The panes numeric selection and pane-ID badges address, split panes in
    /// traversal order first, then detached panes in [`Self::windowed_panes`]
    /// order. This ordering is the badge digit sequence.
    pub fn selectable_pane_ids(&self) -> Vec<PaneId> {
        let mut ids = self.split_pane_ids();
        ids.extend(self.windowed_panes().into_iter().map(|(id, _)| id));
        ids
    }

    /// Enumerate the 1-cell gap segments that sit between adjacent children
    /// of every [`Split`] in the tree. [`PaneTree::recalculate`] already
    /// reserves these gaps; this walk emits the line segment that should be
    /// painted in each one. `touches_focus` is set when either of the
    /// subtrees flanking the gap contains the currently focused leaf.
    pub fn dividers(&self) -> Vec<Divider> {
        let focus = self.focus;
        let mut out = Vec::new();
        let mut stack = vec![self.root];
        while let Some(nid) = stack.pop() {
            if let NodeContent::Split(split) = &self.nodes[nid].content {
                stack.extend(split.children.iter().copied());
                let children: Vec<(NodeId, Rect)> = split
                    .children
                    .iter()
                    .map(|&c| (c, self.node_area(c)))
                    .collect();
                for pair in children.windows(2) {
                    let (left_id, left_rect) = pair[0];
                    let (right_id, _right_rect) = pair[1];
                    let touches_focus = self.subtree_contains(left_id, focus)
                        || self.subtree_contains(right_id, focus);
                    match split.axis {
                        Axis::Vertical => out.push(Divider {
                            orientation: DividerOrientation::Vertical,
                            x: left_rect.x + left_rect.width,
                            y: split.area.y,
                            len: split.area.height,
                            touches_focus,
                        }),
                        Axis::Horizontal => out.push(Divider {
                            orientation: DividerOrientation::Horizontal,
                            x: split.area.x,
                            y: left_rect.y + left_rect.height,
                            len: split.area.width,
                            touches_focus,
                        }),
                    }
                }
            }
        }
        out
    }

    fn subtree_contains(&self, node: NodeId, target: PaneId) -> bool {
        match &self.nodes[node].content {
            NodeContent::Leaf(pid) => *pid == target,
            NodeContent::Split(split) => split
                .children
                .iter()
                .any(|&c| self.subtree_contains(c, target)),
        }
    }

    fn node_area(&self, node: NodeId) -> Rect {
        match &self.nodes[node].content {
            NodeContent::Leaf(pid) => self.panes[*pid].area,
            NodeContent::Split(split) => split.area,
        }
    }

    /// The split node and gap index whose divider segment covers cell
    /// `(col, row)`, or `None` when the point is not on a divider.
    ///
    /// The gap index is the position between two adjacent children, so it pairs
    /// with [`Self::set_divider`]. Mirrors the segment geometry [`Self::dividers`]
    /// paints.
    pub(crate) fn divider_at(&self, col: u16, row: u16) -> Option<(NodeId, usize)> {
        let mut stack = vec![self.root];
        while let Some(nid) = stack.pop() {
            let NodeContent::Split(split) = &self.nodes[nid].content else {
                continue;
            };
            stack.extend(split.children.iter().copied());
            for (gap, pair) in split.children.windows(2).enumerate() {
                let left = self.node_area(pair[0]);
                let hit = match split.axis {
                    Axis::Vertical => {
                        col == left.x + left.width
                            && (split.area.y..split.area.y + split.area.height).contains(&row)
                    },
                    Axis::Horizontal => {
                        row == left.y + left.height
                            && (split.area.x..split.area.x + split.area.width).contains(&col)
                    },
                };
                if hit {
                    return Some((nid, gap));
                }
            }
        }
        None
    }

    /// Move the divider at `gap` in split `node` to screen position `(col, row)`,
    /// resizing the two children it separates.
    ///
    /// A vertical split reads `col`, a horizontal one `row`. The two flanking
    /// children each keep at least [`MIN_PANE_EXTENT`] cells and the rest of the
    /// split is untouched. A no-op when `node` is not a split, `gap` names no
    /// divider, or the pair is already too small to divide.
    pub(crate) fn set_divider(&mut self, node: NodeId, gap: usize, col: u16, row: u16) {
        let (axis, children) = match &self.nodes[node].content {
            NodeContent::Split(split) => (split.axis, split.children.clone()),
            NodeContent::Leaf(_) => return,
        };
        if gap + 1 >= children.len() {
            return;
        }

        let extents: Vec<u16> = children
            .iter()
            .map(|&c| {
                let r = self.node_area(c);
                match axis {
                    Axis::Vertical => r.width,
                    Axis::Horizontal => r.height,
                }
            })
            .collect();
        let (origin, target) = match axis {
            Axis::Vertical => (self.node_area(children[gap]).x, col),
            Axis::Horizontal => (self.node_area(children[gap]).y, row),
        };

        let combined = extents[gap] + extents[gap + 1];
        if combined < 2 * MIN_PANE_EXTENT {
            return;
        }

        let new_lead = target
            .saturating_sub(origin)
            .clamp(MIN_PANE_EXTENT, combined - MIN_PANE_EXTENT);

        let mut weights: Vec<f32> = extents.iter().map(|&e| f32::from(e)).collect();
        weights[gap] = f32::from(new_lead);
        weights[gap + 1] = f32::from(combined - new_lead);

        if let NodeContent::Split(split) = &mut self.nodes[node].content {
            split.weights = weights;
        }
        self.recalculate();
    }

    fn split_pane_count(&self) -> usize {
        self.panes
            .values()
            .filter(|p| p.placement == Placement::Split)
            .count()
    }

    fn node_for_pane(&self, pane_id: PaneId) -> NodeId {
        self.leaf_node(pane_id).expect("pane not found in tree")
    }

    /// The tree node holding leaf pane `id`, or `None` when the pane has no node
    /// (a windowed pane, or an unknown id).
    fn leaf_node(&self, id: PaneId) -> Option<NodeId> {
        self.nodes
            .iter()
            .find_map(|(nid, node)| match &node.content {
                NodeContent::Leaf(pid) if *pid == id => Some(nid),
                _ => None,
            })
    }

    /// The focused pane when it is a live split leaf, otherwise any split pane.
    ///
    /// Detaching leaves focus on a windowed pane that has no tree node, so
    /// operations anchored on the focus resolve through this to act on a real
    /// leaf rather than panicking. There is always at least one split pane, since
    /// [`Self::detach`] refuses to windowize the last one.
    fn focus_anchor(&self) -> PaneId {
        if self.leaf_node(self.focus).is_some() {
            self.focus
        } else {
            self.split_panes()
                .next()
                .map(|(id, _)| id)
                .unwrap_or(self.focus)
        }
    }

    fn next_split_pane(&self, current: PaneId) -> PaneId {
        let mut iter = self.split_panes().skip_while(|(id, _)| *id != current);
        iter.next(); // skip current
        if let Some((id, _)) = iter.next() {
            id
        } else {
            self.split_panes()
                .next()
                .map(|(id, _)| id)
                .unwrap_or(current)
        }
    }

    fn prev_split_pane(&self, current: PaneId) -> PaneId {
        let panes: Vec<_> = self.split_panes().map(|(id, _)| id).collect();
        let pos = panes.iter().position(|&id| id == current).unwrap_or(0);
        if pos == 0 {
            *panes.last().unwrap_or(&current)
        } else {
            panes[pos - 1]
        }
    }

    fn recalculate(&mut self) {
        self.stack.clear();
        self.stack.push((self.root, self.area));

        while let Some((node_id, area)) = self.stack.pop() {
            let node = &mut self.nodes[node_id];

            match &mut node.content {
                NodeContent::Leaf(pane_id) => {
                    self.panes[*pane_id].area = area;
                },
                NodeContent::Split(split) => {
                    split.area = area;
                    let len = split.children.len();
                    if len == 0 {
                        continue;
                    }

                    match split.axis {
                        Axis::Horizontal => {
                            let gap = 1u16;
                            let total_gap = gap.saturating_mul(len.saturating_sub(1) as u16);
                            let usable = area.height.saturating_sub(total_gap);
                            let extents = child_extents(&split.weights, len, usable);
                            let mut y = area.y;

                            for (&child_id, h) in split.children.iter().zip(extents) {
                                self.stack
                                    .push((child_id, Rect::new(area.x, y, area.width, h)));
                                y += h + gap;
                            }
                        },
                        Axis::Vertical => {
                            let gap = 1u16;
                            let total_gap = gap.saturating_mul(len.saturating_sub(1) as u16);
                            let usable = area.width.saturating_sub(total_gap);
                            let extents = child_extents(&split.weights, len, usable);
                            let mut x = area.x;

                            for (&child_id, w) in split.children.iter().zip(extents) {
                                self.stack
                                    .push((child_id, Rect::new(x, area.y, w, area.height)));
                                x += w + gap;
                            }
                        },
                    }
                },
            }
        }
    }

    fn find_split_in_direction(&self, id: NodeId, direction: Direction) -> Option<NodeId> {
        let parent_id = self.nodes[id].parent;
        if parent_id == id {
            return None;
        }

        let parent_split = match &self.nodes[parent_id].content {
            NodeContent::Split(s) => s,
            _ => unreachable!(),
        };

        let dominated = matches!(
            (direction, parent_split.axis),
            (Direction::Up, Axis::Vertical)
                | (Direction::Down, Axis::Vertical)
                | (Direction::Left, Axis::Horizontal)
                | (Direction::Right, Axis::Horizontal)
        );

        if dominated {
            return self.find_split_in_direction(parent_id, direction);
        }

        match self.find_child(id, &parent_split.children, direction) {
            Some(target) => Some(target),
            None => self.find_split_in_direction(parent_id, direction),
        }
    }

    fn find_child(&self, id: NodeId, children: &[NodeId], direction: Direction) -> Option<NodeId> {
        let mut child_id = match direction {
            Direction::Up | Direction::Left => children
                .iter()
                .rev()
                .skip_while(|&&c| c != id)
                .copied()
                .nth(1)?,
            Direction::Down | Direction::Right => {
                children.iter().skip_while(|&&c| c != id).copied().nth(1)?
            },
        };

        let focus_area = self.panes[self.focus].area;

        // Descend into containers to find the closest leaf
        loop {
            match &self.nodes[child_id].content {
                NodeContent::Leaf(_) => return Some(child_id),
                NodeContent::Split(split) => {
                    child_id = match split.axis {
                        Axis::Vertical => *split.children.iter().min_by_key(|&&nid| {
                            let x = self.node_left(nid);
                            (focus_area.x as i32 - x as i32).unsigned_abs()
                        })?,
                        Axis::Horizontal => *split.children.iter().min_by_key(|&&nid| {
                            let y = self.node_top(nid);
                            (focus_area.y as i32 - y as i32).unsigned_abs()
                        })?,
                    };
                },
            }
        }
    }

    fn node_left(&self, id: NodeId) -> u16 {
        match &self.nodes[id].content {
            NodeContent::Leaf(pid) => self.panes[*pid].area.x,
            NodeContent::Split(s) => s.area.x,
        }
    }

    fn node_top(&self, id: NodeId) -> u16 {
        match &self.nodes[id].content {
            NodeContent::Leaf(pid) => self.panes[*pid].area.y,
            NodeContent::Split(s) => s.area.y,
        }
    }
}

/// Per-child extents (summing to `usable`) for a split of `len` children.
///
/// Uses `weights` as fractions when it has one finite, positive entry per
/// child. Otherwise divides evenly. The last child absorbs any rounding
/// remainder so the extents always sum to `usable` exactly.
fn child_extents(weights: &[f32], len: usize, usable: u16) -> Vec<u16> {
    if len == 0 {
        return Vec::new();
    }

    let weighted = weights.len() == len && weights.iter().all(|w| w.is_finite() && *w > 0.0);
    if !weighted {
        let per = usable / len as u16;
        let mut extents = vec![per; len];
        extents[len - 1] = usable.saturating_sub(per.saturating_mul(len as u16 - 1));
        return extents;
    }

    let sum: f32 = weights.iter().sum();
    let mut extents = Vec::with_capacity(len);
    let mut placed = 0u16;
    let mut cumulative = 0.0f32;
    for (i, &w) in weights.iter().enumerate() {
        if i == len - 1 {
            extents.push(usable.saturating_sub(placed));
        } else {
            cumulative += w;
            let boundary = ((cumulative / sum) * f32::from(usable)).round() as u16;
            let extent = boundary.saturating_sub(placed);
            extents.push(extent);
            placed += extent;
        }
    }
    extents
}

pub struct Traverse<'a> {
    tree: &'a PaneTree,
    stack: Vec<NodeId>,
}

impl<'a> Iterator for Traverse<'a> {
    type Item = (PaneId, &'a Pane);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let node_id = self.stack.pop()?;
            match &self.tree.nodes[node_id].content {
                NodeContent::Leaf(pane_id) => {
                    return Some((*pane_id, &self.tree.panes[*pane_id]));
                },
                NodeContent::Split(split) => {
                    self.stack.extend(split.children.iter().rev());
                },
            }
        }
    }
}

impl DoubleEndedIterator for Traverse<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        loop {
            let node_id = self.stack.pop()?;
            match &self.tree.nodes[node_id].content {
                NodeContent::Leaf(pane_id) => {
                    return Some((*pane_id, &self.tree.panes[*pane_id]));
                },
                NodeContent::Split(split) => {
                    self.stack.extend(split.children.iter());
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 120, 40)
    }

    #[test]
    fn new_tree_has_one_pane() {
        let tree = PaneTree::new(area());
        assert_eq!(tree.pane_count(), 1);
        assert_eq!(tree.pane(tree.focus()).placement, Placement::Split);
        assert_eq!(tree.pane(tree.focus()).area, area());
    }

    #[test]
    fn split_vertical() {
        let mut tree = PaneTree::new(area());
        let first = tree.focus();
        let second = tree.split(Axis::Vertical);

        assert_eq!(tree.pane_count(), 2);
        assert_eq!(tree.focus(), second);

        let first_area = tree.pane(first).area;
        let second_area = tree.pane(second).area;

        // Side by side, 1-cell gap
        assert_eq!(first_area.x, 0);
        assert_eq!(first_area.height, 40);
        assert_eq!(second_area.height, 40);
        assert_eq!(first_area.width + 1 + second_area.width, 120);
        assert_eq!(second_area.x, first_area.width + 1);
    }

    #[test]
    fn split_horizontal() {
        let mut tree = PaneTree::new(area());
        let first = tree.focus();
        let second = tree.split(Axis::Horizontal);

        assert_eq!(tree.pane_count(), 2);

        let first_area = tree.pane(first).area;
        let second_area = tree.pane(second).area;

        // Stacked, 1-cell gap
        assert_eq!(first_area.y, 0);
        assert_eq!(first_area.width, 120);
        assert_eq!(second_area.width, 120);
        assert_eq!(first_area.height + 1 + second_area.height, 40);
        assert_eq!(second_area.y, first_area.height + 1);
    }

    #[test]
    fn split_same_axis_inserts_adjacent() {
        let mut tree = PaneTree::new(area());
        let a = tree.focus();
        tree.split(Axis::Vertical);
        let c = tree.split(Axis::Vertical);

        assert_eq!(tree.pane_count(), 3);

        // All three should be side-by-side (no nesting)
        let panes: Vec<_> = tree.split_panes().collect();
        assert_eq!(panes.len(), 3);

        let a_area = tree.pane(a).area;
        let c_area = tree.pane(c).area;
        assert_eq!(a_area.y, 0);
        assert_eq!(c_area.y, 0);
        assert_eq!(a_area.height, 40);
        assert_eq!(c_area.height, 40);
    }

    #[test]
    fn split_different_axis_nests() {
        let mut tree = PaneTree::new(area());
        let a = tree.focus();
        tree.split(Axis::Vertical);
        // Focus is on the right pane, now split it horizontally
        let c = tree.split(Axis::Horizontal);

        assert_eq!(tree.pane_count(), 3);

        let a_area = tree.pane(a).area;
        let c_area = tree.pane(c).area;

        // a takes the left half
        assert_eq!(a_area.x, 0);
        assert_eq!(a_area.height, 40);

        // c is in the bottom-right quadrant
        assert!(c_area.x > 0);
        assert!(c_area.y > 0);
    }

    #[test]
    fn close_pane() {
        let mut tree = PaneTree::new(area());
        let a = tree.focus();
        let b = tree.split(Axis::Vertical);

        assert_eq!(tree.pane_count(), 2);
        assert!(tree.close(b));
        assert_eq!(tree.pane_count(), 1);
        assert_eq!(tree.focus(), a);
        assert_eq!(tree.pane(a).area, area());
    }

    #[test]
    fn close_last_pane_refuses() {
        let mut tree = PaneTree::new(area());
        let a = tree.focus();
        assert!(!tree.close(a));
        assert_eq!(tree.pane_count(), 1);
    }

    #[test]
    fn close_collapses_single_child_split() {
        let mut tree = PaneTree::new(area());
        // Create: [a | [b / c]]
        let _a = tree.focus();
        tree.split(Axis::Vertical); // b
        let c = tree.split(Axis::Horizontal); // c (nested in right)

        assert_eq!(tree.pane_count(), 3);
        assert!(tree.close(c));
        assert_eq!(tree.pane_count(), 2);

        // Remaining two panes should tile the full area
        let panes: Vec<_> = tree.split_panes().collect();
        assert_eq!(panes.len(), 2);
        let total_width: u16 = panes.iter().map(|(_, p)| p.area.width).sum::<u16>() + 1; // +1 gap
        assert_eq!(total_width, 120);
    }

    #[test]
    fn detach_removes_leaf_keeps_pane() {
        let mut tree = PaneTree::new(area());
        let a = tree.focus();
        let b = tree.split(Axis::Vertical);

        assert!(tree.detach(b, 1));
        assert_eq!(tree.pane_count(), 2, "detached pane stays in the slotmap");
        assert_eq!(tree.split_pane_ids(), vec![a], "its leaf leaves the tree");
        assert_eq!(tree.pane(b).placement, Placement::Window(1));
        assert_eq!(tree.focus(), a, "focus moves to the remaining split pane");
    }

    #[test]
    fn attach_restores_at_focused_node() {
        let mut tree = PaneTree::new(area());
        let a = tree.focus();
        let b = tree.split(Axis::Vertical);
        tree.detach(b, 1);

        tree.attach(b);
        assert_eq!(tree.split_pane_ids().len(), 2);
        assert_eq!(tree.pane(b).placement, Placement::Split);
        assert!(tree.split_pane_ids().contains(&a));
        assert_eq!(tree.focus(), b, "attach focuses the reattached pane");
    }

    #[test]
    fn detach_last_split_pane_refuses() {
        let mut tree = PaneTree::new(area());
        let a = tree.focus();
        assert!(!tree.detach(a, 1));
        assert_eq!(tree.pane(a).placement, Placement::Split);
    }

    #[test]
    fn selectable_pane_ids_orders_split_then_windowed() {
        let mut tree = PaneTree::new(area());
        let a = tree.focus();
        let b = tree.split(Axis::Vertical);
        let c = tree.split(Axis::Vertical);

        assert!(tree.detach(c, 1));
        assert_eq!(tree.selectable_pane_ids(), vec![a, b, c]);
    }

    #[test]
    fn resize() {
        let mut tree = PaneTree::new(area());
        tree.split(Axis::Vertical);

        let new_area = Rect::new(0, 0, 200, 50);
        tree.resize(new_area);

        let panes: Vec<_> = tree.split_panes().collect();
        let total_width: u16 = panes.iter().map(|(_, p)| p.area.width).sum::<u16>() + 1;
        assert_eq!(total_width, 200);
        for (_, pane) in &panes {
            assert_eq!(pane.area.height, 50);
        }
    }

    #[test]
    fn focus_direction_left_right() {
        let mut tree = PaneTree::new(area());
        let a = tree.focus();
        let b = tree.split(Axis::Vertical);

        assert_eq!(tree.focus(), b);
        assert!(tree.focus_direction(Direction::Left));
        assert_eq!(tree.focus(), a);
        assert!(tree.focus_direction(Direction::Right));
        assert_eq!(tree.focus(), b);

        // No pane further right
        assert!(!tree.focus_direction(Direction::Right));
        assert_eq!(tree.focus(), b);
    }

    #[test]
    fn focus_direction_up_down() {
        let mut tree = PaneTree::new(area());
        let a = tree.focus();
        let b = tree.split(Axis::Horizontal);

        assert_eq!(tree.focus(), b);
        assert!(tree.focus_direction(Direction::Up));
        assert_eq!(tree.focus(), a);
        assert!(tree.focus_direction(Direction::Down));
        assert_eq!(tree.focus(), b);
    }

    #[test]
    fn focus_direction_nested() {
        let mut tree = PaneTree::new(area());
        let a = tree.focus();
        tree.split(Axis::Vertical);
        let c = tree.split(Axis::Horizontal);

        // c is bottom-right, navigate left to a
        assert_eq!(tree.focus(), c);
        assert!(tree.focus_direction(Direction::Left));
        assert_eq!(tree.focus(), a);
    }

    #[test]
    fn focus_next_prev_wraps() {
        let mut tree = PaneTree::new(area());
        let a = tree.focus();
        let b = tree.split(Axis::Vertical);
        let c = tree.split(Axis::Vertical);

        tree.set_focus(a);
        tree.focus_next();
        assert_eq!(tree.focus(), b);
        tree.focus_next();
        assert_eq!(tree.focus(), c);
        tree.focus_next();
        assert_eq!(tree.focus(), a); // wraps

        tree.focus_prev();
        assert_eq!(tree.focus(), c); // wraps back
    }

    #[test]
    fn rects_cover_full_area() {
        let mut tree = PaneTree::new(area());
        tree.split(Axis::Vertical);
        tree.set_focus(tree.split_panes().next().map(|(id, _)| id).unwrap());
        tree.split(Axis::Horizontal);

        let mut covered = vec![vec![false; 120]; 40];
        for (_, pane) in tree.split_panes() {
            let a = pane.area;
            for y in a.y..a.y + a.height {
                for x in a.x..a.x + a.width {
                    assert!(!covered[y as usize][x as usize], "overlap at ({x}, {y})");
                    covered[y as usize][x as usize] = true;
                }
            }
        }

        // Gaps (1-cell separators) are expected to not be covered.
        // Verify total covered + gaps = full area.
        let total_covered: usize = covered.iter().flatten().filter(|&&c| c).count();
        let total_pane_area: u16 = tree
            .split_panes()
            .map(|(_, p)| p.area.width * p.area.height)
            .sum();
        assert_eq!(total_covered, total_pane_area as usize);
    }

    #[test]
    fn pane_has_view_and_placement() {
        let tree = PaneTree::new(area());
        let pane = tree.pane(tree.focus());
        assert_eq!(pane.placement, Placement::Split);
        assert!(matches!(&pane.view, View::Label(s) if s == "Pane 0"));
    }

    #[test]
    fn snapshot_split_right() {
        let mut h = crate::Stoat::test();
        h.type_action("SplitRight()");
        h.assert_snapshot("split_right");
    }

    #[test]
    fn snapshot_split_down() {
        let mut h = crate::Stoat::test();
        h.type_action("SplitDown()");
        h.assert_snapshot("split_down");
    }

    #[test]
    fn snapshot_nested_splits() {
        let mut h = crate::Stoat::test();
        h.type_action("SplitRight()");
        h.type_action("SplitDown()");
        h.assert_snapshot("nested_splits");
    }

    #[test]
    fn snapshot_three_columns() {
        let mut h = crate::Stoat::test();
        h.type_action("SplitRight()");
        h.type_action("SplitRight()");
        h.assert_snapshot("three_columns");
    }

    #[test]
    fn snapshot_close_returns_to_single() {
        let mut h = crate::Stoat::test();
        h.type_action("SplitRight()");
        h.type_action("ClosePane()");
        h.assert_snapshot("close_returns_to_single");
    }

    #[test]
    fn snapshot_close_other_panes_from_three() {
        let mut h = crate::Stoat::test();
        h.type_action("SplitRight()");
        h.type_action("SplitRight()");
        h.type_action("CloseOtherPanes()");
        h.assert_snapshot("close_other_panes_from_three");
    }

    #[test]
    fn snapshot_quit_closes_one_split() {
        let mut h = crate::Stoat::test();
        h.type_action("SplitRight()");
        h.type_action("Quit()");
        h.assert_snapshot("quit_closes_one_split");
    }

    #[test]
    fn snapshot_split_right_focus_left() {
        let mut h = crate::Stoat::test();
        h.type_action("SplitRight()");
        h.type_action("FocusLeft()");
        h.assert_snapshot("split_right_focus_left");
    }

    #[test]
    fn set_divider_moves_the_boundary() {
        let mut tree = PaneTree::new(Rect::new(0, 0, 101, 40));
        let left = tree.focus();
        let right = tree.split(Axis::Vertical);
        assert_eq!(tree.pane(left).area.width, 50);
        assert_eq!(tree.pane(right).area.width, 50);

        tree.set_divider(tree.root, 0, 30, 0);
        assert_eq!(
            tree.pane(left).area.width,
            30,
            "left shrinks to the new column"
        );
        assert_eq!(tree.pane(right).area.width, 70, "right absorbs the rest");
        assert_eq!(
            tree.pane(right).area.x,
            31,
            "right starts after the divider gap"
        );
    }

    #[test]
    fn weighted_layout_scales_on_resize() {
        let mut tree = PaneTree::new(Rect::new(0, 0, 101, 40));
        let left = tree.focus();
        let right = tree.split(Axis::Vertical);
        tree.set_divider(tree.root, 0, 30, 0);

        tree.resize(Rect::new(0, 0, 201, 40));
        assert_eq!(tree.pane(left).area.width, 60, "left keeps its ~30% share");
        assert_eq!(
            tree.pane(right).area.width,
            140,
            "right keeps its ~70% share"
        );
    }

    #[test]
    fn set_divider_clamps_to_min_pane_extent() {
        let mut tree = PaneTree::new(Rect::new(0, 0, 101, 40));
        let left = tree.focus();
        let right = tree.split(Axis::Vertical);
        tree.set_divider(tree.root, 0, 0, 0);

        assert_eq!(tree.pane(left).area.width, 2, "left clamps to the minimum");
        assert_eq!(
            tree.pane(right).area.width,
            98,
            "right keeps the remaining space"
        );
    }

    #[test]
    fn membership_change_resets_weights_to_even() {
        let mut tree = PaneTree::new(Rect::new(0, 0, 101, 40));
        let left = tree.focus();
        let right = tree.split(Axis::Vertical);
        tree.set_divider(tree.root, 0, 30, 0);
        assert_eq!(tree.pane(left).area.width, 30);

        tree.set_focus(right);
        tree.split(Axis::Vertical);
        assert_eq!(
            tree.pane(left).area.width,
            33,
            "weights reset to even when a child is added"
        );
    }
}
