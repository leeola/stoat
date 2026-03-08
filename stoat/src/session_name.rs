use uuid::Uuid;

const ADJECTIVES: [&str; 256] = [
    "swift", "quick", "fast", "slow", "brisk", "hasty", "rapid", "calm", "warm", "cold", "hot",
    "cool", "icy", "frozen", "mild", "dry", "wet", "big", "small", "tall", "short", "wide", "thin",
    "deep", "flat", "vast", "tiny", "slim", "lean", "broad", "thick", "long", "bright", "dark",
    "dim", "pale", "vivid", "dull", "clear", "foggy", "red", "blue", "green", "gold", "silver",
    "amber", "coral", "jade", "ruby", "gray", "white", "black", "brown", "pink", "copper", "ivory",
    "bronze", "violet", "loud", "soft", "quiet", "silent", "muted", "harsh", "gentle", "happy",
    "sad", "angry", "brave", "bold", "proud", "shy", "keen", "kind", "stern", "glad", "mad",
    "merry", "jolly", "grim", "fierce", "noble", "loyal", "fair", "wise", "meek", "sane", "coy",
    "wary", "eager", "rough", "smooth", "sharp", "hard", "light", "heavy", "strong", "tough",
    "stiff", "loose", "tight", "dense", "raw", "sleek", "coarse", "sunny", "rainy", "stormy",
    "windy", "snowy", "misty", "cloudy", "lunar", "solar", "mossy", "sandy", "rocky", "muddy",
    "grassy", "sweet", "sour", "bitter", "salty", "spicy", "tangy", "rich", "bland", "broken",
    "open", "full", "empty", "busy", "idle", "free", "lost", "found", "ready", "live", "gone",
    "safe", "clean", "dusty", "rusty", "shiny", "new", "old", "fresh", "near", "far", "high",
    "low", "inner", "outer", "upper", "left", "right", "even", "odd", "plain", "fancy", "neat",
    "round", "curly", "wavy", "furry", "lanky", "burly", "great", "fine", "good", "rare", "real",
    "true", "pure", "grand", "humble", "stark", "bleak", "young", "early", "late", "prime", "wild",
    "active", "alert", "awake", "alive", "alone", "royal", "sacred", "steady", "stable", "flying",
    "jumping", "running", "gliding", "diving", "rising", "falling", "growing", "fading", "hiding",
    "seeking", "moving", "rolling", "dashing", "waving", "singing", "humming", "roaring",
    "flowing", "turning", "resting", "burning", "blazing", "soaring", "leaping", "racing",
    "sailing", "trading", "making", "baking", "giving", "taking", "wooden", "golden", "frosty",
    "dusky", "crisp", "terse", "curt", "wry", "gaunt", "plump", "stout", "prim", "snug", "taut",
    "limp", "frail", "sheer", "steep", "lush", "arid", "barren", "mellow", "somber", "sullen",
    "tender", "placid", "serene", "rugged", "simple", "direct", "proper", "florid",
];

const NOUNS: [&str; 256] = [
    "dog", "cat", "fox", "owl", "hawk", "bear", "wolf", "deer", "elk", "crow", "dove", "swan",
    "frog", "fish", "crab", "moth", "wasp", "ant", "bee", "ram", "bull", "seal", "newt", "wren",
    "lark", "jay", "eel", "cod", "yak", "hen", "ape", "bat", "pine", "oak", "elm", "ash", "fir",
    "yew", "birch", "palm", "vine", "reed", "fern", "sage", "mint", "rose", "lily", "iris",
    "maple", "cedar", "plum", "pear", "moon", "sun", "star", "lake", "pond", "creek", "river",
    "hill", "peak", "ridge", "cliff", "cave", "dune", "reef", "isle", "bay", "cove", "field",
    "glen", "dale", "vale", "marsh", "grove", "brook", "rain", "snow", "wind", "storm", "cloud",
    "frost", "fog", "mist", "dew", "hail", "stone", "rock", "sand", "clay", "dust", "iron",
    "steel", "brass", "tin", "coal", "glass", "silk", "wool", "flax", "rope", "hand", "foot",
    "bone", "fang", "claw", "wing", "tail", "mane", "horn", "hoof", "bell", "drum", "flute",
    "harp", "lamp", "flag", "crown", "ring", "coin", "gem", "pearl", "blade", "helm", "bolt",
    "knot", "quill", "wheel", "gate", "arch", "wall", "tower", "fort", "dome", "tent", "raft",
    "ship", "cart", "plow", "loom", "axe", "saw", "rake", "spade", "dawn", "dusk", "noon", "eve",
    "day", "night", "year", "age", "edge", "path", "road", "trail", "track", "mark", "sign", "key",
    "lock", "link", "bond", "ledge", "fire", "flame", "spark", "glow", "beam", "ray", "flash",
    "blaze", "bread", "cake", "pie", "soup", "stew", "jam", "corn", "rice", "oat", "wheat", "rye",
    "bean", "pea", "fig", "nut", "seed", "root", "leaf", "twig", "bark", "bud", "stem", "thorn",
    "bloom", "moss", "shade", "shore", "bank", "notch", "crest", "spur", "gap", "gorge", "dell",
    "bluff", "knoll", "barn", "hut", "shed", "lodge", "mill", "pier", "dock", "well", "moat",
    "ditch", "fence", "hedge", "stump", "log", "plank", "post", "chalk", "ink", "wax", "dye",
    "tar", "foam", "gel", "pitch", "note", "tune", "song", "chord", "beat", "chant", "hymn",
    "verse", "craft", "skill", "trade", "guild", "clan", "tribe", "troop", "fleet", "lance",
    "mace", "pike", "bow", "dart", "sling", "whip", "staff",
];

pub struct SessionName {
    uuid: Uuid,
    adjective: &'static str,
    noun: &'static str,
}

impl SessionName {
    pub fn generate() -> Self {
        let uuid = Uuid::new_v4();
        Self::from_uuid(uuid)
    }

    pub fn generate_unique(existing_slugs: &[&str]) -> Self {
        loop {
            let name = Self::generate();
            let slug = name.file_slug();
            if !existing_slugs.contains(&slug.as_str()) {
                return name;
            }
        }
    }

    fn from_uuid(uuid: Uuid) -> Self {
        let bytes = uuid.as_bytes();
        let adjective = ADJECTIVES[bytes[0] as usize];
        let noun = NOUNS[bytes[1] as usize];
        Self {
            uuid,
            adjective,
            noun,
        }
    }

    pub fn display_name(&self) -> String {
        format!("{} {}", self.adjective, self.noun)
    }

    pub fn file_slug(&self) -> String {
        format!("{}-{}", self.adjective, self.noun)
    }

    pub fn uuid(&self) -> &Uuid {
        &self.uuid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_from_uuid() {
        let uuid = Uuid::parse_str("ab01cd23-0000-0000-0000-000000000000").unwrap();
        let name = SessionName::from_uuid(uuid);
        // byte 0 = 0xAB = 171 -> ADJECTIVES[171]
        // byte 1 = 0x01 = 1   -> NOUNS[1]
        assert_eq!(name.adjective, "real");
        assert_eq!(name.noun, "cat");
        assert_eq!(name.display_name(), "real cat");
        assert_eq!(name.file_slug(), "real-cat");
    }

    #[test]
    fn generate_produces_valid_name() {
        let name = SessionName::generate();
        assert!(ADJECTIVES.contains(&name.adjective));
        assert!(NOUNS.contains(&name.noun));
        assert!(!name.display_name().is_empty());
        assert!(name.file_slug().contains('-'));
    }

    #[test]
    fn generate_unique_avoids_existing() {
        let first = SessionName::generate();
        let slug = first.file_slug();
        let unique = SessionName::generate_unique(&[&slug]);
        assert_ne!(unique.file_slug(), slug);
    }

    #[test]
    fn uuid_accessible() {
        let name = SessionName::generate();
        let _ = name.uuid().to_string();
    }

    #[test]
    fn word_lists_no_duplicates() {
        let mut adj_set = std::collections::HashSet::new();
        for adj in &ADJECTIVES {
            assert!(adj_set.insert(adj), "duplicate adjective: {adj}");
        }
        let mut noun_set = std::collections::HashSet::new();
        for noun in &NOUNS {
            assert!(noun_set.insert(noun), "duplicate noun: {noun}");
        }
    }
}
