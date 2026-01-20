use crate::{rope::TextSummary, Dimension};
use std::ops::{Add, AddAssign};

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Point {
    pub row: u32,
    pub column: u32,
}

impl Point {
    pub fn new(row: u32, column: u32) -> Self {
        Self { row, column }
    }

    pub fn zero() -> Self {
        Self::default()
    }
}

impl Add for Point {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        if other.row == 0 {
            Point {
                row: self.row,
                column: self.column + other.column,
            }
        } else {
            Point {
                row: self.row + other.row,
                column: other.column,
            }
        }
    }
}

impl AddAssign for Point {
    fn add_assign(&mut self, other: Self) {
        *self = *self + other;
    }
}

impl<'a> Dimension<'a, TextSummary> for Point {
    fn zero(_cx: ()) -> Self {
        Self::default()
    }

    fn add_summary(&mut self, summary: &'a TextSummary, _cx: ()) {
        *self += summary.lines;
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, Hash)]
pub struct PointUtf16 {
    pub row: u32,
    pub column: u32,
}
