// The pool regions a live draw skips, prepended to the passes that draw the live
// grid. A pool composite repaints its region opaque, so a live quad inside one
// shades pixels nobody sees.

// Laid out as render::Cover. Each rect is a pool scissor inset by a pixel on
// every side, as x, y, width, and height in pixels, and the array holds
// render::MAX_COVERED of them.
struct Cover {
    rects: array<vec4<f32>, 8>,
    count: u32,
    occluders: u32,
    pad0: u32,
    pad1: u32,
}

// Whether the quad over the pixels from `lo` to `hi` lies inside one pool region
// and meets none of the first `occluders` occluders.
//
// An occludable pool discards its pixels under a panel, which leaves the live
// grid showing there, so a quad that meets an occluder keeps drawing. The test
// takes each occluder's whole cell rect, which holds the rounded box the panel
// draws.
fn under_pool(lo: vec2<f32>, hi: vec2<f32>) -> bool {
    var inside = false;
    for (var i = 0u; i < globals.cover.count; i = i + 1u) {
        let rect = globals.cover.rects[i];
        if all(lo >= rect.xy) && all(hi <= rect.xy + rect.zw) {
            inside = true;
            break;
        }
    }
    if !inside {
        return false;
    }

    for (var j = 0u; j < globals.cover.occluders; j = j + 1u) {
        let o = occluders[j];
        let o_lo = o.cell * globals.occluder_cell;
        let o_hi = (o.cell + o.size) * globals.occluder_cell;
        if all(lo < o_hi) && all(hi > o_lo) {
            return false;
        }
    }
    return true;
}
