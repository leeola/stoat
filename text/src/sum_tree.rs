use std::{cmp::Ordering, sync::Arc};

const TREE_BASE: usize = 6;

pub trait Summary: Clone {
    type Context<'a>: Copy;
    fn zero(cx: Self::Context<'_>) -> Self;
    fn add_summary(&mut self, other: &Self, cx: Self::Context<'_>);
}

pub trait ContextLessSummary: Clone + Default {
    fn add_summary(&mut self, other: &Self);
}

impl<T: ContextLessSummary> Summary for T {
    type Context<'a> = ();
    fn zero(_cx: ()) -> Self {
        Self::default()
    }
    fn add_summary(&mut self, other: &Self, _cx: ()) {
        ContextLessSummary::add_summary(self, other)
    }
}

pub trait Item: Clone {
    type Summary: Summary;
    fn summary(&self, cx: <Self::Summary as Summary>::Context<'_>) -> Self::Summary;
}

pub trait Dimension<'a, S: Summary>: Clone {
    fn zero(cx: S::Context<'_>) -> Self;
    fn add_summary(&mut self, summary: &'a S, cx: S::Context<'_>);
}

#[derive(Clone, Default, Debug)]
pub struct Dimensions<D1, D2>(pub D1, pub D2);

impl<'a, S, D1, D2> Dimension<'a, S> for Dimensions<D1, D2>
where
    S: Summary,
    D1: Dimension<'a, S>,
    D2: Dimension<'a, S>,
{
    fn zero(cx: S::Context<'_>) -> Self {
        Dimensions(D1::zero(cx), D2::zero(cx))
    }
    fn add_summary(&mut self, summary: &'a S, cx: S::Context<'_>) {
        self.0.add_summary(summary, cx);
        self.1.add_summary(summary, cx);
    }
}

pub trait SeekTarget<'a, S: Summary, D: Dimension<'a, S>> {
    fn cmp(&self, cursor_location: &D, cx: S::Context<'_>) -> Ordering;
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum Bias {
    #[default]
    Left,
    Right,
}

pub struct SumTree<T: Item>(Arc<Node<T>>);

enum Node<T: Item> {
    Leaf {
        items: Vec<T>,
        summaries: Vec<T::Summary>,
        summary: T::Summary,
    },
    Internal {
        children: Vec<SumTree<T>>,
        summary: T::Summary,
    },
}

impl<T: Item> Clone for Node<T> {
    fn clone(&self) -> Self {
        match self {
            Node::Leaf {
                items,
                summaries,
                summary,
            } => Node::Leaf {
                items: items.clone(),
                summaries: summaries.clone(),
                summary: summary.clone(),
            },
            Node::Internal { children, summary } => Node::Internal {
                children: children.clone(),
                summary: summary.clone(),
            },
        }
    }
}

impl<T: Item> Clone for SumTree<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T: Item> SumTree<T> {
    pub fn new(cx: <T::Summary as Summary>::Context<'_>) -> Self {
        Self(Arc::new(Node::Leaf {
            items: Vec::new(),
            summaries: Vec::new(),
            summary: T::Summary::zero(cx),
        }))
    }

    pub fn push(&mut self, item: T, cx: <T::Summary as Summary>::Context<'_>) {
        if let Some(sibling) = self.push_internal(item, cx) {
            let left = Self(Arc::new(std::mem::replace(
                Arc::make_mut(&mut self.0),
                Node::Leaf {
                    items: Vec::new(),
                    summaries: Vec::new(),
                    summary: T::Summary::zero(cx),
                },
            )));
            let mut total_summary = left.summary().clone();
            total_summary.add_summary(sibling.summary(), cx);
            *Arc::make_mut(&mut self.0) = Node::Internal {
                children: vec![left, sibling],
                summary: total_summary,
            };
        }
    }

    fn push_internal(
        &mut self,
        item: T,
        cx: <T::Summary as Summary>::Context<'_>,
    ) -> Option<SumTree<T>> {
        let item_summary = item.summary(cx);
        let node = Arc::make_mut(&mut self.0);
        match node {
            Node::Leaf {
                items,
                summaries,
                summary,
            } => {
                items.push(item);
                summary.add_summary(&item_summary, cx);
                summaries.push(item_summary);

                if items.len() > 2 * TREE_BASE {
                    let midpoint = items.len() / 2;
                    let right_items: Vec<_> = items.drain(midpoint..).collect();
                    let right_summaries: Vec<_> = summaries.drain(midpoint..).collect();

                    *summary = T::Summary::zero(cx);
                    for s in summaries.iter() {
                        summary.add_summary(s, cx);
                    }

                    let mut right_summary = T::Summary::zero(cx);
                    for s in &right_summaries {
                        right_summary.add_summary(s, cx);
                    }

                    return Some(SumTree(Arc::new(Node::Leaf {
                        items: right_items,
                        summaries: right_summaries,
                        summary: right_summary,
                    })));
                }
                None
            },
            Node::Internal { children, summary } => {
                let sibling = children.last_mut()?.push_internal(item, cx);
                summary.add_summary(&item_summary, cx);

                if let Some(sibling) = sibling {
                    children.push(sibling);
                }

                if children.len() > 2 * TREE_BASE {
                    let midpoint = children.len() / 2;
                    let right_children: Vec<_> = children.drain(midpoint..).collect();

                    *summary = T::Summary::zero(cx);
                    for child in children.iter() {
                        summary.add_summary(child.summary(), cx);
                    }

                    let mut right_summary = T::Summary::zero(cx);
                    for child in &right_children {
                        right_summary.add_summary(child.summary(), cx);
                    }

                    return Some(SumTree(Arc::new(Node::Internal {
                        children: right_children,
                        summary: right_summary,
                    })));
                }
                None
            },
        }
    }

    pub fn append(&mut self, other: Self, cx: <T::Summary as Summary>::Context<'_>) {
        if other.is_empty() {
            return;
        }
        if self.is_empty() {
            *self = other;
            return;
        }

        if let Some(sibling) = self.append_internal(other, cx) {
            let left = Self(Arc::new(std::mem::replace(
                Arc::make_mut(&mut self.0),
                Node::Leaf {
                    items: Vec::new(),
                    summaries: Vec::new(),
                    summary: T::Summary::zero(cx),
                },
            )));
            let mut total_summary = left.summary().clone();
            total_summary.add_summary(sibling.summary(), cx);
            *Arc::make_mut(&mut self.0) = Node::Internal {
                children: vec![left, sibling],
                summary: total_summary,
            };
        }
    }

    fn append_internal(
        &mut self,
        other: Self,
        cx: <T::Summary as Summary>::Context<'_>,
    ) -> Option<SumTree<T>> {
        let other_summary = other.summary().clone();
        let node = Arc::make_mut(&mut self.0);
        match node {
            Node::Leaf {
                items,
                summaries,
                summary,
            } => match Arc::try_unwrap(other.0).unwrap_or_else(|arc| (*arc).clone()) {
                Node::Leaf {
                    items: other_items,
                    summaries: other_summaries,
                    ..
                } => {
                    items.extend(other_items);
                    summaries.extend(other_summaries);
                    summary.add_summary(&other_summary, cx);

                    if items.len() > 2 * TREE_BASE {
                        let midpoint = items.len() / 2;
                        let right_items: Vec<_> = items.drain(midpoint..).collect();
                        let right_summaries: Vec<_> = summaries.drain(midpoint..).collect();

                        *summary = T::Summary::zero(cx);
                        for s in summaries.iter() {
                            summary.add_summary(s, cx);
                        }

                        let mut right_summary = T::Summary::zero(cx);
                        for s in &right_summaries {
                            right_summary.add_summary(s, cx);
                        }

                        return Some(SumTree(Arc::new(Node::Leaf {
                            items: right_items,
                            summaries: right_summaries,
                            summary: right_summary,
                        })));
                    }
                    None
                },
                Node::Internal {
                    children: other_children,
                    ..
                } => {
                    let left = SumTree(Arc::new(std::mem::replace(
                        node,
                        Node::Leaf {
                            items: Vec::new(),
                            summaries: Vec::new(),
                            summary: T::Summary::zero(cx),
                        },
                    )));
                    let mut children = vec![left];
                    children.extend(other_children);

                    let mut total_summary = T::Summary::zero(cx);
                    for child in &children {
                        total_summary.add_summary(child.summary(), cx);
                    }

                    if children.len() > 2 * TREE_BASE {
                        let midpoint = children.len() / 2;
                        let right_children: Vec<_> = children.drain(midpoint..).collect();

                        let mut left_summary = T::Summary::zero(cx);
                        for child in &children {
                            left_summary.add_summary(child.summary(), cx);
                        }

                        let mut right_summary = T::Summary::zero(cx);
                        for child in &right_children {
                            right_summary.add_summary(child.summary(), cx);
                        }

                        *node = Node::Internal {
                            children,
                            summary: left_summary,
                        };

                        return Some(SumTree(Arc::new(Node::Internal {
                            children: right_children,
                            summary: right_summary,
                        })));
                    }

                    *node = Node::Internal {
                        children,
                        summary: total_summary,
                    };
                    None
                },
            },
            Node::Internal { children, summary } => {
                children.push(SumTree(Arc::new(
                    Arc::try_unwrap(other.0).unwrap_or_else(|arc| (*arc).clone()),
                )));
                summary.add_summary(&other_summary, cx);

                if children.len() > 2 * TREE_BASE {
                    let midpoint = children.len() / 2;
                    let right_children: Vec<_> = children.drain(midpoint..).collect();

                    *summary = T::Summary::zero(cx);
                    for child in children.iter() {
                        summary.add_summary(child.summary(), cx);
                    }

                    let mut right_summary = T::Summary::zero(cx);
                    for child in &right_children {
                        right_summary.add_summary(child.summary(), cx);
                    }

                    return Some(SumTree(Arc::new(Node::Internal {
                        children: right_children,
                        summary: right_summary,
                    })));
                }
                None
            },
        }
    }

    pub fn summary(&self) -> &T::Summary {
        match self.0.as_ref() {
            Node::Leaf { summary, .. } => summary,
            Node::Internal { summary, .. } => summary,
        }
    }

    pub fn is_empty(&self) -> bool {
        match self.0.as_ref() {
            Node::Leaf { items, .. } => items.is_empty(),
            Node::Internal { children, .. } => children.is_empty(),
        }
    }

    pub fn update_last(
        &mut self,
        f: impl FnOnce(&mut T),
        cx: <T::Summary as Summary>::Context<'_>,
    ) {
        self.update_last_recursive(f, cx);
    }

    fn update_last_recursive(
        &mut self,
        f: impl FnOnce(&mut T),
        cx: <T::Summary as Summary>::Context<'_>,
    ) -> Option<T::Summary> {
        let node = Arc::make_mut(&mut self.0);
        match node {
            Node::Internal { children, summary } => {
                let last_child = children.last_mut()?;
                last_child.update_last_recursive(f, cx)?;
                *summary = T::Summary::zero(cx);
                for child in children.iter() {
                    summary.add_summary(child.summary(), cx);
                }
                Some(summary.clone())
            },
            Node::Leaf {
                items,
                summaries,
                summary,
            } => {
                let (item, item_summary) = items.last_mut().zip(summaries.last_mut())?;
                f(item);
                *item_summary = item.summary(cx);
                *summary = T::Summary::zero(cx);
                for s in summaries.iter() {
                    summary.add_summary(s, cx);
                }
                Some(summary.clone())
            },
        }
    }

    pub fn extent<'a, D: Dimension<'a, T::Summary>>(
        &'a self,
        cx: <T::Summary as Summary>::Context<'_>,
    ) -> D {
        let mut extent = D::zero(cx);
        extent.add_summary(self.summary(), cx);
        extent
    }

    pub fn cursor<'a, 'b, D: Dimension<'a, T::Summary>>(
        &'a self,
        cx: <T::Summary as Summary>::Context<'b>,
    ) -> Cursor<'a, 'b, T, D> {
        Cursor {
            tree: self,
            stack: Vec::new(),
            position: D::zero(cx),
            did_seek: false,
            at_end: false,
            cx,
        }
    }

    #[cfg(test)]
    fn max_children_count(&self) -> usize {
        match self.0.as_ref() {
            Node::Leaf { .. } => 0,
            Node::Internal { children, .. } => {
                let child_max = children
                    .iter()
                    .map(|c| c.max_children_count())
                    .max()
                    .unwrap_or(0);
                children.len().max(child_max)
            },
        }
    }

    #[cfg(test)]
    fn max_items_count(&self) -> usize {
        match self.0.as_ref() {
            Node::Leaf { items, .. } => items.len(),
            Node::Internal { children, .. } => children
                .iter()
                .map(|c| c.max_items_count())
                .max()
                .unwrap_or(0),
        }
    }

    #[cfg(test)]
    fn is_internal(&self) -> bool {
        matches!(self.0.as_ref(), Node::Internal { .. })
    }
}

struct StackEntry<'a, T: Item, D> {
    tree: &'a SumTree<T>,
    index: usize,
    position: D,
}

pub struct Cursor<'a, 'b, T: Item, D> {
    tree: &'a SumTree<T>,
    stack: Vec<StackEntry<'a, T, D>>,
    position: D,
    did_seek: bool,
    at_end: bool,
    cx: <T::Summary as Summary>::Context<'b>,
}

impl<'a, 'b, T: Item, D: Dimension<'a, T::Summary>> Cursor<'a, 'b, T, D> {
    pub fn seek<Target: SeekTarget<'a, T::Summary, D>>(
        &mut self,
        target: &Target,
        bias: Bias,
    ) -> bool {
        self.reset();
        self.seek_internal(target, bias)
    }

    pub fn seek_forward<Target: SeekTarget<'a, T::Summary, D>>(
        &mut self,
        target: &Target,
        bias: Bias,
    ) -> bool {
        if !self.did_seek {
            self.did_seek = true;
        }
        self.seek_internal(target, bias)
    }

    fn reset(&mut self) {
        self.stack.clear();
        self.position = D::zero(self.cx);
        self.did_seek = false;
        self.at_end = self.tree.is_empty();
    }

    fn seek_internal<Target: SeekTarget<'a, T::Summary, D>>(
        &mut self,
        target: &Target,
        bias: Bias,
    ) -> bool {
        if !self.did_seek {
            self.did_seek = true;
            if !self.tree.is_empty() {
                self.stack.push(StackEntry {
                    tree: self.tree,
                    index: 0,
                    position: D::zero(self.cx),
                });
            }
        }

        let mut ascending = false;
        'outer: while let Some(entry) = self.stack.last_mut() {
            match entry.tree.0.as_ref() {
                Node::Internal { children, .. } => {
                    if ascending {
                        entry.index += 1;
                        entry.position = self.position.clone();
                    }

                    let start_index = entry.index;
                    for (ix, child) in children[start_index..].iter().enumerate() {
                        let child_summary = child.summary();
                        let mut child_end = self.position.clone();
                        child_end.add_summary(child_summary, self.cx);

                        let cmp = target.cmp(&child_end, self.cx);
                        if cmp == Ordering::Greater
                            || (cmp == Ordering::Equal && bias == Bias::Right)
                        {
                            self.position = child_end;
                            entry.position = self.position.clone();
                        } else {
                            entry.index = start_index + ix;
                            self.stack.push(StackEntry {
                                tree: child,
                                index: 0,
                                position: self.position.clone(),
                            });
                            ascending = false;
                            continue 'outer;
                        }
                    }
                    entry.index = children.len();
                },
                Node::Leaf { summaries, .. } => {
                    let start_index = entry.index;
                    for (ix, item_summary) in summaries[start_index..].iter().enumerate() {
                        let mut child_end = self.position.clone();
                        child_end.add_summary(item_summary, self.cx);

                        let cmp = target.cmp(&child_end, self.cx);
                        if cmp == Ordering::Greater
                            || (cmp == Ordering::Equal && bias == Bias::Right)
                        {
                            self.position = child_end;
                        } else {
                            entry.index = start_index + ix;
                            break 'outer;
                        }
                    }
                    entry.index = summaries.len();
                },
            }
            self.stack.pop();
            ascending = true;
        }

        self.at_end = self.stack.is_empty();

        let mut end = self.position.clone();
        if let Some(summary) = self.item_summary() {
            end.add_summary(summary, self.cx);
        }
        target.cmp(&end, self.cx) == Ordering::Equal
    }

    pub fn next(&mut self) {
        if self.at_end {
            return;
        }

        if !self.did_seek {
            self.did_seek = true;
            return;
        }

        while let Some(entry) = self.stack.pop() {
            match entry.tree.0.as_ref() {
                Node::Leaf { summaries, .. } => {
                    let next_index = entry.index + 1;
                    if next_index < summaries.len() {
                        self.position.add_summary(&summaries[entry.index], self.cx);
                        self.stack.push(StackEntry {
                            tree: entry.tree,
                            index: next_index,
                            position: self.position.clone(),
                        });
                        return;
                    }
                },
                Node::Internal { children, .. } => {
                    let next_index = entry.index + 1;
                    if next_index < children.len() {
                        self.position
                            .add_summary(children[entry.index].summary(), self.cx);
                        self.stack.push(StackEntry {
                            tree: entry.tree,
                            index: next_index,
                            position: self.position.clone(),
                        });
                        self.descend_to_first_item(&children[next_index]);
                        return;
                    }
                },
            }
        }
        self.at_end = true;
    }

    fn descend_to_first_item(&mut self, tree: &'a SumTree<T>) {
        let mut current = tree;
        loop {
            match current.0.as_ref() {
                Node::Leaf { .. } => {
                    self.stack.push(StackEntry {
                        tree: current,
                        index: 0,
                        position: self.position.clone(),
                    });
                    break;
                },
                Node::Internal { children, .. } => {
                    if children.is_empty() {
                        break;
                    }
                    self.stack.push(StackEntry {
                        tree: current,
                        index: 0,
                        position: self.position.clone(),
                    });
                    current = &children[0];
                },
            }
        }
    }

    pub fn item(&self) -> Option<&'a T> {
        if self.at_end || !self.did_seek {
            return None;
        }
        self.stack
            .last()
            .and_then(|entry| match entry.tree.0.as_ref() {
                Node::Leaf { items, .. } => items.get(entry.index),
                Node::Internal { .. } => None,
            })
    }

    fn item_summary(&self) -> Option<&'a T::Summary> {
        if self.at_end || !self.did_seek {
            return None;
        }
        self.stack
            .last()
            .and_then(|entry| match entry.tree.0.as_ref() {
                Node::Leaf { summaries, .. } => summaries.get(entry.index),
                Node::Internal { .. } => None,
            })
    }

    pub fn start(&self) -> &D {
        &self.position
    }

    pub fn end(&self) -> D {
        if let Some(item_summary) = self.item_summary() {
            let mut end = self.position.clone();
            end.add_summary(item_summary, self.cx);
            end
        } else {
            self.position.clone()
        }
    }

    pub fn slice<Target: SeekTarget<'a, T::Summary, D>>(
        &mut self,
        end: &Target,
        bias: Bias,
    ) -> SumTree<T> {
        let mut result = SumTree::new(self.cx);

        if !self.did_seek {
            self.did_seek = true;
        }

        while !self.at_end {
            if let (Some(item), Some(item_summary)) = (self.item(), self.item_summary()) {
                let mut end_position = self.position.clone();
                end_position.add_summary(item_summary, self.cx);

                let cmp = end.cmp(&end_position, self.cx);
                if cmp == Ordering::Less || (cmp == Ordering::Equal && bias == Bias::Left) {
                    break;
                }
                result.push(item.clone(), self.cx);
                self.position = end_position;
            }
            self.next();
        }

        result
    }

    pub fn suffix(&mut self) -> SumTree<T> {
        let mut result = SumTree::new(self.cx);

        if !self.did_seek {
            self.did_seek = true;
        }

        while let Some(item) = self.item() {
            result.push(item.clone(), self.cx);
            self.next();
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::{Bias, ContextLessSummary, Dimension, Item, SeekTarget, SumTree, TREE_BASE};
    use std::cmp::Ordering;

    #[derive(Clone, Debug)]
    struct TestItem;

    #[derive(Clone, Default, Debug)]
    struct TestSummary {
        count: usize,
    }

    impl ContextLessSummary for TestSummary {
        fn add_summary(&mut self, other: &Self) {
            self.count += other.count;
        }
    }

    impl Item for TestItem {
        type Summary = TestSummary;
        fn summary(&self, _cx: ()) -> TestSummary {
            TestSummary { count: 1 }
        }
    }

    impl<'a> Dimension<'a, TestSummary> for usize {
        fn zero(_cx: ()) -> Self {
            0
        }
        fn add_summary(&mut self, summary: &'a TestSummary, _cx: ()) {
            *self += summary.count;
        }
    }

    impl<'a> SeekTarget<'a, TestSummary, usize> for usize {
        fn cmp(&self, cursor_location: &usize, _cx: ()) -> Ordering {
            Ord::cmp(self, cursor_location)
        }
    }

    #[test]
    fn push_maintains_balance() {
        let mut tree: SumTree<TestItem> = SumTree::new(());

        for _ in 0..1000 {
            tree.push(TestItem, ());
        }

        assert_eq!(tree.summary().count, 1000);
        assert!(
            tree.max_children_count() <= 2 * TREE_BASE,
            "tree has {} children, max allowed is {}",
            tree.max_children_count(),
            2 * TREE_BASE
        );
    }

    #[test]
    fn append_leaf_respects_size_limit() {
        let mut tree1: SumTree<TestItem> = SumTree::new(());
        let mut tree2: SumTree<TestItem> = SumTree::new(());

        for _ in 0..(2 * TREE_BASE) {
            tree1.push(TestItem, ());
            tree2.push(TestItem, ());
        }

        tree1.append(tree2, ());

        assert_eq!(tree1.summary().count, 4 * TREE_BASE);
        assert!(
            tree1.max_items_count() <= 2 * TREE_BASE,
            "leaf has {} items, max allowed is {}",
            tree1.max_items_count(),
            2 * TREE_BASE
        );
    }

    #[test]
    fn append_internal_to_leaf() {
        let mut leaf: SumTree<TestItem> = SumTree::new(());
        for _ in 0..5 {
            leaf.push(TestItem, ());
        }
        assert!(!leaf.is_internal());

        let mut internal: SumTree<TestItem> = SumTree::new(());
        for _ in 0..100 {
            internal.push(TestItem, ());
        }
        assert!(internal.is_internal());

        leaf.append(internal, ());

        assert_eq!(leaf.summary().count, 105);
        assert!(
            leaf.max_children_count() <= 2 * TREE_BASE,
            "tree has {} children, max allowed is {}",
            leaf.max_children_count(),
            2 * TREE_BASE
        );
        assert!(
            leaf.max_items_count() <= 2 * TREE_BASE,
            "leaf has {} items, max allowed is {}",
            leaf.max_items_count(),
            2 * TREE_BASE
        );
    }

    #[test]
    fn seek_past_end_position_is_consistent() {
        let mut tree: SumTree<TestItem> = SumTree::new(());
        for _ in 0..100 {
            tree.push(TestItem, ());
        }
        assert!(tree.is_internal(), "need internal nodes for this test");

        let extent: usize = tree.extent(());
        assert_eq!(extent, 100);

        let mut cursor = tree.cursor::<usize>(());
        let found = cursor.seek(&200usize, Bias::Right);
        assert!(!found, "should not find position past end");
        assert!(cursor.item().is_none(), "item should be None when past end");
        assert_eq!(*cursor.start(), extent, "position should equal tree extent");
    }
}
