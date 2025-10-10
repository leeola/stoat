use gpui::{
    Along, AnyElement, App, Axis, Bounds, CursorStyle, Element, GlobalElementId, Hitbox,
    HitboxBehavior, IntoElement, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement,
    Pixels, Point, Size, Style, Window, px, relative, size,
};
use parking_lot::Mutex;
use smallvec::SmallVec;
use std::{cell::RefCell, mem, rc::Rc, sync::Arc};
use tracing::{debug, trace};

const HANDLE_HITBOX_SIZE: f32 = 4.0;
const DIVIDER_SIZE: f32 = 1.0;
const HORIZONTAL_MIN_SIZE: f32 = 80.0;
const VERTICAL_MIN_SIZE: f32 = 100.0;

/// Creates a pane axis element for laying out panes with resizable dividers.
pub fn pane_axis(
    axis: Axis,
    basis: usize,
    flexes: Arc<Mutex<Vec<f32>>>,
    bounding_boxes: Arc<Mutex<Vec<Option<Bounds<Pixels>>>>>,
) -> PaneAxisElement {
    PaneAxisElement {
        axis,
        basis,
        flexes,
        bounding_boxes,
        children: SmallVec::new(),
    }
}

/// Custom GPUI element for rendering panes with interactive resize handles.
pub struct PaneAxisElement {
    axis: Axis,
    basis: usize,
    flexes: Arc<Mutex<Vec<f32>>>,
    bounding_boxes: Arc<Mutex<Vec<Option<Bounds<Pixels>>>>>,
    children: SmallVec<[AnyElement; 2]>,
}

pub struct PaneAxisLayout {
    dragged_handle: Rc<RefCell<Option<usize>>>,
    children: Vec<PaneAxisChildLayout>,
}

pub struct PaneAxisChildLayout {
    bounds: Bounds<Pixels>,
    element: AnyElement,
    handle: Option<PaneAxisHandleLayout>,
}

pub struct PaneAxisHandleLayout {
    hitbox: Hitbox,
    divider_bounds: Bounds<Pixels>,
}

impl PaneAxisElement {
    fn compute_resize(
        flexes: &Arc<Mutex<Vec<f32>>>,
        e: &MouseMoveEvent,
        ix: usize,
        axis: Axis,
        child_start: Point<Pixels>,
        container_size: Size<Pixels>,
        window: &mut Window,
    ) {
        let min_size = match axis {
            Axis::Horizontal => px(HORIZONTAL_MIN_SIZE),
            Axis::Vertical => px(VERTICAL_MIN_SIZE),
        };
        let mut flexes = flexes.lock();

        let size = move |ix: usize, flexes: &[f32]| {
            container_size.along(axis) * (flexes[ix] / flexes.len() as f32)
        };

        // Don't allow resizing to less than the minimum size
        if min_size - px(1.0) > size(ix, flexes.as_slice()) {
            return;
        }

        let mut proposed_current_pixel_change =
            (e.position - child_start).along(axis) - size(ix, flexes.as_slice());

        let flex_changes = |pixel_dx: Pixels, target_ix: usize, next: isize, flexes: &[f32]| {
            let flex_change = pixel_dx / container_size.along(axis);
            let current_target_flex = flexes[target_ix] + flex_change;
            let next_target_flex = flexes[(target_ix as isize + next) as usize] - flex_change;
            (current_target_flex, next_target_flex)
        };

        // Generate successors based on drag direction
        let mut successors = std::iter::from_fn({
            let forward = proposed_current_pixel_change > px(0.0);
            let mut ix_offset = 0;
            let len = flexes.len();
            move || {
                let result = if forward {
                    (ix + 1 + ix_offset < len).then(|| ix + ix_offset)
                } else {
                    (ix as isize - ix_offset as isize >= 0).then(|| ix - ix_offset)
                };
                ix_offset += 1;
                result
            }
        });

        // Apply pixel changes to flex values
        while proposed_current_pixel_change.abs() > px(0.0) {
            let Some(current_ix) = successors.next() else {
                break;
            };

            let next_target_size = Pixels::max(
                size(current_ix + 1, flexes.as_slice()) - proposed_current_pixel_change,
                min_size,
            );

            let current_target_size = Pixels::max(
                size(current_ix, flexes.as_slice()) + size(current_ix + 1, flexes.as_slice())
                    - next_target_size,
                min_size,
            );

            let current_pixel_change = current_target_size - size(current_ix, flexes.as_slice());

            let (current_target_flex, next_target_flex) =
                flex_changes(current_pixel_change, current_ix, 1, flexes.as_slice());

            flexes[current_ix] = current_target_flex;
            flexes[current_ix + 1] = next_target_flex;

            proposed_current_pixel_change -= current_pixel_change;
        }

        window.refresh();
    }

    fn layout_handle(
        axis: Axis,
        pane_bounds: Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut App,
    ) -> PaneAxisHandleLayout {
        let handle_bounds = Bounds {
            origin: pane_bounds.origin.apply_along(axis, |origin| {
                origin + pane_bounds.size.along(axis) - px(HANDLE_HITBOX_SIZE / 2.0)
            }),
            size: pane_bounds
                .size
                .apply_along(axis, |_| px(HANDLE_HITBOX_SIZE)),
        };
        let divider_bounds = Bounds {
            origin: pane_bounds
                .origin
                .apply_along(axis, |origin| origin + pane_bounds.size.along(axis)),
            size: pane_bounds.size.apply_along(axis, |_| px(DIVIDER_SIZE)),
        };

        PaneAxisHandleLayout {
            hitbox: window.insert_hitbox(handle_bounds, HitboxBehavior::BlockMouse),
            divider_bounds,
        }
    }
}

impl IntoElement for PaneAxisElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for PaneAxisElement {
    type RequestLayoutState = ();
    type PrepaintState = PaneAxisLayout;

    fn id(&self) -> Option<gpui::ElementId> {
        Some(self.basis.into())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        let style = Style {
            flex_grow: 1.0,
            flex_shrink: 1.0,
            flex_basis: relative(0.0).into(),
            size: size(relative(1.0).into(), relative(1.0).into()),
            ..Style::default()
        };
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> PaneAxisLayout {
        let dragged_handle = window.with_element_state::<Rc<RefCell<Option<usize>>>, _>(
            global_id.expect("PaneAxisElement should have a global_id"),
            |state, _cx| {
                let state = state.unwrap_or_else(|| Rc::new(RefCell::new(None)));
                (state.clone(), state)
            },
        );

        let flexes = self.flexes.lock().clone();
        let len = self.children.len();
        let total_flex = len as f32;

        let mut origin = bounds.origin;
        let space_per_flex = bounds.size.along(self.axis) / total_flex;

        let mut bounding_boxes = self.bounding_boxes.lock();
        bounding_boxes.clear();

        let mut layout = PaneAxisLayout {
            dragged_handle,
            children: Vec::new(),
        };

        for (ix, mut child) in mem::take(&mut self.children).into_iter().enumerate() {
            let child_flex = flexes[ix];

            let child_size = bounds
                .size
                .apply_along(self.axis, |_| space_per_flex * child_flex)
                .map(|d| d.round());

            let child_bounds = Bounds {
                origin,
                size: child_size,
            };

            bounding_boxes.push(Some(child_bounds));
            child.layout_as_root(child_size.into(), window, cx);
            child.prepaint_at(origin, window, cx);

            origin = origin.apply_along(self.axis, |val| val + child_size.along(self.axis));

            layout.children.push(PaneAxisChildLayout {
                bounds: child_bounds,
                element: child,
                handle: None,
            });
        }

        // Add resize handles between panes
        for (ix, child_layout) in layout.children.iter_mut().enumerate() {
            if ix < len - 1 {
                child_layout.handle = Some(Self::layout_handle(
                    self.axis,
                    child_layout.bounds,
                    window,
                    cx,
                ));
            }
        }

        layout
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        layout: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Paint children
        for child in &mut layout.children {
            child.element.paint(window, cx);
        }

        // Paint dividers and handle mouse events
        for (ix, child) in &mut layout.children.iter_mut().enumerate() {
            if let Some(handle) = child.handle.as_mut() {
                let cursor_style = match self.axis {
                    Axis::Vertical => CursorStyle::ResizeRow,
                    Axis::Horizontal => CursorStyle::ResizeColumn,
                };

                if layout
                    .dragged_handle
                    .borrow()
                    .is_some_and(|dragged_ix| dragged_ix == ix)
                {
                    window.set_window_cursor_style(cursor_style);
                } else {
                    window.set_cursor_style(cursor_style, &handle.hitbox);
                }

                // Paint divider
                window.paint_quad(gpui::fill(handle.divider_bounds, gpui::rgb(0x3c3c3c)));

                // Mouse down event - start dragging
                window.on_mouse_event({
                    let dragged_handle = layout.dragged_handle.clone();
                    let flexes = self.flexes.clone();
                    let handle_hitbox = handle.hitbox.clone();
                    move |e: &MouseDownEvent, phase, window, cx| {
                        if phase.bubble() && handle_hitbox.is_hovered(window) {
                            if e.click_count >= 2 {
                                debug!(
                                    handle_ix = ix,
                                    "Double-click: resetting pane sizes to equal"
                                );
                                let mut borrow = flexes.lock();
                                *borrow = vec![1.0; borrow.len()];
                                window.refresh();
                            } else {
                                trace!(handle_ix = ix, "Starting pane resize drag");
                                dragged_handle.replace(Some(ix));
                            }
                            cx.stop_propagation();
                        }
                    }
                });

                // Mouse move event - resize
                window.on_mouse_event({
                    let dragged_handle = layout.dragged_handle.clone();
                    let flexes = self.flexes.clone();
                    let child_bounds = child.bounds;
                    let axis = self.axis;
                    move |e: &MouseMoveEvent, phase, window, cx| {
                        let dragged_handle = dragged_handle.borrow();
                        if phase.bubble() && *dragged_handle == Some(ix) {
                            Self::compute_resize(
                                &flexes,
                                e,
                                ix,
                                axis,
                                child_bounds.origin,
                                bounds.size,
                                window,
                            );
                            cx.stop_propagation();
                        }
                    }
                });
            }
        }

        // Mouse up event - stop dragging
        window.on_mouse_event({
            let dragged_handle = layout.dragged_handle.clone();
            move |_: &MouseUpEvent, phase, _window, _cx| {
                if phase.bubble() {
                    dragged_handle.replace(None);
                }
            }
        });
    }
}

impl ParentElement for PaneAxisElement {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}
