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
    /// Notification attached to the active review session. At most one
    /// Review-sourced badge exists per workspace.
    Review,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
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

pub(crate) const THROBBER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

#[allow(dead_code)]
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

    pub(crate) fn find_by_source(&self, source: BadgeSource) -> Option<BadgeId> {
        self.badges
            .iter()
            .find(|(_, b)| b.source == source)
            .map(|(id, _)| id)
    }

    pub(crate) fn remove_by_source(&mut self, source: BadgeSource) -> Option<Badge> {
        if let Some(id) = self.find_by_source(source) {
            self.remove(id)
        } else {
            None
        }
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

    use crate::{
        host::{AgentMessage, FakeClaudeCode},
        test_harness::{claude::ResultSpec, setup_hidden_claude_session, TestHarness},
    };

    #[test]
    fn badge_appears_when_not_visible() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        assert!(h.claude_badge_state(id).is_none());

        h.claude().get_session(id).thinking("let me think");

        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Active));
        assert_eq!(h.claude_badge_detail(id), Some("thinking".into()));
    }

    #[test]
    fn badge_detail_updates_with_tool() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        h.claude().get_session(id).thinking("hmm");
        assert_eq!(h.claude_badge_detail(id), Some("thinking".into()));

        h.claude()
            .get_session(id)
            .read("/tmp/example.txt")
            .pending();
        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Active));
        assert_eq!(h.claude_badge_detail(id), Some("Read".into()));

        h.claude().get_session(id).text("done reading");
        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Active));
        assert_eq!(h.claude_badge_detail(id), None);
    }

    #[test]
    fn badge_completes_on_result() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        h.claude().get_session(id).thinking("work");
        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Active));

        h.claude().get_session(id).result_with(ResultSpec {
            cost_usd: 0.01,
            duration_ms: 1000,
            num_turns: 1,
        });
        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Complete));
        assert_eq!(h.claude_badge_detail(id), None);
    }

    #[test]
    fn badge_errors_on_error() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        h.claude()
            .get_session(id)
            .thinking("work")
            .error("rate limit");

        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Error));
        assert_eq!(h.claude_badge_detail(id), Some("rate limit".into()));
    }

    #[test]
    fn badge_removed_when_session_shown() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        h.claude().get_session(id).thinking("work");
        assert!(h.claude_badge_state(id).is_some());

        h.show_claude_session(id);
        assert!(h.claude_badge_state(id).is_none());
    }

    #[test]
    fn no_badge_when_visible() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        h.show_claude_session(id);

        h.claude().get_session(id).thinking("work");
        assert!(h.claude_badge_state(id).is_none());
    }

    #[test]
    fn badge_reappears_after_hide() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        h.show_claude_session(id);
        h.claude().get_session(id).thinking("work");
        assert!(h.claude_badge_state(id).is_none());

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleDockRight);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleDockRight);

        h.claude()
            .get_session(id)
            .edit("/tmp/file.txt", "old", "new")
            .pending();
        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Active));
        assert_eq!(h.claude_badge_detail(id), Some("Edit".into()));
    }

    #[test]
    fn init_and_unknown_inert() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        h.claude().get_session(id).init();
        assert!(h.claude_badge_state(id).is_none());

        h.claude()
            .get_session(id)
            .raw(AgentMessage::Unknown { raw: "{}".into() });
        assert!(h.claude_badge_state(id).is_none());
    }

    #[test]
    fn multiple_sessions_independent() {
        let mut h = TestHarness::default();
        let id_a = setup_hidden_claude_session(&mut h);
        let id_b = h.create_background_session(FakeClaudeCode::new());

        h.claude().get_session(id_a).thinking("a");
        h.claude().get_session(id_b).bash("echo hi").pending();

        assert_eq!(h.claude_badge_state(id_a), Some(BadgeState::Active));
        assert_eq!(h.claude_badge_detail(id_a), Some("thinking".into()));
        assert_eq!(h.claude_badge_state(id_b), Some(BadgeState::Active));
        assert_eq!(h.claude_badge_detail(id_b), Some("Bash".into()));

        h.claude().get_session(id_a).result();
        assert_eq!(h.claude_badge_state(id_a), Some(BadgeState::Complete));
        assert_eq!(h.claude_badge_state(id_b), Some(BadgeState::Active));
    }

    #[test]
    fn snapshot_badge_active_styled() {
        let mut h = TestHarness::with_size(40, 10);
        let id = setup_hidden_claude_session(&mut h);
        h.claude().get_session(id).thinking("work");
        h.assert_snapshot("badge_active");
    }

    #[test]
    fn snapshot_badge_complete_styled() {
        let mut h = TestHarness::with_size(40, 10);
        let id = setup_hidden_claude_session(&mut h);
        h.claude()
            .get_session(id)
            .thinking("work")
            .result_with(ResultSpec {
                cost_usd: 0.01,
                duration_ms: 1000,
                num_turns: 1,
            });
        h.assert_snapshot("badge_complete");
    }

    #[test]
    fn result_without_prior_activity_creates_badge() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        h.claude().get_session(id).result_with(ResultSpec {
            cost_usd: 0.01,
            duration_ms: 100,
            num_turns: 1,
        });
        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Complete));
    }

    #[test]
    fn error_without_prior_activity_creates_badge() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        h.claude().get_session(id).error("failed");
        assert_eq!(h.claude_badge_state(id), Some(BadgeState::Error));
        assert_eq!(h.claude_badge_detail(id), Some("failed".into()));
    }

    #[test]
    fn visible_session_result_removes_badge() {
        let mut h = TestHarness::default();
        let id = setup_hidden_claude_session(&mut h);

        h.claude().get_session(id).thinking("work");
        assert!(h.claude_badge_state(id).is_some());

        h.show_claude_session(id);
        assert!(h.claude_badge_state(id).is_none());

        h.claude().get_session(id).result_with(ResultSpec {
            cost_usd: 0.01,
            duration_ms: 100,
            num_turns: 1,
        });
        assert!(h.claude_badge_state(id).is_none());
    }
}
