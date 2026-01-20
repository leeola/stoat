use crate::{rope::TextSummary, Dimension};
use std::ops::{Add, AddAssign};

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OffsetUtf16(pub usize);

impl Add for OffsetUtf16 {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        OffsetUtf16(self.0 + other.0)
    }
}

impl AddAssign for OffsetUtf16 {
    fn add_assign(&mut self, other: Self) {
        self.0 += other.0;
    }
}

impl<'a> Dimension<'a, TextSummary> for OffsetUtf16 {
    fn zero(_cx: ()) -> Self {
        Self::default()
    }

    fn add_summary(&mut self, summary: &'a TextSummary, _cx: ()) {
        *self += summary.len_utf16;
    }
}
