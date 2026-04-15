//! Fuzzy model-preference resolver.
//!
//! Given a user-supplied preference string (e.g. `"opus"`, `"sonnet
//! [1m]"`, `"opusplan"`) and the list of available models advertised
//! by the CLI in its `init` frame, tokenize the preference, score every
//! candidate, and return the best match. Context hints like `[1m]` or
//! `[2m]` are extracted from the tail of the preference; noise words
//! (`best`, `default`, `claude`, `opusplan` -> `opus`) are stripped.

use stoat::host::ModelInfo;

/// Tokenize a user preference. Returns `(tokens, context_hint)`. The
/// context hint is `[Nm]`-style (e.g. `[1m]`) extracted from the tail
/// of the input; noise words (`best`, `default`, `claude`, `opusplan`
/// -> `opus`) are stripped, and the remainder is split on
/// non-alphanumeric boundaries.
pub fn tokenize_model_preference(pref: &str) -> (Vec<String>, Option<String>) {
    let trimmed = pref.trim();
    let lower = trimmed.to_lowercase();
    let mut work = lower.clone();
    let mut context_hint = None;

    // Extract trailing `[Nm]` context hint.
    if let Some(start) = work.rfind('[')
        && work.ends_with(']')
    {
        let inner = &work[start + 1..work.len() - 1];
        if inner.ends_with('m') && inner[..inner.len() - 1].chars().all(|c| c.is_ascii_digit()) {
            context_hint = Some(inner.to_string());
            work.truncate(start);
        }
    }

    // Collapse punctuation into spaces, then split.
    let mut tokens: Vec<String> = work
        .replace(['[', ']', '(', ')', ',', '/', '_', '-'], " ")
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();

    // Normalise synonyms / strip noise words.
    let mut cleaned: Vec<String> = Vec::new();
    for tok in tokens.drain(..) {
        let norm = match tok.as_str() {
            "opusplan" => "opus",
            "best" | "default" | "claude" => continue,
            other => other,
        };
        if norm.chars().any(|c| c.is_ascii_alphabetic()) || norm.ends_with('m') {
            cleaned.push(norm.to_string());
        }
    }

    (cleaned, context_hint)
}

/// Score a candidate model against the tokenised preference.
pub fn score_model_match(model: &ModelInfo, tokens: &[String], context_hint: Option<&str>) -> u32 {
    let haystack = format!("{} {}", model.id.to_lowercase(), model.name.to_lowercase());
    let mut score = 0u32;
    for tok in tokens {
        if haystack.contains(tok) {
            score += 1;
        }
    }
    if let Some(hint) = context_hint
        && haystack.contains(hint)
    {
        score += 3;
    }
    score
}

/// Resolve a preference to the best-matching model. Returns `None`
/// when nothing scores above zero.
pub fn resolve_model_preference(pref: &str, available: &[ModelInfo]) -> Option<ModelInfo> {
    let lower = pref.trim().to_lowercase();

    // Exact match on id or name.
    for model in available {
        if model.id.to_lowercase() == lower || model.name.to_lowercase() == lower {
            return Some(model.clone());
        }
    }
    // Substring both directions.
    for model in available {
        let id_l = model.id.to_lowercase();
        let name_l = model.name.to_lowercase();
        if id_l.contains(&lower)
            || name_l.contains(&lower)
            || lower.contains(&id_l)
            || lower.contains(&name_l)
        {
            return Some(model.clone());
        }
    }
    // Tokenised scoring.
    let (tokens, context_hint) = tokenize_model_preference(pref);
    if tokens.is_empty() && context_hint.is_none() {
        return None;
    }
    let mut best: Option<(u32, &ModelInfo)> = None;
    for model in available {
        let score = score_model_match(model, &tokens, context_hint.as_deref());
        if score == 0 {
            continue;
        }
        match best {
            Some((s, _)) if s >= score => (),
            _ => best = Some((score, model)),
        }
    }
    best.map(|(_, m)| m.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: &str, name: &str) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: name.into(),
            description: String::new(),
        }
    }

    #[test]
    fn tokenizer_extracts_context_hint() {
        let (tokens, hint) = tokenize_model_preference("opus[1m]");
        assert_eq!(hint.as_deref(), Some("1m"));
        assert_eq!(tokens, vec!["opus"]);
    }

    #[test]
    fn tokenizer_strips_noise_words() {
        let (tokens, _) = tokenize_model_preference("best claude opus");
        assert_eq!(tokens, vec!["opus"]);
    }

    #[test]
    fn tokenizer_aliases_opusplan_to_opus() {
        let (tokens, _) = tokenize_model_preference("opusplan");
        assert_eq!(tokens, vec!["opus"]);
    }

    #[test]
    fn resolve_exact_match_wins() {
        let models = vec![
            m("claude-opus-4", "Opus 4"),
            m("claude-sonnet-4", "Sonnet 4"),
        ];
        let result = resolve_model_preference("claude-opus-4", &models).unwrap();
        assert_eq!(result.id, "claude-opus-4");
    }

    #[test]
    fn resolve_tokenised_scores_highest() {
        let models = vec![
            m("claude-opus-4-6", "Opus 4.6"),
            m("claude-sonnet-4-6", "Sonnet 4.6"),
            m("claude-haiku-4-5", "Haiku 4.5"),
        ];
        let result = resolve_model_preference("opus[1m]", &models).unwrap();
        assert_eq!(result.id, "claude-opus-4-6");
    }

    #[test]
    fn resolve_none_when_unmatched() {
        let models = vec![m("claude-opus-4", "Opus 4")];
        assert!(resolve_model_preference("gpt", &models).is_none());
    }
}
