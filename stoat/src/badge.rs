use crate::{host::ClaudeSessionId, run::RunId};
use slotmap::{new_key_type, SlotMap};

new_key_type! {
    pub struct BadgeId;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Anchor {
    TopLeft,
    TopCenter,
    TopRight,
    MidLeft,
    MidRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

impl Anchor {
    pub(crate) const ALL: [Anchor; 8] = [
        Anchor::TopLeft,
        Anchor::TopCenter,
        Anchor::TopRight,
        Anchor::MidLeft,
        Anchor::MidRight,
        Anchor::BottomLeft,
        Anchor::BottomCenter,
        Anchor::BottomRight,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeState {
    Active,
    Complete,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeSource {
    Run(RunId),
    Claude(ClaudeSessionId),
}

#[derive(Debug, Clone)]
pub struct Badge {
    pub(crate) source: BadgeSource,
    pub(crate) anchor: Anchor,
    pub(crate) state: BadgeState,
    pub(crate) label: String,
    pub(crate) detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StackDirection {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone)]
pub(crate) struct Tray {
    pub(crate) stack: StackDirection,
    pub(crate) max_visible: u8,
}

pub(crate) struct BadgeTray {
    badges: SlotMap<BadgeId, Badge>,
    trays: [Tray; 8],
}

impl BadgeTray {
    pub(crate) fn new() -> Self {
        let trays = Anchor::ALL.map(|anchor| {
            let stack = match anchor {
                Anchor::TopCenter | Anchor::BottomCenter => StackDirection::Horizontal,
                _ => StackDirection::Vertical,
            };
            Tray {
                stack,
                max_visible: 3,
            }
        });
        Self {
            badges: SlotMap::with_key(),
            trays,
        }
    }

    pub(crate) fn insert(&mut self, badge: Badge) -> BadgeId {
        self.badges.insert(badge)
    }

    pub(crate) fn remove(&mut self, id: BadgeId) -> Option<Badge> {
        self.badges.remove(id)
    }

    pub(crate) fn get(&self, id: BadgeId) -> Option<&Badge> {
        self.badges.get(id)
    }

    pub(crate) fn get_mut(&mut self, id: BadgeId) -> Option<&mut Badge> {
        self.badges.get_mut(id)
    }

    pub(crate) fn at_anchor(&self, anchor: Anchor) -> impl Iterator<Item = (BadgeId, &Badge)> {
        self.badges.iter().filter(move |(_, b)| b.anchor == anchor)
    }

    pub(crate) fn tray(&self, anchor: Anchor) -> &Tray {
        &self.trays[anchor as usize]
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.badges.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_run_id() -> RunId {
        let mut map: SlotMap<RunId, ()> = SlotMap::with_key();
        map.insert(())
    }

    fn make_badge(anchor: Anchor, state: BadgeState, label: &str) -> Badge {
        Badge {
            source: BadgeSource::Run(test_run_id()),
            anchor,
            state,
            label: label.into(),
            detail: None,
        }
    }

    #[test]
    fn insert_and_query() {
        let mut tray = BadgeTray::new();
        assert!(tray.is_empty());

        let id = tray.insert(make_badge(Anchor::BottomRight, BadgeState::Active, "make"));
        assert!(!tray.is_empty());
        assert_eq!(tray.get(id).unwrap().anchor, Anchor::BottomRight);
        assert_eq!(tray.get(id).unwrap().label, "make");
    }

    #[test]
    fn at_anchor_filters() {
        let mut tray = BadgeTray::new();

        tray.insert(make_badge(Anchor::TopLeft, BadgeState::Active, "a"));
        tray.insert(make_badge(Anchor::BottomRight, BadgeState::Complete, "b"));
        tray.insert(make_badge(Anchor::TopLeft, BadgeState::Error, "c"));

        let top_left: Vec<_> = tray.at_anchor(Anchor::TopLeft).collect();
        assert_eq!(top_left.len(), 2);

        let bottom_right: Vec<_> = tray.at_anchor(Anchor::BottomRight).collect();
        assert_eq!(bottom_right.len(), 1);

        let empty: Vec<_> = tray.at_anchor(Anchor::MidLeft).collect();
        assert_eq!(empty.len(), 0);
    }

    #[test]
    fn remove_badge() {
        let mut tray = BadgeTray::new();
        let id = tray.insert(make_badge(Anchor::MidRight, BadgeState::Active, "x"));

        assert!(tray.remove(id).is_some());
        assert!(tray.is_empty());
        assert!(tray.get(id).is_none());
    }

    #[test]
    fn mutate_badge() {
        let mut tray = BadgeTray::new();
        let id = tray.insert(make_badge(Anchor::TopCenter, BadgeState::Active, "mk"));

        tray.get_mut(id).unwrap().state = BadgeState::Complete;
        assert_eq!(tray.get(id).unwrap().state, BadgeState::Complete);
    }

    #[test]
    fn default_tray_config() {
        let tray = BadgeTray::new();
        assert_eq!(tray.tray(Anchor::TopLeft).stack, StackDirection::Vertical);
        assert_eq!(
            tray.tray(Anchor::BottomRight).stack,
            StackDirection::Vertical
        );
        assert_eq!(tray.tray(Anchor::MidRight).stack, StackDirection::Vertical);
        assert_eq!(
            tray.tray(Anchor::TopCenter).stack,
            StackDirection::Horizontal
        );
        assert_eq!(
            tray.tray(Anchor::BottomCenter).stack,
            StackDirection::Horizontal
        );
        for anchor in Anchor::ALL {
            assert_eq!(tray.tray(anchor).max_visible, 3);
        }
    }
}
