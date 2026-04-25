use crate::workspace::WorkspaceUid;

/// 32 short adjectives. Index = low 5 bits of the workspace uid.
const ADJECTIVES: &[&str] = &[
    "brave", "calm", "dusty", "eager", "fancy", "glad", "hazy", "icy", "jolly", "keen", "lazy",
    "mild", "neat", "odd", "prim", "quiet", "rapid", "slim", "tidy", "vast", "wild", "wise",
    "zany", "plump", "swift", "blue", "gold", "sharp", "sleek", "fuzzy", "rusty", "jumpy",
];

/// 32 short animal names. Index = next 5 bits of the workspace uid.
const ANIMALS: &[&str] = &[
    "otter", "bear", "fox", "owl", "mole", "vole", "lynx", "hare", "deer", "swan", "crane",
    "eagle", "hawk", "robin", "finch", "badger", "raven", "wolf", "lion", "panda", "koala",
    "gecko", "lizard", "crab", "salmon", "trout", "octopus", "manatee", "walrus", "mongoose",
    "ferret", "beaver",
];

/// Deterministic short name like `"rapid mongoose"` derived from a workspace
/// uid. Same uid always yields the same name; consecutive uids vary because
/// [`WorkspaceUid`] is wall-clock nanoseconds and the low bits index the
/// adjective list.
pub(crate) fn default_workspace_name(uid: WorkspaceUid) -> String {
    let bits = uid.0;
    let adj = ADJECTIVES[(bits & 0x1F) as usize];
    let animal = ANIMALS[((bits >> 5) & 0x1F) as usize];
    format!("{adj} {animal}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_have_expected_lengths() {
        assert_eq!(ADJECTIVES.len(), 32);
        assert_eq!(ANIMALS.len(), 32);
    }

    #[test]
    fn deterministic_for_same_uid() {
        let uid = WorkspaceUid(0x1234_5678_9abc_def0);
        assert_eq!(default_workspace_name(uid), default_workspace_name(uid));
    }

    #[test]
    fn name_format_is_adj_space_animal() {
        let name = default_workspace_name(WorkspaceUid(0));
        let parts: Vec<&str> = name.split(' ').collect();
        assert_eq!(parts.len(), 2);
        assert!(ADJECTIVES.contains(&parts[0]));
        assert!(ANIMALS.contains(&parts[1]));
    }

    #[test]
    fn low_bits_pick_adjective_high_bits_pick_animal() {
        assert_eq!(default_workspace_name(WorkspaceUid(0)), "brave otter");
        assert_eq!(default_workspace_name(WorkspaceUid(1)), "calm otter");
        assert_eq!(default_workspace_name(WorkspaceUid(1 << 5)), "brave bear");
        assert_eq!(
            default_workspace_name(WorkspaceUid((1 << 5) | 1)),
            "calm bear"
        );
    }
}
