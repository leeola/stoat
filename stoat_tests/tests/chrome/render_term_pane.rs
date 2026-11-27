use ratatui::{buffer::Buffer, layout::Rect};
use stoat::{
    pane::{Axis, View},
    render::term_pane::render_term_pane,
    test_fixture::fake_term_session,
    theme::Theme,
    Stoat,
};

/// A cell with a combining mark paints its character and the mark as one
/// symbol, so the accent shows on screen as it does in a copy.
#[test]
fn a_combining_mark_paints_with_its_base() {
    let (mut agent, _) = fake_term_session(2, 4);
    agent.term.feed("e\u{301}x".as_bytes());
    let area = Rect::new(0, 0, 4, 2);
    let mut buf = Buffer::empty(area);

    render_term_pane(&agent, &Theme::empty(), area, false, &mut buf);

    assert_eq!(
        (buf[(0, 0)].symbol(), buf[(1, 0)].symbol()),
        ("e\u{301}", "x"),
        "the accent shares its letter's cell, and the next cell keeps its own",
    );
}

#[test]
fn snapshot_term_pane_composited_into_split() {
    let mut h = Stoat::test();
    let ws = h.stoat.active_workspace_mut();
    ws.panes.split(Axis::Vertical);
    let focused = ws.panes.focus();

    let (term, _) = fake_term_session(24, 80);
    let agent_id = ws.terms.insert(term);
    ws.panes.pane_mut(focused).view = View::Agent(agent_id);

    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);
    h.stoat.active_workspace_mut().terms[agent_id]
        .term
        .feed(b"\x1b[1;32mclaude>\x1b[0m ready\r\nsecond line");

    h.assert_snapshot("term_pane_composited_into_split");
}
