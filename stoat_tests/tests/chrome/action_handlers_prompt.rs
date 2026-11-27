use stoat::{app::UpdateEffect, input_history::InputHistory, Stoat};
use stoat_action::{OpenFileFinder, PromptHistoryPrev};

/// The generic verb reaches the palette's history rather than naming it.
///
/// The Alt-Up tests drive the binding; this drives the action itself, so a
/// later prompt gaining history cannot quietly drop the palette arm.
#[test]
fn the_generic_verb_walks_the_palette_history() {
    let mut h = Stoat::test();
    h.stoat.active_workspace_mut().palette_history =
        InputHistory::from_entries(vec!["w".to_string()]);
    h.type_text(":");

    stoat::action_handlers::dispatch(&mut h.stoat, &PromptHistoryPrev);

    let palette = h.stoat.command_palette.as_ref().expect("open");
    let ws = h.stoat.active_workspace();
    assert_eq!(
        palette.input.text(ws),
        "w",
        "the generic verb recalled the palette's newest entry"
    );
}

/// A prompt with no history absorbs the verb rather than passing it on, so
/// Alt-Up over the finder never reaches the editor behind it.
#[test]
fn the_generic_verb_is_inert_over_a_prompt_without_history() {
    let mut h = Stoat::test();
    h.seed_focused_buffer("alpha\nbeta\n");
    stoat::action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
    h.settle();

    let effect = stoat::action_handlers::dispatch(&mut h.stoat, &PromptHistoryPrev);

    assert_eq!(
        effect,
        UpdateEffect::None,
        "the finder answers with nothing"
    );
    assert!(
        h.stoat.file_finder.is_some(),
        "and the finder is still the open modal"
    );
}
