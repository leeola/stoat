use crate::{editor_state::EditorId, host::ClaudeSessionId, run::RunId};
use ratatui::layout::Rect;
use slotmap::{new_key_type, SlotMap};

new_key_type! {
    pub struct PaneId;
    pub struct DockId;
    struct NodeId;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Children stacked top-to-bottom (horizontal divider line).
    Horizontal,
    /// Children side-by-side left-to-right (vertical divider line).
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Clone)]
pub enum View {
    Label(String),
    Editor(EditorId),
    Run(RunId),
    Claude(ClaudeSessionId),
}

/// How a pane is presented on screen.
///
/// Only [`Placement::Split`] is implemented now. The other variants establish
/// the type structure for future modal, floating status, and docked panes
/// without requiring a redesign.
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockSide {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockVisibility {
    Open { width: u16 },
    Minimized,
    Hidden,
}

#[derive(Debug)]
pub struct DockPanel {
    pub view: View,
    pub side: DockSide,
    pub visibility: DockVisibility,
    pub default_width: u16,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusTarget {
    SplitPane(PaneId),
    Dock(DockId),
}

#[derive(Debug)]
pub struct Pane {
    pub view: View,
    pub placement: Placement,
    pub area: Rect,
    pub index: u32,
}

#[derive(Debug)]
struct Split {
    axis: Axis,
    children: Vec<NodeId>,
    area: Rect,
}

#[derive(Debug)]
enum NodeContent {
    Leaf(PaneId),
    Split(Split),
}

#[derive(Debug)]
struct Node {
    parent: NodeId,
    content: NodeContent,
}

pub struct PaneTree {
    root: NodeId,
    focus: PaneId,
    area: Rect,
    panes: SlotMap<PaneId, Pane>,
    nodes: SlotMap<NodeId, Node>,
    next_index: u32,
    stack: Vec<(NodeId, Rect)>,
}

impl PaneTree {
    pub fn new(area: Rect) -> Self {
        let mut panes = SlotMap::with_key();
        let mut nodes = SlotMap::with_key();

        let pane_id = panes.insert(Pane {
            view: View::Label("Pane 0".into()),
            placement: Placement::Split,
            area,
            index: 0,
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

    /// Splits the focused pane along `axis`, creating a new pane.
    ///
    /// If the parent split has the same axis, the new pane is inserted adjacent.
    /// Otherwise a new nested split is created. Focus moves to the new pane.
    pub fn split(&mut self, axis: Axis) -> PaneId {
        let focused_pane = self.focus;
        let focused_node = self.node_for_pane(focused_pane);

        let focused_view = self.panes[focused_pane].view.clone();
        let new_pane_id = self.panes.insert(Pane {
            view: focused_view,
            placement: Placement::Split,
            area: Rect::default(),
            index: self.next_index,
        });
        self.next_index += 1;

        let new_leaf = self.nodes.insert(Node {
            parent: NodeId::default(),
            content: NodeContent::Leaf(new_pane_id),
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
            }
            self.nodes[new_leaf].parent = parent_id;
        } else {
            let split_node_id = self.nodes.insert(Node {
                parent: NodeId::default(),
                content: NodeContent::Split(Split {
                    axis,
                    children: vec![focused_node, new_leaf],
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

        self.focus = new_pane_id;
        self.recalculate();
        new_pane_id
    }

    /// Removes a pane. Returns `false` if it's the last split pane.
    ///
    /// Collapses single-child splits. Moves focus to a neighbor if the
    /// closed pane was focused.
    pub fn close(&mut self, id: PaneId) -> bool {
        if self.split_pane_count() <= 1 {
            return false;
        }

        let node_id = self.node_for_pane(id);

        if self.focus == id {
            self.focus = self.next_split_pane(id);
        }

        let parent_id = self.nodes[node_id].parent;
        self.nodes.remove(node_id);
        self.panes.remove(id);

        if let NodeContent::Split(split) = &mut self.nodes[parent_id].content {
            split.children.retain(|&c| c != node_id);

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

        self.recalculate();
        true
    }

    pub fn resize(&mut self, area: Rect) {
        self.area = area;
        self.recalculate();
    }

    /// Moves focus to the adjacent split pane in `direction`.
    /// Returns whether focus actually changed.
    pub fn focus_direction(&mut self, direction: Direction) -> bool {
        let current_node = self.node_for_pane(self.focus);
        if let Some(target) = self.find_split_in_direction(current_node, direction) {
            if let NodeContent::Leaf(pane_id) = self.nodes[target].content {
                if pane_id != self.focus {
                    self.focus = pane_id;
                    return true;
                }
            }
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

    fn split_pane_count(&self) -> usize {
        self.panes
            .values()
            .filter(|p| p.placement == Placement::Split)
            .count()
    }

    fn node_for_pane(&self, pane_id: PaneId) -> NodeId {
        self.nodes
            .iter()
            .find_map(|(nid, node)| match &node.content {
                NodeContent::Leaf(pid) if *pid == pane_id => Some(nid),
                _ => None,
            })
            .expect("pane not found in tree")
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
                            let per_child = usable / len as u16;
                            let mut y = area.y;

                            for (i, &child_id) in split.children.iter().enumerate() {
                                let h = if i == len - 1 {
                                    area.y + area.height - y
                                } else {
                                    per_child
                                };
                                self.stack
                                    .push((child_id, Rect::new(area.x, y, area.width, h)));
                                y += h + gap;
                            }
                        },
                        Axis::Vertical => {
                            let gap = 1u16;
                            let total_gap = gap.saturating_mul(len.saturating_sub(1) as u16);
                            let usable = area.width.saturating_sub(total_gap);
                            let per_child = usable / len as u16;
                            let mut x = area.x;

                            for (i, &child_id) in split.children.iter().enumerate() {
                                let w = if i == len - 1 {
                                    area.x + area.width - x
                                } else {
                                    per_child
                                };
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
}
