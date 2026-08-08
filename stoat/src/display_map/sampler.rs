//! Seeded sampling for the display-map layers' randomized round-trip suites.
//!
//! The layers each pin their coordinate conversions against hand-written
//! fixtures, which only ever cover the configurations someone thought to write
//! down. A sweep over generated buffers reaches the ones nobody did: a fold
//! ending mid-character, a hint on a row of wide glyphs, a tab landing off the
//! expansion grid.

use super::{
    fold_map::{FoldMap, FoldPlaceholder},
    inlay_map::{InlayKind, InlayMap},
    tab_map::TabMap,
    wrap_map::{WrapMap, WrapSnapshot},
};
use crate::{
    buffer::{BufferId, TextBuffer},
    multi_buffer::MultiBuffer,
};
use std::{
    num::NonZeroU32,
    sync::{Arc, RwLock},
};
use stoat_scheduler::{Executor, TestScheduler};
use stoat_text::{patch::Patch, Bias};

/// Characters spanning every UTF-8 width, plus the newlines that make rows and
/// the tabs that make expansions.
///
/// The non-ASCII run holds e-acute and eszett at two bytes, two CJK ideographs
/// at three bytes and two cells, then a G clef and an Old Italic letter at
/// four. Newlines appear twice so rows stay short enough that sweeping every
/// column is cheap. Without a tab the tab layer is a passthrough, and without
/// characters wider than one byte a column confusing bytes for characters reads
/// as correct.
const ALPHABET: [char; 12] = [
    'a',
    'z',
    ' ',
    '\t',
    '\n',
    '\n',
    '\u{e9}',
    '\u{df}',
    '\u{4e16}',
    '\u{754c}',
    '\u{1d11e}',
    '\u{10300}',
];

/// xorshift64 generator, seeded per test so a red run reproduces from the seed
/// printed in its assertion message.
pub(super) struct Sampler(u64);

impl Sampler {
    /// Spreading the seed keeps tests that share a low seed like 1 or 2 from
    /// correlating over their first few draws.
    pub(super) fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    pub(super) fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A value in `0..n`, or 0 when `n` is 0.
    pub(super) fn below(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        (self.next() % n as u64) as u32
    }

    pub(super) fn text(&mut self, len: usize) -> String {
        (0..len)
            .map(|_| ALPHABET[(self.next() as usize) % ALPHABET.len()])
            .collect()
    }
}

/// A generated buffer stacked through the inlay, fold, tab and wrap layers.
///
/// The sampler comes back so a caller can keep drawing from the same seed for
/// whatever it layers on top, which is what lets the block suite generate its
/// blocks without starting a second sequence that would repeat these draws.
///
/// Wrap widths are drawn from a range that mostly misses the tab grid. Tab
/// expansion is modular in the tab size, so a width that is a multiple of it
/// never puts a break inside an expansion, which is where the wrap layer's
/// column arithmetic is hardest.
pub(super) fn random_wrap_stack(seed: u64) -> (Sampler, Arc<WrapSnapshot>) {
    let mut sampler = Sampler::new(seed);
    let len = 1 + sampler.below(60) as usize;
    let text = sampler.text(len);
    let tab_size = NonZeroU32::new(1 + sampler.below(4)).expect("nonzero");
    let wrap_width = match sampler.below(4) {
        0 => None,
        _ => Some(3 + sampler.below(14)),
    };

    let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), &text)));
    let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
    let buffer_snapshot = multi_buffer.snapshot();
    let rope = buffer_snapshot.rope();
    let offset = |sampler: &mut Sampler| {
        rope.clip_offset(sampler.below(rope.len() as u32 + 1) as usize, Bias::Left)
    };

    let (mut inlay_map, _) = InlayMap::new(buffer_snapshot.clone());
    let hint_count = sampler.below(3);
    let hints = (0..hint_count)
        .map(|_| {
            let anchor = buffer_snapshot.anchor_at(offset(&mut sampler), Bias::Right);
            let hint_len = 1 + sampler.below(4) as usize;
            (anchor, sampler.text(hint_len), InlayKind::Hint)
        })
        .collect();
    inlay_map.splice(&buffer_snapshot, Vec::new(), hints);
    let (inlay_snapshot, _) = inlay_map.sync(buffer_snapshot.clone(), &Patch::empty());

    let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());
    let fold_count = sampler.below(3);
    let folds = (0..fold_count)
        .map(|_| {
            let (a, b) = (offset(&mut sampler), offset(&mut sampler));
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            buffer_snapshot.anchor_at(start, Bias::Right)
                ..buffer_snapshot.anchor_at(end, Bias::Left)
        })
        .collect();
    fold_map.fold(folds, FoldPlaceholder::default(), &buffer_snapshot);
    let (fold_snapshot, _) = fold_map.sync(inlay_snapshot, &Patch::empty(), None);

    let (tab_snapshot, _) = TabMap::new(tab_size).sync(fold_snapshot, Patch::empty());
    let (_, wrap_snapshot) = WrapMap::new(
        tab_snapshot,
        wrap_width,
        Executor::new(Arc::new(TestScheduler::new())),
        crate::test_notify(),
    );

    (sampler, wrap_snapshot)
}
