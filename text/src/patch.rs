use std::{
    cmp, mem,
    ops::{Add, AddAssign, Range, Sub},
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Edit<D> {
    pub old: Range<D>,
    pub new: Range<D>,
}

impl<D> Edit<D>
where
    D: PartialEq,
{
    pub fn is_empty(&self) -> bool {
        self.old.start == self.old.end && self.new.start == self.new.end
    }
}

impl<D, DDelta> Edit<D>
where
    D: Sub<D, Output = DDelta> + Copy,
{
    pub fn old_len(&self) -> DDelta {
        self.old.end - self.old.start
    }

    pub fn new_len(&self) -> DDelta {
        self.new.end - self.new.start
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct Patch<T>(Vec<Edit<T>>);

impl<T> Patch<T>
where
    T: 'static + Clone + Copy + Ord + Default,
{
    pub const fn empty() -> Self {
        Self(Vec::new())
    }

    pub fn new(edits: Vec<Edit<T>>) -> Self {
        #[cfg(debug_assertions)]
        {
            let mut last_edit: Option<&Edit<T>> = None;
            for edit in &edits {
                if let Some(last_edit) = last_edit {
                    assert!(edit.old.start > last_edit.old.end);
                    assert!(edit.new.start > last_edit.new.end);
                }
                last_edit = Some(edit);
            }
        }
        Self(edits)
    }

    pub fn edits(&self) -> &[Edit<T>] {
        &self.0
    }

    pub fn into_inner(self) -> Vec<Edit<T>> {
        self.0
    }

    pub fn invert(&mut self) -> &mut Self {
        for edit in &mut self.0 {
            mem::swap(&mut edit.old, &mut edit.new);
        }
        self
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn push(&mut self, edit: Edit<T>) {
        if edit.is_empty() {
            return;
        }

        if let Some(last) = self.0.last_mut() {
            if last.old.end >= edit.old.start {
                last.old.end = edit.old.end;
                last.new.end = edit.new.end;
            } else {
                self.0.push(edit);
            }
        } else {
            self.0.push(edit);
        }
    }

    pub fn consolidate(&mut self) {
        if self.0.len() <= 1 {
            return;
        }
        self.0.sort_unstable_by(|a, b| {
            a.old
                .start
                .cmp(&b.old.start)
                .then_with(|| b.old.end.cmp(&a.old.end))
        });
        let mut write = 0;
        for read in 1..self.0.len() {
            if self.0[write].old.end >= self.0[read].old.start {
                self.0[write].old.end = self.0[write].old.end.max(self.0[read].old.end);
                self.0[write].new.start = self.0[write].new.start.min(self.0[read].new.start);
                self.0[write].new.end = self.0[write].new.end.max(self.0[read].new.end);
            } else {
                write += 1;
                self.0[write] = self.0[read].clone();
            }
        }
        self.0.truncate(write + 1);
    }
}

impl<T, TDelta> Patch<T>
where
    T: 'static
        + Copy
        + Ord
        + Sub<T, Output = TDelta>
        + Add<TDelta, Output = T>
        + AddAssign<TDelta>
        + Default,
    TDelta: Ord + Copy,
{
    #[must_use]
    pub fn compose(&self, new_edits_iter: impl IntoIterator<Item = Edit<T>>) -> Self {
        let mut old_edits_iter = self.0.iter().cloned().peekable();
        let mut new_edits_iter = new_edits_iter.into_iter().peekable();
        let mut composed = Patch(Vec::new());

        let mut old_start = T::default();
        let mut new_start = T::default();
        loop {
            let old_edit = old_edits_iter.peek_mut();
            let new_edit = new_edits_iter.peek_mut();

            if let Some(old_edit) = old_edit.as_ref() {
                let new_edit = new_edit.as_ref();
                if new_edit.is_none_or(|new_edit| old_edit.new.end < new_edit.old.start) {
                    let catchup = old_edit.old.start - old_start;
                    old_start += catchup;
                    new_start += catchup;

                    let old_end = old_start + old_edit.old_len();
                    let new_end = new_start + old_edit.new_len();
                    composed.push(Edit {
                        old: old_start..old_end,
                        new: new_start..new_end,
                    });
                    old_start = old_end;
                    new_start = new_end;
                    old_edits_iter.next();
                    continue;
                }
            }

            if let Some(new_edit) = new_edit.as_ref() {
                let old_edit = old_edit.as_ref();
                if old_edit.is_none_or(|old_edit| new_edit.old.end < old_edit.new.start) {
                    let catchup = new_edit.new.start - new_start;
                    old_start += catchup;
                    new_start += catchup;

                    let old_end = old_start + new_edit.old_len();
                    let new_end = new_start + new_edit.new_len();
                    composed.push(Edit {
                        old: old_start..old_end,
                        new: new_start..new_end,
                    });
                    old_start = old_end;
                    new_start = new_end;
                    new_edits_iter.next();
                    continue;
                }
            }

            if let Some((old_edit, new_edit)) = old_edit.zip(new_edit) {
                if old_edit.new.start < new_edit.old.start {
                    let catchup = old_edit.old.start - old_start;
                    old_start += catchup;
                    new_start += catchup;

                    let overshoot = new_edit.old.start - old_edit.new.start;
                    let old_end = cmp::min(old_start + overshoot, old_edit.old.end);
                    let new_end = new_start + overshoot;
                    composed.push(Edit {
                        old: old_start..old_end,
                        new: new_start..new_end,
                    });

                    old_edit.old.start = old_end;
                    old_edit.new.start += overshoot;
                    old_start = old_end;
                    new_start = new_end;
                } else {
                    let catchup = new_edit.new.start - new_start;
                    old_start += catchup;
                    new_start += catchup;

                    let overshoot = old_edit.new.start - new_edit.old.start;
                    let old_end = old_start + overshoot;
                    let new_end = cmp::min(new_start + overshoot, new_edit.new.end);
                    composed.push(Edit {
                        old: old_start..old_end,
                        new: new_start..new_end,
                    });

                    new_edit.old.start += overshoot;
                    new_edit.new.start = new_end;
                    old_start = old_end;
                    new_start = new_end;
                }

                if old_edit.new.end > new_edit.old.end {
                    let old_end = old_start + cmp::min(old_edit.old_len(), new_edit.old_len());
                    let new_end = new_start + new_edit.new_len();
                    composed.push(Edit {
                        old: old_start..old_end,
                        new: new_start..new_end,
                    });

                    old_edit.old.start = old_end;
                    old_edit.new.start = new_edit.old.end;
                    old_start = old_end;
                    new_start = new_end;
                    new_edits_iter.next();
                } else {
                    let old_end = old_start + old_edit.old_len();
                    let new_end = new_start + cmp::min(old_edit.new_len(), new_edit.new_len());
                    composed.push(Edit {
                        old: old_start..old_end,
                        new: new_start..new_end,
                    });

                    new_edit.old.start = old_edit.new.end;
                    new_edit.new.start = new_end;
                    old_start = old_end;
                    new_start = new_end;
                    old_edits_iter.next();
                }
            } else {
                break;
            }
        }

        composed
    }

    pub fn old_to_new(&self, old: T) -> T {
        let ix = match self.0.binary_search_by(|probe| probe.old.start.cmp(&old)) {
            Ok(ix) => ix,
            Err(ix) => {
                if ix == 0 {
                    return old;
                } else {
                    ix - 1
                }
            },
        };
        if let Some(edit) = self.0.get(ix) {
            if old >= edit.old.end {
                edit.new.end + (old - edit.old.end)
            } else {
                edit.new.start
            }
        } else {
            old
        }
    }
}

impl<T: Clone> IntoIterator for Patch<T> {
    type Item = Edit<T>;
    type IntoIter = std::vec::IntoIter<Edit<T>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a, T: Clone> IntoIterator for &'a Patch<T> {
    type Item = Edit<T>;
    type IntoIter = std::iter::Cloned<std::slice::Iter<'a, Edit<T>>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter().cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::{Edit, Patch};

    #[test]
    fn push_coalesces_adjacent() {
        let mut patch = Patch::empty();
        patch.push(Edit {
            old: 0u32..2,
            new: 0..3,
        });
        patch.push(Edit {
            old: 2..4,
            new: 3..5,
        });
        assert_eq!(
            patch.edits(),
            &[Edit {
                old: 0..4,
                new: 0..5
            }]
        );
    }

    #[test]
    fn push_keeps_disjoint() {
        let mut patch = Patch::empty();
        patch.push(Edit {
            old: 0u32..2,
            new: 0..3,
        });
        patch.push(Edit {
            old: 5..7,
            new: 6..8,
        });
        assert_eq!(patch.edits().len(), 2);
    }

    #[test]
    fn push_skips_empty() {
        let mut patch = Patch::empty();
        patch.push(Edit {
            old: 3u32..3,
            new: 3..3,
        });
        assert!(patch.is_empty());
    }

    #[test]
    fn invert_swaps_old_new() {
        let mut patch = Patch::new(vec![Edit {
            old: 1u32..3,
            new: 1..4,
        }]);
        patch.invert();
        assert_eq!(
            patch.edits(),
            &[Edit {
                old: 1..4,
                new: 1..3
            }]
        );
    }

    #[test]
    fn compose_disjoint_edits() {
        let old = Patch(vec![Edit {
            old: 1u32..3,
            new: 1..4,
        }]);
        let new = Patch(vec![Edit {
            old: 0u32..0,
            new: 0..4,
        }]);
        let composed = old.compose(&new);
        assert_eq!(
            composed,
            Patch(vec![
                Edit {
                    old: 0..0,
                    new: 0..4
                },
                Edit {
                    old: 1..3,
                    new: 5..8
                },
            ])
        );
    }

    #[test]
    fn compose_overlapping_edits() {
        let old = Patch(vec![Edit {
            old: 1u32..3,
            new: 1..4,
        }]);
        let new = Patch(vec![Edit {
            old: 3u32..5,
            new: 3..6,
        }]);
        let composed = old.compose(&new);
        assert_eq!(
            composed,
            Patch(vec![Edit {
                old: 1..4,
                new: 1..6
            }])
        );
    }

    #[test]
    fn old_to_new_mapping() {
        let patch = Patch(vec![
            Edit {
                old: 2u32..4,
                new: 2..4,
            },
            Edit {
                old: 7..8,
                new: 7..11,
            },
        ]);
        assert_eq!(patch.old_to_new(0), 0);
        assert_eq!(patch.old_to_new(1), 1);
        assert_eq!(patch.old_to_new(2), 2);
        assert_eq!(patch.old_to_new(3), 2);
        assert_eq!(patch.old_to_new(4), 4);
        assert_eq!(patch.old_to_new(7), 7);
        assert_eq!(patch.old_to_new(8), 11);
        assert_eq!(patch.old_to_new(9), 12);
    }

    #[test]
    fn identity_compose() {
        let patch = Patch::<u32>::empty();
        let composed = patch.compose(Patch::empty());
        assert!(composed.is_empty());
    }

    fn apply_patch(text: &mut Vec<char>, patch: &Patch<u32>, new_text: &[char]) {
        for edit in patch.edits().iter().rev() {
            text.splice(
                edit.old.start as usize..edit.old.end as usize,
                new_text[edit.new.start as usize..edit.new.end as usize]
                    .iter()
                    .copied(),
            );
        }
    }

    #[test]
    fn consolidate_empty() {
        let mut patch = Patch::<u32>::empty();
        patch.consolidate();
        assert!(patch.is_empty());
    }

    #[test]
    fn consolidate_single() {
        let mut patch = Patch(vec![Edit {
            old: 1u32..3,
            new: 1..4,
        }]);
        patch.consolidate();
        assert_eq!(
            patch.edits(),
            &[Edit {
                old: 1..3,
                new: 1..4
            }]
        );
    }

    #[test]
    fn consolidate_already_sorted_no_overlap() {
        let mut patch = Patch(vec![
            Edit {
                old: 1u32..3,
                new: 1..4,
            },
            Edit {
                old: 5..7,
                new: 6..8,
            },
        ]);
        patch.consolidate();
        assert_eq!(
            patch.edits(),
            &[
                Edit {
                    old: 1..3,
                    new: 1..4
                },
                Edit {
                    old: 5..7,
                    new: 6..8
                },
            ]
        );
    }

    #[test]
    fn consolidate_out_of_order() {
        let mut patch = Patch(vec![
            Edit {
                old: 5u32..7,
                new: 6..8,
            },
            Edit {
                old: 1..3,
                new: 1..4,
            },
        ]);
        patch.consolidate();
        assert_eq!(
            patch.edits(),
            &[
                Edit {
                    old: 1..3,
                    new: 1..4
                },
                Edit {
                    old: 5..7,
                    new: 6..8
                },
            ]
        );
    }

    #[test]
    fn consolidate_overlapping() {
        let mut patch = Patch(vec![
            Edit {
                old: 1u32..5,
                new: 1..6,
            },
            Edit {
                old: 3..7,
                new: 4..8,
            },
        ]);
        patch.consolidate();
        assert_eq!(
            patch.edits(),
            &[Edit {
                old: 1..7,
                new: 1..8
            }]
        );
    }

    #[test]
    fn consolidate_nested() {
        let mut patch = Patch(vec![
            Edit {
                old: 1u32..10,
                new: 1..12,
            },
            Edit {
                old: 3..5,
                new: 3..5,
            },
        ]);
        patch.consolidate();
        assert_eq!(
            patch.edits(),
            &[Edit {
                old: 1..10,
                new: 1..12
            }]
        );
    }

    #[test]
    fn consolidate_adjacent() {
        let mut patch = Patch(vec![
            Edit {
                old: 1u32..3,
                new: 1..4,
            },
            Edit {
                old: 3..5,
                new: 4..7,
            },
        ]);
        patch.consolidate();
        assert_eq!(
            patch.edits(),
            &[Edit {
                old: 1..5,
                new: 1..7
            }]
        );
    }

    #[test]
    fn compose_two_new_edits_overlapping_one_old() {
        let original: Vec<char> = ('a'..='z').collect();
        let inserted: Vec<char> = ('A'..='Z').collect();

        let old = Patch(vec![Edit {
            old: 0u32..0,
            new: 0..3,
        }]);
        let new = Patch(vec![
            Edit {
                old: 0u32..0,
                new: 0..1,
            },
            Edit {
                old: 1..2,
                new: 2..2,
            },
        ]);
        let composed = old.compose(&new);

        let mut expected = original.clone();
        apply_patch(&mut expected, &old, &inserted);
        apply_patch(&mut expected, &new, &inserted);

        let mut actual = original;
        apply_patch(&mut actual, &composed, &expected);
        assert_eq!(actual, expected);
    }
}
