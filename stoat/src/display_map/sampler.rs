//! Seeded sampling for the display-map layers' randomized round-trip suites.
//!
//! The layers each pin their coordinate conversions against hand-written
//! fixtures, which only ever cover the configurations someone thought to write
//! down. A sweep over generated buffers reaches the ones nobody did: a fold
//! ending mid-character, a hint on a row of wide glyphs, a tab landing off the
//! expansion grid.

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
