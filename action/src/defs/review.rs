use crate::{action::define_action, ActionKind};

define_action!(
    OpenReviewDef,
    OpenReview,
    "OpenReview",
    ActionKind::OpenReview,
    "review changed files",
    "Open the first modified or staged file with a structural diff against HEAD."
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Action;

    #[test]
    fn kind_and_name() {
        assert_eq!(OpenReview.kind(), ActionKind::OpenReview);
        assert_eq!(OpenReview.def().name(), "OpenReview");
        assert!(OpenReview.def().params().is_empty());
        assert!(OpenReview.def().palette_visible());
    }

    #[test]
    fn downcast() {
        let action: Box<dyn Action> = Box::new(OpenReview);
        assert!(action.as_any().downcast_ref::<OpenReview>().is_some());
    }
}
