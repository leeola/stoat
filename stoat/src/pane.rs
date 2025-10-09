use anyhow::{Result, bail};
use gpui::{Axis, Bounds, Pixels};
use parking_lot::Mutex;
use std::sync::Arc;

pub type PaneId = usize;

/// Manages a tree of panes that can be split and arranged.
///
/// PaneGroup maintains a recursive tree structure where panes can be split
/// horizontally or vertically, creating nested layouts. Each pane is identified
/// by a unique [`PaneId`].
///
/// # Example
///
/// ```
/// use stoat::pane::{PaneGroup, SplitDirection};
///
/// let mut group = PaneGroup::new();
/// let pane0 = group.panes()[0];
/// let pane1 = group.split(pane0, SplitDirection::Right);
/// assert_eq!(group.panes().len(), 2);
/// ```
pub struct PaneGroup {
    root: Member,
    next_id: PaneId,
}

impl PaneGroup {
    /// Create a new pane group with a single pane (id 0).
    pub fn new() -> Self {
        Self {
            root: Member::Pane(0),
            next_id: 1,
        }
    }

    /// Get the root member of the pane tree.
    pub fn root(&self) -> &Member {
        &self.root
    }

    /// Get a list of all pane IDs in the group.
    pub fn panes(&self) -> Vec<PaneId> {
        let mut panes = Vec::new();
        self.root.collect_panes(&mut panes);
        panes
    }

    /// Split a pane in the given direction, creating a new pane.
    ///
    /// Returns the ID of the newly created pane.
    pub fn split(&mut self, pane: PaneId, direction: SplitDirection) -> PaneId {
        let new_pane_id = self.next_id;
        self.next_id += 1;

        match &mut self.root {
            Member::Pane(id) if *id == pane => {
                self.root = Member::new_axis(pane, new_pane_id, direction);
            },
            Member::Axis(axis) => {
                axis.split(pane, new_pane_id, direction)
                    .expect("Pane not found");
            },
            _ => panic!("Pane not found"),
        }

        new_pane_id
    }

    /// Remove a pane from the group.
    ///
    /// If this is the last pane, returns an error.
    /// When removing a pane causes an axis to have only one child,
    /// the axis is collapsed.
    pub fn remove(&mut self, pane_to_remove: PaneId) -> Result<()> {
        match &mut self.root {
            Member::Pane(id) if *id == pane_to_remove => {
                bail!("Cannot remove the last pane");
            },
            Member::Axis(axis) => {
                if let Ok(Some(last_member)) = axis.remove(pane_to_remove) {
                    self.root = last_member;
                }
                Ok(())
            },
            _ => bail!("Pane not found"),
        }
    }
}

impl Default for PaneGroup {
    fn default() -> Self {
        Self::new()
    }
}

/// A member of the pane tree - either a leaf pane or an axis containing more members.
#[derive(Debug, Clone)]
pub enum Member {
    Axis(PaneAxis),
    Pane(PaneId),
}

impl Member {
    /// Create a new axis containing the old and new panes in the appropriate order
    /// based on the split direction.
    fn new_axis(old_pane: PaneId, new_pane: PaneId, direction: SplitDirection) -> Self {
        let axis = direction.axis();

        let members = if direction.increasing() {
            vec![Member::Pane(old_pane), Member::Pane(new_pane)]
        } else {
            vec![Member::Pane(new_pane), Member::Pane(old_pane)]
        };

        Member::Axis(PaneAxis::new(axis, members))
    }

    /// Collect all pane IDs from this member and its children.
    fn collect_panes(&self, panes: &mut Vec<PaneId>) {
        match self {
            Member::Axis(axis) => {
                for member in &axis.members {
                    member.collect_panes(panes);
                }
            },
            Member::Pane(pane) => panes.push(*pane),
        }
    }
}

/// A container that splits space along an axis, containing multiple members.
#[derive(Debug, Clone)]
pub struct PaneAxis {
    pub axis: Axis,
    pub members: Vec<Member>,
    pub flexes: Arc<Mutex<Vec<f32>>>,
    pub bounding_boxes: Arc<Mutex<Vec<Option<Bounds<Pixels>>>>>,
}

impl PaneAxis {
    /// Create a new axis with the given members.
    ///
    /// Flexes are initialized to 1.0 for each member.
    pub fn new(axis: Axis, members: Vec<Member>) -> Self {
        let flexes = Arc::new(Mutex::new(vec![1.0; members.len()]));
        let bounding_boxes = Arc::new(Mutex::new(vec![None; members.len()]));
        Self {
            axis,
            members,
            flexes,
            bounding_boxes,
        }
    }

    /// Split a pane within this axis, creating a new pane.
    fn split(
        &mut self,
        old_pane: PaneId,
        new_pane: PaneId,
        direction: SplitDirection,
    ) -> Result<()> {
        for (mut idx, member) in self.members.iter_mut().enumerate() {
            match member {
                Member::Axis(axis) => {
                    if axis.split(old_pane, new_pane, direction).is_ok() {
                        return Ok(());
                    }
                },
                Member::Pane(pane) if *pane == old_pane => {
                    if direction.axis() == self.axis {
                        // Same axis - insert adjacent
                        if direction.increasing() {
                            idx += 1;
                        }
                        self.members.insert(idx, Member::Pane(new_pane));
                        *self.flexes.lock() = vec![1.0; self.members.len()];
                    } else {
                        // Different axis - create nested axis
                        *member = Member::new_axis(old_pane, new_pane, direction);
                    }
                    return Ok(());
                },
                _ => {},
            }
        }
        bail!("Pane not found")
    }

    /// Remove a pane from this axis.
    ///
    /// Returns `Ok(Some(member))` if the axis now has only one child (which should replace the
    /// axis). Returns `Ok(None)` if the axis still has multiple children.
    fn remove(&mut self, pane_to_remove: PaneId) -> Result<Option<Member>> {
        let mut found_pane = false;
        let mut remove_member = None;

        for (idx, member) in self.members.iter_mut().enumerate() {
            match member {
                Member::Axis(axis) => {
                    if let Ok(last_member) = axis.remove(pane_to_remove) {
                        if let Some(last_member) = last_member {
                            *member = last_member;
                        }
                        found_pane = true;
                        break;
                    }
                },
                Member::Pane(pane) if *pane == pane_to_remove => {
                    found_pane = true;
                    remove_member = Some(idx);
                    break;
                },
                _ => {},
            }
        }

        if found_pane {
            if let Some(idx) = remove_member {
                self.members.remove(idx);
                *self.flexes.lock() = vec![1.0; self.members.len()];
            }

            if self.members.len() == 1 {
                Ok(self.members.pop())
            } else {
                Ok(None)
            }
        } else {
            bail!("Pane not found")
        }
    }
}

/// Direction of a pane split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    /// Split upward - creates tall panes side-by-side (vertical axis)
    Up,
    /// Split downward - creates tall panes side-by-side (vertical axis)
    Down,
    /// Split left - creates wide panes stacked (horizontal axis)
    Left,
    /// Split right - creates wide panes stacked (horizontal axis)
    Right,
}

impl SplitDirection {
    /// Get the axis this split direction creates.
    pub fn axis(&self) -> Axis {
        match self {
            SplitDirection::Up | SplitDirection::Down => Axis::Vertical,
            SplitDirection::Left | SplitDirection::Right => Axis::Horizontal,
        }
    }

    /// Whether the new pane should be inserted after the existing pane.
    pub fn increasing(&self) -> bool {
        match self {
            SplitDirection::Left | SplitDirection::Up => false,
            SplitDirection::Down | SplitDirection::Right => true,
        }
    }
}
