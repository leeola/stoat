use super::*;

#[test]
fn new_pane_group_has_single_pane() {
    let group = PaneGroup::new();
    assert_eq!(group.panes(), vec![0]);
}

#[test]
fn split_right_creates_horizontal_axis() {
    let mut group = PaneGroup::new();
    let pane0 = group.panes()[0];
    let pane1 = group.split(pane0, SplitDirection::Right);

    assert_eq!(group.panes(), vec![0, 1]);

    match &group.root {
        Member::Axis(axis) => {
            assert_eq!(axis.axis, Axis::Horizontal);
            assert_eq!(axis.members.len(), 2);
            assert_eq!(*axis.flexes.lock(), vec![1.0, 1.0]);
        },
        _ => panic!("Expected axis"),
    }

    assert_eq!(pane1, 1);
}

#[test]
fn split_left_creates_horizontal_axis_reversed() {
    let mut group = PaneGroup::new();
    let pane0 = group.panes()[0];
    let pane1 = group.split(pane0, SplitDirection::Left);

    let panes = group.panes();
    assert_eq!(panes, vec![1, 0]);

    assert_eq!(pane1, 1);
}

#[test]
fn split_down_creates_vertical_axis() {
    let mut group = PaneGroup::new();
    let pane0 = group.panes()[0];
    let pane1 = group.split(pane0, SplitDirection::Down);

    assert_eq!(group.panes(), vec![0, 1]);

    match &group.root {
        Member::Axis(axis) => {
            assert_eq!(axis.axis, Axis::Vertical);
            assert_eq!(axis.members.len(), 2);
        },
        _ => panic!("Expected axis"),
    }

    assert_eq!(pane1, 1);
}

#[test]
fn split_up_creates_vertical_axis_reversed() {
    let mut group = PaneGroup::new();
    let pane0 = group.panes()[0];
    let pane1 = group.split(pane0, SplitDirection::Up);

    let panes = group.panes();
    assert_eq!(panes, vec![1, 0]);

    assert_eq!(pane1, 1);
}

#[test]
fn split_same_axis_inserts_adjacent() {
    let mut group = PaneGroup::new();
    let p0 = group.panes()[0];

    let p1 = group.split(p0, SplitDirection::Right);
    let _p2 = group.split(p1, SplitDirection::Right);

    assert_eq!(group.panes(), vec![0, 1, 2]);

    match &group.root {
        Member::Axis(axis) => {
            assert_eq!(axis.axis, Axis::Horizontal);
            assert_eq!(axis.members.len(), 3);
            assert_eq!(*axis.flexes.lock(), vec![1.0, 1.0, 1.0]);
        },
        _ => panic!("Expected axis"),
    }
}

#[test]
fn split_different_axis_creates_nested() {
    let mut group = PaneGroup::new();
    let p0 = group.panes()[0];

    let p1 = group.split(p0, SplitDirection::Right);
    let _p2 = group.split(p1, SplitDirection::Down);

    assert_eq!(group.panes(), vec![0, 1, 2]);

    match &group.root {
        Member::Axis(axis) => {
            assert_eq!(axis.axis, Axis::Horizontal);
            assert_eq!(axis.members.len(), 2);

            match &axis.members[1] {
                Member::Axis(nested) => {
                    assert_eq!(nested.axis, Axis::Vertical);
                    assert_eq!(nested.members.len(), 2);
                },
                _ => panic!("Expected nested axis"),
            }
        },
        _ => panic!("Expected axis"),
    }
}

#[test]
fn remove_pane_from_two_pane_axis() {
    let mut group = PaneGroup::new();
    let p0 = group.panes()[0];
    let p1 = group.split(p0, SplitDirection::Right);

    group.remove(p1).unwrap();

    assert_eq!(group.panes(), vec![0]);

    match &group.root {
        Member::Pane(id) => assert_eq!(*id, 0),
        _ => panic!("Expected single pane after axis collapse"),
    }
}

#[test]
fn remove_pane_from_three_pane_axis() {
    let mut group = PaneGroup::new();
    let p0 = group.panes()[0];
    let p1 = group.split(p0, SplitDirection::Right);
    let _p2 = group.split(p1, SplitDirection::Right);

    group.remove(p1).unwrap();

    assert_eq!(group.panes(), vec![0, 2]);

    match &group.root {
        Member::Axis(axis) => {
            assert_eq!(axis.members.len(), 2);
            assert_eq!(*axis.flexes.lock(), vec![1.0, 1.0]);
        },
        _ => panic!("Expected axis"),
    }
}

#[test]
fn remove_nested_pane_collapses_parent_axis() {
    let mut group = PaneGroup::new();
    let p0 = group.panes()[0];
    let p1 = group.split(p0, SplitDirection::Right);
    let _p2 = group.split(p1, SplitDirection::Down);

    group.remove(_p2).unwrap();

    assert_eq!(group.panes(), vec![0, 1]);

    match &group.root {
        Member::Axis(axis) => {
            assert_eq!(axis.axis, Axis::Horizontal);
            assert_eq!(axis.members.len(), 2);

            match &axis.members[1] {
                Member::Pane(id) => assert_eq!(*id, 1),
                _ => panic!("Expected nested axis to collapse to single pane"),
            }
        },
        _ => panic!("Expected axis"),
    }
}

#[test]
fn cannot_remove_last_pane() {
    let mut group = PaneGroup::new();
    let p0 = group.panes()[0];

    let result = group.remove(p0);
    assert!(result.is_err());
}

#[test]
fn pane_ids_are_unique() {
    let mut group = PaneGroup::new();
    let p0 = group.panes()[0];
    let p1 = group.split(p0, SplitDirection::Right);
    let _p2 = group.split(p1, SplitDirection::Down);
    let _p3 = group.split(p0, SplitDirection::Left);

    let panes = group.panes();
    assert_eq!(panes.len(), 4);

    let mut sorted_panes = panes.clone();
    sorted_panes.sort();
    sorted_panes.dedup();
    assert_eq!(panes.len(), sorted_panes.len());
}

#[test]
fn flexes_reset_after_split() {
    let mut group = PaneGroup::new();
    let p0 = group.panes()[0];
    let p1 = group.split(p0, SplitDirection::Right);

    match &group.root {
        Member::Axis(axis) => {
            assert_eq!(*axis.flexes.lock(), vec![1.0, 1.0]);
        },
        _ => panic!("Expected axis"),
    }

    let _p2 = group.split(p1, SplitDirection::Right);

    match &group.root {
        Member::Axis(axis) => {
            assert_eq!(*axis.flexes.lock(), vec![1.0, 1.0, 1.0]);
        },
        _ => panic!("Expected axis"),
    }
}

#[test]
fn flexes_reset_after_remove() {
    let mut group = PaneGroup::new();
    let p0 = group.panes()[0];
    let p1 = group.split(p0, SplitDirection::Right);
    let _p2 = group.split(p1, SplitDirection::Right);

    group.remove(p1).unwrap();

    match &group.root {
        Member::Axis(axis) => {
            assert_eq!(*axis.flexes.lock(), vec![1.0, 1.0]);
        },
        _ => panic!("Expected axis"),
    }
}
