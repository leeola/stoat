use arrayvec::ArrayVec;
use std::{cmp::Ordering, fmt, marker::PhantomData, mem, sync::Arc};

#[cfg(test)]
const TREE_BASE: usize = 2;
#[cfg(not(test))]
const TREE_BASE: usize = 6;

pub trait Summary: Clone {
    type Context<'a>: Copy;
    fn zero(cx: Self::Context<'_>) -> Self;
    fn add_summary(&mut self, other: &Self, cx: Self::Context<'_>);
}

pub trait ContextLessSummary: Clone + Default {
    fn add_summary(&mut self, other: &Self);
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct NoSummary;

impl ContextLessSummary for NoSummary {
    fn add_summary(&mut self, _: &Self) {}
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

pub trait KeyedItem: Item {
    type Key: for<'a> Dimension<'a, Self::Summary> + Ord;
    fn key(&self) -> Self::Key;
}

pub trait Dimension<'a, S: Summary>: Clone {
    fn zero(cx: S::Context<'_>) -> Self;
    fn add_summary(&mut self, summary: &'a S, cx: S::Context<'_>);

    fn from_summary(summary: &'a S, cx: S::Context<'_>) -> Self {
        let mut dim = Self::zero(cx);
        dim.add_summary(summary, cx);
        dim
    }

    #[must_use]
    fn with_added_summary(mut self, summary: &'a S, cx: S::Context<'_>) -> Self {
        self.add_summary(summary, cx);
        self
    }
}

impl<'a, T: Summary> Dimension<'a, T> for T {
    fn zero(cx: T::Context<'_>) -> Self {
        Summary::zero(cx)
    }
    fn add_summary(&mut self, summary: &'a T, cx: T::Context<'_>) {
        Summary::add_summary(self, summary, cx);
    }
}

impl<'a, T: Summary> Dimension<'a, T> for () {
    fn zero(_: T::Context<'_>) {}
    fn add_summary(&mut self, _: &'a T, _: T::Context<'_>) {}
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Dimensions<D1, D2, D3 = ()>(pub D1, pub D2, pub D3);

impl<'a, S, D1, D2, D3> Dimension<'a, S> for Dimensions<D1, D2, D3>
where
    S: Summary,
    D1: Dimension<'a, S>,
    D2: Dimension<'a, S>,
    D3: Dimension<'a, S>,
{
    fn zero(cx: S::Context<'_>) -> Self {
        Dimensions(D1::zero(cx), D2::zero(cx), D3::zero(cx))
    }
    fn add_summary(&mut self, summary: &'a S, cx: S::Context<'_>) {
        self.0.add_summary(summary, cx);
        self.1.add_summary(summary, cx);
        self.2.add_summary(summary, cx);
    }
}

pub trait SeekTarget<'a, S: Summary, D: Dimension<'a, S>> {
    fn cmp(&self, cursor_location: &D, cx: S::Context<'_>) -> Ordering;
}

impl<'a, S: Summary, D: Dimension<'a, S> + Ord> SeekTarget<'a, S, D> for D {
    fn cmp(&self, cursor_location: &Self, _: S::Context<'_>) -> Ordering {
        Ord::cmp(self, cursor_location)
    }
}

impl<'a, S, D1, D2, D3> SeekTarget<'a, S, Dimensions<D1, D2, D3>> for D1
where
    S: Summary,
    D1: SeekTarget<'a, S, D1> + Dimension<'a, S>,
    D2: Dimension<'a, S>,
    D3: Dimension<'a, S>,
{
    fn cmp(&self, cursor_location: &Dimensions<D1, D2, D3>, cx: S::Context<'_>) -> Ordering {
        SeekTarget::cmp(self, &cursor_location.0, cx)
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Bias {
    #[default]
    Left,
    Right,
}

impl Bias {
    pub fn invert(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

pub struct SumTree<T: Item>(Arc<Node<T>>);

impl<T: Item> Clone for SumTree<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T: Item + fmt::Debug> fmt::Debug for SumTree<T>
where
    T::Summary: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

enum Node<T: Item> {
    Internal {
        height: u8,
        summary: T::Summary,
        child_summaries: ArrayVec<T::Summary, { 2 * TREE_BASE }>,
        child_trees: ArrayVec<SumTree<T>, { 2 * TREE_BASE }>,
    },
    Leaf {
        summary: T::Summary,
        items: ArrayVec<T, { 2 * TREE_BASE }>,
        item_summaries: ArrayVec<T::Summary, { 2 * TREE_BASE }>,
    },
}

impl<T: Item> Clone for Node<T> {
    fn clone(&self) -> Self {
        match self {
            Node::Internal {
                height,
                summary,
                child_summaries,
                child_trees,
            } => Node::Internal {
                height: *height,
                summary: summary.clone(),
                child_summaries: child_summaries.clone(),
                child_trees: child_trees.clone(),
            },
            Node::Leaf {
                summary,
                items,
                item_summaries,
            } => Node::Leaf {
                summary: summary.clone(),
                items: items.clone(),
                item_summaries: item_summaries.clone(),
            },
        }
    }
}

impl<T: Item + fmt::Debug> fmt::Debug for Node<T>
where
    T::Summary: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Node::Internal {
                height,
                summary,
                child_trees,
                ..
            } => f
                .debug_struct("Internal")
                .field("height", height)
                .field("summary", summary)
                .field("child_trees", child_trees)
                .finish(),
            Node::Leaf { summary, items, .. } => f
                .debug_struct("Leaf")
                .field("summary", summary)
                .field("items", items)
                .finish(),
        }
    }
}

impl<T: Item> Node<T> {
    fn is_leaf(&self) -> bool {
        matches!(self, Node::Leaf { .. })
    }

    fn height(&self) -> u8 {
        match self {
            Node::Internal { height, .. } => *height,
            Node::Leaf { .. } => 0,
        }
    }

    fn summary(&self) -> &T::Summary {
        match self {
            Node::Internal { summary, .. } | Node::Leaf { summary, .. } => summary,
        }
    }

    fn child_summaries(&self) -> &[T::Summary] {
        match self {
            Node::Internal {
                child_summaries, ..
            } => child_summaries,
            Node::Leaf { item_summaries, .. } => item_summaries,
        }
    }

    fn child_trees(&self) -> &[SumTree<T>] {
        match self {
            Node::Internal { child_trees, .. } => child_trees,
            Node::Leaf { .. } => panic!("leaf nodes have no child trees"),
        }
    }

    fn items(&self) -> &[T] {
        match self {
            Node::Leaf { items, .. } => items,
            Node::Internal { .. } => panic!("internal nodes have no items"),
        }
    }

    fn is_underflowing(&self) -> bool {
        match self {
            Node::Internal { child_trees, .. } => child_trees.len() < TREE_BASE,
            Node::Leaf { items, .. } => items.len() < TREE_BASE,
        }
    }
}

impl<T: Item> SumTree<T> {
    pub fn new(cx: <T::Summary as Summary>::Context<'_>) -> Self {
        SumTree(Arc::new(Node::Leaf {
            summary: <T::Summary as Summary>::zero(cx),
            items: ArrayVec::new(),
            item_summaries: ArrayVec::new(),
        }))
    }

    pub fn from_summary(summary: T::Summary) -> Self {
        SumTree(Arc::new(Node::Leaf {
            summary,
            items: ArrayVec::new(),
            item_summaries: ArrayVec::new(),
        }))
    }

    pub fn from_item(item: T, cx: <T::Summary as Summary>::Context<'_>) -> Self {
        let mut tree = Self::new(cx);
        tree.push(item, cx);
        tree
    }

    pub fn from_iter<I: IntoIterator<Item = T>>(
        iter: I,
        cx: <T::Summary as Summary>::Context<'_>,
    ) -> Self {
        let mut nodes = Vec::new();
        let mut iter = iter.into_iter().fuse().peekable();
        while iter.peek().is_some() {
            let items: ArrayVec<T, { 2 * TREE_BASE }> = iter.by_ref().take(2 * TREE_BASE).collect();
            let item_summaries: ArrayVec<T::Summary, { 2 * TREE_BASE }> =
                items.iter().map(|item| item.summary(cx)).collect();
            let mut summary = item_summaries[0].clone();
            for item_summary in &item_summaries[1..] {
                Summary::add_summary(&mut summary, item_summary, cx);
            }
            nodes.push(SumTree(Arc::new(Node::Leaf {
                summary,
                items,
                item_summaries,
            })));
        }

        let mut parent_nodes = Vec::new();
        let mut height: u8 = 0;
        while nodes.len() > 1 {
            height += 1;
            let mut current_parent_node: Option<SumTree<T>> = None;
            for child_node in nodes.drain(..) {
                let parent_node = current_parent_node.get_or_insert_with(|| {
                    SumTree(Arc::new(Node::Internal {
                        summary: <T::Summary as Summary>::zero(cx),
                        height,
                        child_summaries: ArrayVec::new(),
                        child_trees: ArrayVec::new(),
                    }))
                });
                let Node::Internal {
                    summary,
                    child_summaries,
                    child_trees,
                    ..
                } = Arc::get_mut(&mut parent_node.0).expect("sole owner of new Arc")
                else {
                    unreachable!()
                };
                let child_summary = child_node.summary();
                Summary::add_summary(summary, child_summary, cx);
                child_summaries.push(child_summary.clone());
                child_trees.push(child_node);

                if child_trees.len() == 2 * TREE_BASE {
                    parent_nodes.extend(current_parent_node.take());
                }
            }
            parent_nodes.extend(current_parent_node.take());
            mem::swap(&mut nodes, &mut parent_nodes);
        }

        if nodes.is_empty() {
            Self::new(cx)
        } else {
            nodes.pop().expect("checked non-empty above")
        }
    }

    pub fn summary(&self) -> &T::Summary {
        self.0.summary()
    }

    pub fn is_empty(&self) -> bool {
        match self.0.as_ref() {
            Node::Internal { .. } => false,
            Node::Leaf { items, .. } => items.is_empty(),
        }
    }

    pub fn first(&self) -> Option<&T> {
        self.leftmost_leaf().0.items().first()
    }

    pub fn last(&self) -> Option<&T> {
        self.rightmost_leaf().0.items().last()
    }

    pub fn last_summary(&self) -> Option<&T::Summary> {
        self.rightmost_leaf().0.child_summaries().last()
    }

    pub fn items(&self, cx: <T::Summary as Summary>::Context<'_>) -> Vec<T> {
        let mut items = Vec::new();
        let mut cursor = self.cursor::<()>(cx);
        cursor.next();
        while let Some(item) = cursor.item() {
            items.push(item.clone());
            cursor.next();
        }
        items
    }

    pub fn iter(&self) -> Iter<'_, T> {
        Iter::new(self)
    }

    pub fn extent<'a, D: Dimension<'a, T::Summary>>(
        &'a self,
        cx: <T::Summary as Summary>::Context<'_>,
    ) -> D {
        D::from_summary(self.summary(), cx)
    }

    pub fn cursor<'a, 'b, D: Dimension<'a, T::Summary>>(
        &'a self,
        cx: <T::Summary as Summary>::Context<'b>,
    ) -> Cursor<'a, 'b, T, D> {
        Cursor::new(self, cx)
    }

    pub fn filter<'a, 'b, F, U>(
        &'a self,
        cx: <T::Summary as Summary>::Context<'b>,
        filter_node: F,
    ) -> FilterCursor<'a, 'b, F, T, U>
    where
        F: FnMut(&T::Summary) -> bool,
        U: Dimension<'a, T::Summary>,
    {
        FilterCursor::new(self, cx, filter_node)
    }

    pub fn find<'a, D, Target>(
        &'a self,
        cx: <T::Summary as Summary>::Context<'_>,
        target: &Target,
        bias: Bias,
    ) -> (D, D, Option<&'a T>)
    where
        D: Dimension<'a, T::Summary>,
        Target: SeekTarget<'a, T::Summary, D>,
    {
        let tree_end = D::zero(cx).with_added_summary(self.summary(), cx);
        let cmp = target.cmp(&tree_end, cx);
        if cmp == Ordering::Greater || (cmp == Ordering::Equal && bias == Bias::Right) {
            return (tree_end.clone(), tree_end, None);
        }
        let mut pos = D::zero(cx);
        match Self::find_iterate::<_, _, false>(cx, target, bias, &mut pos, self) {
            Some((item, end)) => (pos, end, Some(item)),
            None => (pos.clone(), pos, None),
        }
    }

    pub fn find_exact<'a, D, Target>(
        &'a self,
        cx: <T::Summary as Summary>::Context<'_>,
        target: &Target,
        bias: Bias,
    ) -> (D, D, Option<&'a T>)
    where
        D: Dimension<'a, T::Summary>,
        Target: SeekTarget<'a, T::Summary, D>,
    {
        let tree_end = D::zero(cx).with_added_summary(self.summary(), cx);
        let cmp = target.cmp(&tree_end, cx);
        if cmp == Ordering::Greater || (cmp == Ordering::Equal && bias == Bias::Right) {
            return (tree_end.clone(), tree_end, None);
        }
        let mut pos = D::zero(cx);
        match Self::find_iterate::<_, _, true>(cx, target, bias, &mut pos, self) {
            Some((item, end)) => (pos, end, Some(item)),
            None => (pos.clone(), pos, None),
        }
    }

    pub fn find_with_prev<'a, D, Target>(
        &'a self,
        cx: <T::Summary as Summary>::Context<'_>,
        target: &Target,
        bias: Bias,
    ) -> (D, D, Option<(Option<&'a T>, &'a T)>)
    where
        D: Dimension<'a, T::Summary>,
        Target: SeekTarget<'a, T::Summary, D>,
    {
        let tree_end = D::zero(cx).with_added_summary(self.summary(), cx);
        let cmp = target.cmp(&tree_end, cx);
        if cmp == Ordering::Greater || (cmp == Ordering::Equal && bias == Bias::Right) {
            return (tree_end.clone(), tree_end, None);
        }
        let mut pos = D::zero(cx);
        match Self::find_with_prev_iterate::<_, _, false>(cx, target, bias, &mut pos, self) {
            Some((prev, item, end)) => (pos, end, Some((prev, item))),
            None => (pos.clone(), pos, None),
        }
    }

    fn find_with_prev_iterate<'tree, 'cx, D, Target, const EXACT: bool>(
        cx: <T::Summary as Summary>::Context<'cx>,
        target: &Target,
        bias: Bias,
        position: &mut D,
        mut this: &'tree SumTree<T>,
    ) -> Option<(Option<&'tree T>, &'tree T, D)>
    where
        D: Dimension<'tree, T::Summary>,
        Target: SeekTarget<'tree, T::Summary, D>,
    {
        let mut prev = None;
        'iterate: loop {
            match this.0.as_ref() {
                Node::Internal {
                    child_summaries,
                    child_trees,
                    ..
                } => {
                    for (child_tree, child_summary) in child_trees.iter().zip(child_summaries) {
                        let child_end = position.clone().with_added_summary(child_summary, cx);
                        let cmp = target.cmp(&child_end, cx);
                        if cmp == Ordering::Less || (cmp == Ordering::Equal && bias == Bias::Left) {
                            this = child_tree;
                            continue 'iterate;
                        }
                        prev = child_tree.last();
                        *position = child_end;
                    }
                },
                Node::Leaf {
                    items,
                    item_summaries,
                    ..
                } => {
                    for (item, item_summary) in items.iter().zip(item_summaries) {
                        let child_end = position.clone().with_added_summary(item_summary, cx);
                        let cmp = target.cmp(&child_end, cx);
                        let found = if EXACT {
                            cmp == Ordering::Equal
                        } else {
                            cmp == Ordering::Less || (cmp == Ordering::Equal && bias == Bias::Left)
                        };
                        if found {
                            return Some((prev, item, child_end));
                        }
                        prev = Some(item);
                        *position = child_end;
                    }
                },
            }
            return None;
        }
    }

    fn find_iterate<'tree, 'cx, D, Target, const EXACT: bool>(
        cx: <T::Summary as Summary>::Context<'cx>,
        target: &Target,
        bias: Bias,
        position: &mut D,
        mut this: &'tree SumTree<T>,
    ) -> Option<(&'tree T, D)>
    where
        D: Dimension<'tree, T::Summary>,
        Target: SeekTarget<'tree, T::Summary, D>,
    {
        'iterate: loop {
            match this.0.as_ref() {
                Node::Internal {
                    child_summaries,
                    child_trees,
                    ..
                } => {
                    for (child_tree, child_summary) in child_trees.iter().zip(child_summaries) {
                        let child_end = position.clone().with_added_summary(child_summary, cx);
                        let cmp = target.cmp(&child_end, cx);
                        if cmp == Ordering::Less || (cmp == Ordering::Equal && bias == Bias::Left) {
                            this = child_tree;
                            continue 'iterate;
                        }
                        *position = child_end;
                    }
                },
                Node::Leaf {
                    items,
                    item_summaries,
                    ..
                } => {
                    for (item, item_summary) in items.iter().zip(item_summaries) {
                        let child_end = position.clone().with_added_summary(item_summary, cx);
                        let cmp = target.cmp(&child_end, cx);
                        let found = if EXACT {
                            cmp == Ordering::Equal
                        } else {
                            cmp == Ordering::Less || (cmp == Ordering::Equal && bias == Bias::Left)
                        };
                        if found {
                            return Some((item, child_end));
                        }
                        *position = child_end;
                    }
                },
            }
            return None;
        }
    }

    pub fn push(&mut self, item: T, cx: <T::Summary as Summary>::Context<'_>) {
        let item_summary = item.summary(cx);
        self.append(
            SumTree(Arc::new(Node::Leaf {
                summary: item_summary.clone(),
                items: ArrayVec::from_iter([item]),
                item_summaries: ArrayVec::from_iter([item_summary]),
            })),
            cx,
        );
    }

    pub fn extend<I: IntoIterator<Item = T>>(
        &mut self,
        iter: I,
        cx: <T::Summary as Summary>::Context<'_>,
    ) {
        self.append(Self::from_iter(iter, cx), cx);
    }

    pub fn append(&mut self, mut other: Self, cx: <T::Summary as Summary>::Context<'_>) {
        if self.is_empty() {
            *self = other;
        } else if !other.0.is_leaf() || !other.0.items().is_empty() {
            if self.0.height() < other.0.height() {
                if let Some(tree) = Self::append_large(self.clone(), &mut other, cx) {
                    *self = Self::from_child_trees(tree, other, cx);
                } else {
                    *self = other;
                }
            } else if let Some(split_tree) = self.push_tree_recursive(other, cx) {
                *self = Self::from_child_trees(self.clone(), split_tree, cx);
            }
        }
    }

    fn push_tree_recursive(
        &mut self,
        other: SumTree<T>,
        cx: <T::Summary as Summary>::Context<'_>,
    ) -> Option<SumTree<T>> {
        match Arc::make_mut(&mut self.0) {
            Node::Internal {
                height,
                summary,
                child_summaries,
                child_trees,
            } => {
                let other_node = other.0.clone();
                Summary::add_summary(summary, other_node.summary(), cx);

                let height_delta = *height - other_node.height();
                let mut summaries_to_append: ArrayVec<T::Summary, { 2 * TREE_BASE }> =
                    ArrayVec::new();
                let mut trees_to_append: ArrayVec<SumTree<T>, { 2 * TREE_BASE }> = ArrayVec::new();
                if height_delta == 0 {
                    summaries_to_append.extend(other_node.child_summaries().iter().cloned());
                    trees_to_append.extend(other_node.child_trees().iter().cloned());
                } else if height_delta == 1 && !other_node.is_underflowing() {
                    summaries_to_append.push(other_node.summary().clone());
                    trees_to_append.push(other);
                } else {
                    let tree_to_append = child_trees
                        .last_mut()
                        .expect("internal node has children")
                        .push_tree_recursive(other, cx);
                    *child_summaries
                        .last_mut()
                        .expect("internal node has children") = child_trees
                        .last()
                        .expect("internal node has children")
                        .0
                        .summary()
                        .clone();

                    if let Some(split_tree) = tree_to_append {
                        summaries_to_append.push(split_tree.0.summary().clone());
                        trees_to_append.push(split_tree);
                    }
                }

                let child_count = child_trees.len() + trees_to_append.len();
                if child_count > 2 * TREE_BASE {
                    let midpoint = (child_count + child_count % 2) / 2;
                    let mut all_summaries = child_summaries
                        .iter()
                        .chain(summaries_to_append.iter())
                        .cloned();
                    let left_summaries: ArrayVec<_, { 2 * TREE_BASE }> =
                        all_summaries.by_ref().take(midpoint).collect();
                    let right_summaries: ArrayVec<_, { 2 * TREE_BASE }> = all_summaries.collect();
                    let mut all_trees = child_trees.iter().chain(trees_to_append.iter()).cloned();
                    let left_trees: ArrayVec<_, { 2 * TREE_BASE }> =
                        all_trees.by_ref().take(midpoint).collect();
                    let right_trees: ArrayVec<_, { 2 * TREE_BASE }> = all_trees.collect();

                    *summary = sum(left_summaries.iter(), cx);
                    *child_summaries = left_summaries;
                    *child_trees = left_trees;

                    Some(SumTree(Arc::new(Node::Internal {
                        height: *height,
                        summary: sum(right_summaries.iter(), cx),
                        child_summaries: right_summaries,
                        child_trees: right_trees,
                    })))
                } else {
                    child_summaries.extend(summaries_to_append);
                    child_trees.extend(trees_to_append);
                    None
                }
            },
            Node::Leaf {
                summary,
                items,
                item_summaries,
            } => {
                let other_node = other.0;
                let child_count = items.len() + other_node.items().len();
                if child_count > 2 * TREE_BASE {
                    let midpoint = (child_count + child_count % 2) / 2;
                    let mut all_items = items.iter().chain(other_node.items().iter()).cloned();
                    let left_items: ArrayVec<_, { 2 * TREE_BASE }> =
                        all_items.by_ref().take(midpoint).collect();
                    let right_items: ArrayVec<_, { 2 * TREE_BASE }> = all_items.collect();
                    let mut all_summaries = item_summaries
                        .iter()
                        .chain(other_node.child_summaries())
                        .cloned();
                    let left_summaries: ArrayVec<_, { 2 * TREE_BASE }> =
                        all_summaries.by_ref().take(midpoint).collect();
                    let right_summaries: ArrayVec<_, { 2 * TREE_BASE }> = all_summaries.collect();

                    *items = left_items;
                    *item_summaries = left_summaries;
                    *summary = sum(item_summaries.iter(), cx);

                    Some(SumTree(Arc::new(Node::Leaf {
                        items: right_items,
                        summary: sum(right_summaries.iter(), cx),
                        item_summaries: right_summaries,
                    })))
                } else {
                    Summary::add_summary(summary, other_node.summary(), cx);
                    items.extend(other_node.items().iter().cloned());
                    item_summaries.extend(other_node.child_summaries().iter().cloned());
                    None
                }
            },
        }
    }

    fn append_large(
        small: Self,
        large: &mut Self,
        cx: <T::Summary as Summary>::Context<'_>,
    ) -> Option<Self> {
        if small.0.height() == large.0.height() {
            if !small.0.is_underflowing() {
                Some(small)
            } else {
                Self::merge_into_right(small, large, cx)
            }
        } else {
            let Node::Internal {
                height,
                summary,
                child_summaries,
                child_trees,
            } = Arc::make_mut(&mut large.0)
            else {
                unreachable!();
            };
            let mut full_summary = small.summary().clone();
            Summary::add_summary(&mut full_summary, summary, cx);
            *summary = full_summary;

            let first = child_trees.first_mut().expect("internal node has children");
            let res = Self::append_large(small, first, cx);
            *child_summaries
                .first_mut()
                .expect("internal node has children") = first.summary().clone();
            if let Some(tree) = res {
                if child_trees.len() < 2 * TREE_BASE {
                    child_summaries.insert(0, tree.summary().clone());
                    child_trees.insert(0, tree);
                    None
                } else {
                    let mut new_child_summaries: ArrayVec<_, { 2 * TREE_BASE }> = ArrayVec::new();
                    new_child_summaries.push(tree.summary().clone());
                    new_child_summaries.extend(child_summaries.drain(..TREE_BASE));
                    let mut new_child_trees: ArrayVec<_, { 2 * TREE_BASE }> = ArrayVec::new();
                    new_child_trees.push(tree);
                    new_child_trees.extend(child_trees.drain(..TREE_BASE));
                    let tree = SumTree(Arc::new(Node::Internal {
                        height: *height,
                        summary: sum(new_child_summaries.iter(), cx),
                        child_summaries: new_child_summaries,
                        child_trees: new_child_trees,
                    }));
                    *summary = sum(child_summaries.iter(), cx);
                    Some(tree)
                }
            } else {
                None
            }
        }
    }

    fn merge_into_right(
        small: Self,
        large: &mut Self,
        cx: <T::Summary as Summary>::Context<'_>,
    ) -> Option<SumTree<T>> {
        match (small.0.as_ref(), Arc::make_mut(&mut large.0)) {
            (
                Node::Internal {
                    summary: small_summary,
                    child_summaries: small_child_summaries,
                    child_trees: small_child_trees,
                    ..
                },
                Node::Internal {
                    summary,
                    child_summaries,
                    child_trees,
                    height,
                },
            ) => {
                let total = child_trees.len() + small_child_trees.len();
                if total <= 2 * TREE_BASE {
                    let mut all_trees = small_child_trees.clone();
                    all_trees.extend(child_trees.drain(..));
                    *child_trees = all_trees;
                    let mut all_summaries = small_child_summaries.clone();
                    all_summaries.extend(child_summaries.drain(..));
                    *child_summaries = all_summaries;
                    let mut full = small_summary.clone();
                    Summary::add_summary(&mut full, summary, cx);
                    *summary = full;
                    None
                } else {
                    let midpoint = total.div_ceil(2);
                    let mut all_trees = small_child_trees.iter().chain(child_trees.iter()).cloned();
                    let left_trees: ArrayVec<_, { 2 * TREE_BASE }> =
                        all_trees.by_ref().take(midpoint).collect();
                    *child_trees = all_trees.collect();
                    let mut all_summaries = small_child_summaries
                        .iter()
                        .chain(child_summaries.iter())
                        .cloned();
                    let left_summaries: ArrayVec<_, { 2 * TREE_BASE }> =
                        all_summaries.by_ref().take(midpoint).collect();
                    *child_summaries = all_summaries.collect();
                    *summary = sum(child_summaries.iter(), cx);
                    Some(SumTree(Arc::new(Node::Internal {
                        height: *height,
                        summary: sum(left_summaries.iter(), cx),
                        child_summaries: left_summaries,
                        child_trees: left_trees,
                    })))
                }
            },
            (
                Node::Leaf {
                    summary: small_summary,
                    items: small_items,
                    item_summaries: small_item_summaries,
                },
                Node::Leaf {
                    summary,
                    items,
                    item_summaries,
                },
            ) => {
                let total = small_items.len() + items.len();
                if total <= 2 * TREE_BASE {
                    let mut all_items = small_items.clone();
                    all_items.extend(items.drain(..));
                    *items = all_items;
                    let mut all_summaries = small_item_summaries.clone();
                    all_summaries.extend(item_summaries.drain(..));
                    *item_summaries = all_summaries;
                    let mut full = small_summary.clone();
                    Summary::add_summary(&mut full, summary, cx);
                    *summary = full;
                    None
                } else {
                    let midpoint = total.div_ceil(2);
                    let mut all_items = small_items.iter().chain(items.iter()).cloned();
                    let left_items: ArrayVec<_, { 2 * TREE_BASE }> =
                        all_items.by_ref().take(midpoint).collect();
                    *items = all_items.collect();
                    let mut all_summaries = small_item_summaries
                        .iter()
                        .chain(item_summaries.iter())
                        .cloned();
                    let left_summaries: ArrayVec<_, { 2 * TREE_BASE }> =
                        all_summaries.by_ref().take(midpoint).collect();
                    *item_summaries = all_summaries.collect();
                    *summary = sum(item_summaries.iter(), cx);
                    Some(SumTree(Arc::new(Node::Leaf {
                        items: left_items,
                        summary: sum(left_summaries.iter(), cx),
                        item_summaries: left_summaries,
                    })))
                }
            },
            _ => unreachable!(),
        }
    }

    fn from_child_trees(
        left: SumTree<T>,
        right: SumTree<T>,
        cx: <T::Summary as Summary>::Context<'_>,
    ) -> Self {
        let height = left.0.height() + 1;
        let mut child_summaries = ArrayVec::new();
        child_summaries.push(left.0.summary().clone());
        child_summaries.push(right.0.summary().clone());
        let summary = sum(child_summaries.iter(), cx);
        let mut child_trees = ArrayVec::new();
        child_trees.push(left);
        child_trees.push(right);
        SumTree(Arc::new(Node::Internal {
            height,
            summary,
            child_summaries,
            child_trees,
        }))
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
        match Arc::make_mut(&mut self.0) {
            Node::Internal {
                summary,
                child_summaries,
                child_trees,
                ..
            } => {
                let last_summary = child_summaries
                    .last_mut()
                    .expect("internal node has children");
                let last_child = child_trees.last_mut().expect("internal node has children");
                *last_summary = last_child
                    .update_last_recursive(f, cx)
                    .expect("non-empty tree returns summary");
                *summary = sum(child_summaries.iter(), cx);
                Some(summary.clone())
            },
            Node::Leaf {
                summary,
                items,
                item_summaries,
            } => {
                let (item, item_summary) = items.last_mut().zip(item_summaries.last_mut())?;
                f(item);
                *item_summary = item.summary(cx);
                *summary = sum(item_summaries.iter(), cx);
                Some(summary.clone())
            },
        }
    }

    pub fn update_first(
        &mut self,
        f: impl FnOnce(&mut T),
        cx: <T::Summary as Summary>::Context<'_>,
    ) {
        self.update_first_recursive(f, cx);
    }

    fn update_first_recursive(
        &mut self,
        f: impl FnOnce(&mut T),
        cx: <T::Summary as Summary>::Context<'_>,
    ) -> Option<T::Summary> {
        match Arc::make_mut(&mut self.0) {
            Node::Internal {
                summary,
                child_summaries,
                child_trees,
                ..
            } => {
                let first_summary = child_summaries
                    .first_mut()
                    .expect("internal node has children");
                let first_child = child_trees.first_mut().expect("internal node has children");
                *first_summary = first_child
                    .update_first_recursive(f, cx)
                    .expect("non-empty tree returns summary");
                *summary = sum(child_summaries.iter(), cx);
                Some(summary.clone())
            },
            Node::Leaf {
                summary,
                items,
                item_summaries,
            } => {
                let (item, item_summary) = items.first_mut().zip(item_summaries.first_mut())?;
                f(item);
                *item_summary = item.summary(cx);
                *summary = sum(item_summaries.iter(), cx);
                Some(summary.clone())
            },
        }
    }

    fn leftmost_leaf(&self) -> &Self {
        match self.0.as_ref() {
            Node::Leaf { .. } => self,
            Node::Internal { child_trees, .. } => child_trees
                .first()
                .expect("internal node has children")
                .leftmost_leaf(),
        }
    }

    fn rightmost_leaf(&self) -> &Self {
        match self.0.as_ref() {
            Node::Leaf { .. } => self,
            Node::Internal { child_trees, .. } => child_trees
                .last()
                .expect("internal node has children")
                .rightmost_leaf(),
        }
    }

    #[cfg(test)]
    fn max_children_count(&self) -> usize {
        match self.0.as_ref() {
            Node::Leaf { .. } => 0,
            Node::Internal { child_trees, .. } => {
                let child_max = child_trees
                    .iter()
                    .map(|c| c.max_children_count())
                    .max()
                    .unwrap_or(0);
                child_trees.len().max(child_max)
            },
        }
    }

    #[cfg(test)]
    fn max_items_count(&self) -> usize {
        match self.0.as_ref() {
            Node::Leaf { items, .. } => items.len(),
            Node::Internal { child_trees, .. } => child_trees
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

impl<T, S> Default for SumTree<T>
where
    T: Item<Summary = S>,
    S: for<'a> Summary<Context<'a> = ()>,
{
    fn default() -> Self {
        Self::new(())
    }
}

impl<T: Item + PartialEq> PartialEq for SumTree<T> {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}

impl<T: Item + Eq> Eq for SumTree<T> {}

// KeyedItem operations

impl<T: KeyedItem> SumTree<T> {
    pub fn insert_or_replace(
        &mut self,
        item: T,
        cx: <T::Summary as Summary>::Context<'_>,
    ) -> Option<T> {
        let mut replaced = None;
        let mut cursor = self.cursor::<T::Key>(cx);
        let mut new_tree = cursor.slice(&item.key(), Bias::Left);
        if let Some(cursor_item) = cursor.item() {
            if cursor_item.key() == item.key() {
                replaced = Some(cursor_item.clone());
                cursor.next();
            }
        }
        new_tree.push(item, cx);
        new_tree.append(cursor.suffix(), cx);
        drop(cursor);
        *self = new_tree;
        replaced
    }

    pub fn remove(&mut self, key: &T::Key, cx: <T::Summary as Summary>::Context<'_>) -> Option<T> {
        let mut removed = None;
        *self = {
            let mut cursor = self.cursor::<T::Key>(cx);
            let mut new_tree = cursor.slice(key, Bias::Left);
            if let Some(item) = cursor.item() {
                if item.key() == *key {
                    removed = Some(item.clone());
                    cursor.next();
                }
            }
            new_tree.append(cursor.suffix(), cx);
            new_tree
        };
        removed
    }

    pub fn edit(
        &mut self,
        mut edits: Vec<Edit<T>>,
        cx: <T::Summary as Summary>::Context<'_>,
    ) -> Vec<T> {
        if edits.is_empty() {
            return Vec::new();
        }

        let mut removed = Vec::new();
        edits.sort_unstable_by_key(|item| item.key());

        *self = {
            let mut cursor = self.cursor::<T::Key>(cx);
            let mut new_tree = SumTree::new(cx);
            let mut buffered_items = Vec::new();

            cursor.seek(&T::Key::zero(cx), Bias::Left);
            for edit in edits {
                let new_key = edit.key();
                let mut old_item = cursor.item();

                if old_item.is_some_and(|old| old.key() < new_key) {
                    new_tree.extend(buffered_items.drain(..), cx);
                    let slice = cursor.slice(&new_key, Bias::Left);
                    new_tree.append(slice, cx);
                    old_item = cursor.item();
                }

                if let Some(old) = old_item {
                    if old.key() == new_key {
                        removed.push(old.clone());
                        cursor.next();
                    }
                }

                match edit {
                    Edit::Insert(item) => buffered_items.push(item),
                    Edit::Remove(_) => {},
                }
            }

            new_tree.extend(buffered_items, cx);
            new_tree.append(cursor.suffix(), cx);
            new_tree
        };

        removed
    }

    pub fn get<'a>(
        &'a self,
        key: &T::Key,
        cx: <T::Summary as Summary>::Context<'a>,
    ) -> Option<&'a T> {
        let (_, _, item) = self.find_exact::<T::Key, _>(cx, key, Bias::Left);
        item
    }
}

// Edit

#[derive(Debug)]
pub enum Edit<T: KeyedItem> {
    Insert(T),
    Remove(T::Key),
}

impl<T: KeyedItem> Edit<T> {
    fn key(&self) -> T::Key {
        match self {
            Edit::Insert(item) => item.key(),
            Edit::Remove(key) => key.clone(),
        }
    }
}

// Cursor

#[derive(Clone)]
struct StackEntry<'a, T: Item, D> {
    tree: &'a SumTree<T>,
    index: u32,
    position: D,
}

impl<'a, T: Item, D> StackEntry<'a, T, D> {
    #[inline]
    fn index(&self) -> usize {
        self.index as usize
    }
}

impl<T: Item, D: fmt::Debug> fmt::Debug for StackEntry<'_, T, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StackEntry")
            .field("index", &self.index)
            .field("position", &self.position)
            .finish()
    }
}

pub struct Cursor<'a, 'b, T: Item, D> {
    tree: &'a SumTree<T>,
    stack: ArrayVec<StackEntry<'a, T, D>, 16>,
    position: D,
    did_seek: bool,
    at_end: bool,
    cx: <T::Summary as Summary>::Context<'b>,
}

impl<T: Item, D: fmt::Debug> fmt::Debug for Cursor<'_, '_, T, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cursor")
            .field("position", &self.position)
            .field("did_seek", &self.did_seek)
            .field("at_end", &self.at_end)
            .finish()
    }
}

impl<'a, 'b, T: Item, D: Dimension<'a, T::Summary>> Cursor<'a, 'b, T, D> {
    fn new(tree: &'a SumTree<T>, cx: <T::Summary as Summary>::Context<'b>) -> Self {
        Self {
            tree,
            stack: ArrayVec::new(),
            position: D::zero(cx),
            did_seek: false,
            at_end: tree.is_empty(),
            cx,
        }
    }

    pub fn did_seek(&self) -> bool {
        self.did_seek
    }

    pub fn reset(&mut self) {
        self.did_seek = false;
        self.at_end = self.tree.is_empty();
        self.stack.clear();
        self.position = D::zero(self.cx);
    }

    pub fn start(&self) -> &D {
        &self.position
    }

    #[track_caller]
    pub fn end(&self) -> D {
        if let Some(item_summary) = self.item_summary() {
            let mut end = self.position.clone();
            end.add_summary(item_summary, self.cx);
            end
        } else {
            self.position.clone()
        }
    }

    #[track_caller]
    pub fn item(&self) -> Option<&'a T> {
        if !self.did_seek {
            return None;
        }
        self.stack
            .last()
            .and_then(|entry| match entry.tree.0.as_ref() {
                Node::Leaf { items, .. } => items.get(entry.index()),
                _ => None,
            })
    }

    #[track_caller]
    pub fn item_summary(&self) -> Option<&'a T::Summary> {
        if !self.did_seek {
            return None;
        }
        self.stack
            .last()
            .and_then(|entry| match entry.tree.0.as_ref() {
                Node::Leaf { item_summaries, .. } => item_summaries.get(entry.index()),
                _ => None,
            })
    }

    #[track_caller]
    pub fn next_item(&self) -> Option<&'a T> {
        if !self.did_seek {
            return None;
        }
        if let Some(entry) = self.stack.last() {
            let items = entry.tree.0.items();
            if entry.index() + 1 < items.len() {
                Some(&items[entry.index() + 1])
            } else if let Some(next_leaf) = self.next_leaf() {
                next_leaf.0.items().first()
            } else {
                None
            }
        } else if self.at_end {
            None
        } else {
            self.tree.first()
        }
    }

    #[track_caller]
    pub fn prev_item(&self) -> Option<&'a T> {
        if !self.did_seek {
            return None;
        }
        if let Some(entry) = self.stack.last() {
            if entry.index > 0 {
                let items = entry.tree.0.items();
                Some(&items[entry.index() - 1])
            } else if let Some(prev_leaf) = self.prev_leaf() {
                prev_leaf.0.items().last()
            } else {
                None
            }
        } else if self.at_end {
            self.tree.last()
        } else {
            None
        }
    }

    #[track_caller]
    fn next_leaf(&self) -> Option<&'a SumTree<T>> {
        for entry in self.stack.iter().rev().skip(1) {
            let child_trees = entry.tree.0.child_trees();
            if entry.index() + 1 < child_trees.len() {
                return Some(child_trees[entry.index() + 1].leftmost_leaf());
            }
        }
        None
    }

    #[track_caller]
    fn prev_leaf(&self) -> Option<&'a SumTree<T>> {
        for entry in self.stack.iter().rev().skip(1) {
            if entry.index > 0 {
                let child_trees = entry.tree.0.child_trees();
                return Some(child_trees[entry.index() - 1].rightmost_leaf());
            }
        }
        None
    }

    #[track_caller]
    pub fn next(&mut self) {
        self.search_forward(|_| true)
    }

    #[track_caller]
    pub fn prev(&mut self) {
        self.search_backward(|_| true)
    }

    #[track_caller]
    pub fn search_forward<F>(&mut self, mut filter_node: F)
    where
        F: FnMut(&T::Summary) -> bool,
    {
        let mut descend = false;

        if self.stack.is_empty() {
            if !self.at_end {
                self.stack.push(StackEntry {
                    tree: self.tree,
                    index: 0,
                    position: D::zero(self.cx),
                });
                descend = true;
            }
            self.did_seek = true;
        }

        while !self.stack.is_empty() {
            let new_subtree = {
                let entry = self.stack.last_mut().expect("loop guard checks non-empty");
                match entry.tree.0.as_ref() {
                    Node::Internal {
                        child_trees,
                        child_summaries,
                        ..
                    } => {
                        if !descend {
                            entry.index += 1;
                            entry.position = self.position.clone();
                        }

                        while entry.index() < child_summaries.len() {
                            let next_summary = &child_summaries[entry.index()];
                            if filter_node(next_summary) {
                                break;
                            } else {
                                entry.index += 1;
                                entry.position.add_summary(next_summary, self.cx);
                                self.position.add_summary(next_summary, self.cx);
                            }
                        }

                        child_trees.get(entry.index())
                    },
                    Node::Leaf { item_summaries, .. } => {
                        if !descend {
                            let item_summary = &item_summaries[entry.index()];
                            entry.index += 1;
                            entry.position.add_summary(item_summary, self.cx);
                            self.position.add_summary(item_summary, self.cx);
                        }

                        loop {
                            if let Some(next_item_summary) = item_summaries.get(entry.index()) {
                                if filter_node(next_item_summary) {
                                    return;
                                } else {
                                    entry.index += 1;
                                    entry.position.add_summary(next_item_summary, self.cx);
                                    self.position.add_summary(next_item_summary, self.cx);
                                }
                            } else {
                                break None;
                            }
                        }
                    },
                }
            };

            if let Some(subtree) = new_subtree {
                descend = true;
                self.stack.push(StackEntry {
                    tree: subtree,
                    index: 0,
                    position: self.position.clone(),
                });
            } else {
                descend = false;
                self.stack.pop();
            }
        }

        self.at_end = self.stack.is_empty();
    }

    #[track_caller]
    pub fn search_backward<F>(&mut self, mut filter_node: F)
    where
        F: FnMut(&T::Summary) -> bool,
    {
        if !self.did_seek {
            self.did_seek = true;
            self.at_end = true;
        }

        if self.at_end {
            self.position = D::zero(self.cx);
            self.at_end = self.tree.is_empty();
            if !self.tree.is_empty() {
                self.stack.push(StackEntry {
                    tree: self.tree,
                    index: self.tree.0.child_summaries().len() as u32,
                    position: D::from_summary(self.tree.summary(), self.cx),
                });
            }
        }

        let mut descending = false;
        while !self.stack.is_empty() {
            if let Some(StackEntry { position, .. }) = self.stack.iter().rev().nth(1) {
                self.position = position.clone();
            } else {
                self.position = D::zero(self.cx);
            }

            let entry = self.stack.last_mut().expect("loop guard checks non-empty");
            if !descending {
                if entry.index == 0 {
                    self.stack.pop();
                    continue;
                } else {
                    entry.index -= 1;
                }
            }

            for s in &entry.tree.0.child_summaries()[..entry.index()] {
                self.position.add_summary(s, self.cx);
            }
            entry.position = self.position.clone();

            descending = filter_node(&entry.tree.0.child_summaries()[entry.index()]);
            match entry.tree.0.as_ref() {
                Node::Internal { child_trees, .. } => {
                    if descending {
                        let tree = &child_trees[entry.index()];
                        self.stack.push(StackEntry {
                            position: D::zero(self.cx),
                            tree,
                            index: (tree.0.child_summaries().len() - 1) as u32,
                        });
                    }
                },
                Node::Leaf { .. } => {
                    if descending {
                        break;
                    }
                },
            }
        }
    }

    #[track_caller]
    pub fn seek<Target: SeekTarget<'a, T::Summary, D>>(
        &mut self,
        target: &Target,
        bias: Bias,
    ) -> bool {
        self.reset();
        self.seek_internal(target, bias, &mut ())
    }

    #[track_caller]
    pub fn seek_forward<Target: SeekTarget<'a, T::Summary, D>>(
        &mut self,
        target: &Target,
        bias: Bias,
    ) -> bool {
        self.seek_internal(target, bias, &mut ())
    }

    #[track_caller]
    pub fn slice<Target: SeekTarget<'a, T::Summary, D>>(
        &mut self,
        end: &Target,
        bias: Bias,
    ) -> SumTree<T> {
        let mut slice = SliceSeekAggregate {
            tree: SumTree::new(self.cx),
            leaf_items: ArrayVec::new(),
            leaf_item_summaries: ArrayVec::new(),
            leaf_summary: <T::Summary as Summary>::zero(self.cx),
        };
        self.seek_internal(end, bias, &mut slice);
        slice.tree
    }

    #[track_caller]
    pub fn suffix(&mut self) -> SumTree<T> {
        self.slice(&End::new(), Bias::Right)
    }

    #[track_caller]
    pub fn summary<Target, Output>(&mut self, end: &Target, bias: Bias) -> Output
    where
        Target: SeekTarget<'a, T::Summary, D>,
        Output: Dimension<'a, T::Summary>,
    {
        let mut agg = SummarySeekAggregate(Output::zero(self.cx));
        self.seek_internal(end, bias, &mut agg);
        agg.0
    }

    fn seek_internal(
        &mut self,
        target: &dyn SeekTarget<'a, T::Summary, D>,
        bias: Bias,
        aggregate: &mut dyn SeekAggregate<'a, T>,
    ) -> bool {
        assert!(
            target.cmp(&self.position, self.cx).is_ge(),
            "cannot seek backward",
        );

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
                Node::Internal {
                    child_summaries,
                    child_trees,
                    ..
                } => {
                    if ascending {
                        entry.index += 1;
                        entry.position = self.position.clone();
                    }

                    for (child_tree, child_summary) in child_trees[entry.index()..]
                        .iter()
                        .zip(&child_summaries[entry.index()..])
                    {
                        let mut child_end = self.position.clone();
                        child_end.add_summary(child_summary, self.cx);

                        let cmp = target.cmp(&child_end, self.cx);
                        if cmp == Ordering::Greater
                            || (cmp == Ordering::Equal && bias == Bias::Right)
                        {
                            self.position = child_end;
                            aggregate.push_tree(child_tree, child_summary, self.cx);
                            entry.index += 1;
                            entry.position = self.position.clone();
                        } else {
                            self.stack.push(StackEntry {
                                tree: child_tree,
                                index: 0,
                                position: self.position.clone(),
                            });
                            ascending = false;
                            continue 'outer;
                        }
                    }
                },
                Node::Leaf {
                    items,
                    item_summaries,
                    ..
                } => {
                    aggregate.begin_leaf();

                    for (item, item_summary) in items[entry.index()..]
                        .iter()
                        .zip(&item_summaries[entry.index()..])
                    {
                        let mut child_end = self.position.clone();
                        child_end.add_summary(item_summary, self.cx);

                        let cmp = target.cmp(&child_end, self.cx);
                        if cmp == Ordering::Greater
                            || (cmp == Ordering::Equal && bias == Bias::Right)
                        {
                            self.position = child_end;
                            aggregate.push_item(item, item_summary, self.cx);
                            entry.index += 1;
                        } else {
                            aggregate.end_leaf(self.cx);
                            break 'outer;
                        }
                    }

                    aggregate.end_leaf(self.cx);
                },
            }
            self.stack.pop();
            ascending = true;
        }

        self.at_end = self.stack.is_empty();

        let mut end = self.position.clone();
        if bias == Bias::Left {
            if let Some(s) = self.item_summary() {
                end.add_summary(s, self.cx);
            }
        }
        target.cmp(&end, self.cx) == Ordering::Equal
    }
}

impl<'a, 'b, T: Item, D: Dimension<'a, T::Summary>> Iterator for Cursor<'a, 'b, T, D> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.did_seek {
            Cursor::next(self);
        }
        if let Some(item) = self.item() {
            Cursor::next(self);
            Some(item)
        } else {
            None
        }
    }
}

// Iter

pub struct Iter<'a, T: Item> {
    tree: &'a SumTree<T>,
    stack: ArrayVec<StackEntry<'a, T, ()>, 16>,
}

impl<'a, T: Item> Iter<'a, T> {
    fn new(tree: &'a SumTree<T>) -> Self {
        Self {
            tree,
            stack: ArrayVec::new(),
        }
    }
}

impl<'a, T: Item> Iterator for Iter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        let mut descend = false;

        if self.stack.is_empty() {
            self.stack.push(StackEntry {
                tree: self.tree,
                index: 0,
                position: (),
            });
            descend = true;
        }

        while let Some(entry) = self.stack.last_mut() {
            let new_subtree = match entry.tree.0.as_ref() {
                Node::Internal { child_trees, .. } => {
                    if !descend {
                        entry.index += 1;
                    }
                    child_trees.get(entry.index())
                },
                Node::Leaf { items, .. } => {
                    if !descend {
                        entry.index += 1;
                    }
                    if let Some(item) = items.get(entry.index()) {
                        return Some(item);
                    }
                    None
                },
            };

            if let Some(subtree) = new_subtree {
                descend = true;
                self.stack.push(StackEntry {
                    tree: subtree,
                    index: 0,
                    position: (),
                });
            } else {
                descend = false;
                self.stack.pop();
            }
        }

        None
    }

    fn last(mut self) -> Option<Self::Item> {
        self.stack.clear();
        self.tree.rightmost_leaf().0.items().last()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let lower_bound = match self.stack.last() {
            Some(top) => top.tree.0.child_summaries().len() - top.index(),
            None => self.tree.0.child_summaries().len(),
        };
        (lower_bound, None)
    }
}

// FilterCursor

pub struct FilterCursor<'a, 'b, F, T: Item, D> {
    cursor: Cursor<'a, 'b, T, D>,
    filter_node: F,
}

impl<'a, 'b, F, T, D> FilterCursor<'a, 'b, F, T, D>
where
    F: FnMut(&T::Summary) -> bool,
    T: Item,
    D: Dimension<'a, T::Summary>,
{
    fn new(tree: &'a SumTree<T>, cx: <T::Summary as Summary>::Context<'b>, filter_node: F) -> Self {
        let cursor = tree.cursor::<D>(cx);
        Self {
            cursor,
            filter_node,
        }
    }

    pub fn start(&self) -> &D {
        self.cursor.start()
    }

    pub fn end(&self) -> D {
        self.cursor.end()
    }

    pub fn item(&self) -> Option<&'a T> {
        self.cursor.item()
    }

    pub fn item_summary(&self) -> Option<&'a T::Summary> {
        self.cursor.item_summary()
    }

    pub fn next(&mut self) {
        self.cursor.search_forward(&mut self.filter_node);
    }

    pub fn prev(&mut self) {
        self.cursor.search_backward(&mut self.filter_node);
    }
}

impl<'a, 'b, F, T: Item, U> Iterator for FilterCursor<'a, 'b, F, T, U>
where
    F: FnMut(&T::Summary) -> bool,
    U: Dimension<'a, T::Summary>,
{
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.cursor.did_seek {
            FilterCursor::next(self);
        }
        if let Some(item) = self.item() {
            self.cursor.search_forward(&mut self.filter_node);
            Some(item)
        } else {
            None
        }
    }
}

// SeekAggregate (private)

trait SeekAggregate<'a, T: Item> {
    fn begin_leaf(&mut self);
    fn end_leaf(&mut self, cx: <T::Summary as Summary>::Context<'_>);
    fn push_item(
        &mut self,
        item: &'a T,
        summary: &'a T::Summary,
        cx: <T::Summary as Summary>::Context<'_>,
    );
    fn push_tree(
        &mut self,
        tree: &'a SumTree<T>,
        summary: &'a T::Summary,
        cx: <T::Summary as Summary>::Context<'_>,
    );
}

impl<T: Item> SeekAggregate<'_, T> for () {
    fn begin_leaf(&mut self) {}
    fn end_leaf(&mut self, _: <T::Summary as Summary>::Context<'_>) {}
    fn push_item(&mut self, _: &T, _: &T::Summary, _: <T::Summary as Summary>::Context<'_>) {}
    fn push_tree(
        &mut self,
        _: &SumTree<T>,
        _: &T::Summary,
        _: <T::Summary as Summary>::Context<'_>,
    ) {
    }
}

struct SliceSeekAggregate<T: Item> {
    tree: SumTree<T>,
    leaf_items: ArrayVec<T, { 2 * TREE_BASE }>,
    leaf_item_summaries: ArrayVec<T::Summary, { 2 * TREE_BASE }>,
    leaf_summary: T::Summary,
}

impl<T: Item> SeekAggregate<'_, T> for SliceSeekAggregate<T> {
    fn begin_leaf(&mut self) {}

    fn end_leaf(&mut self, cx: <T::Summary as Summary>::Context<'_>) {
        self.tree.append(
            SumTree(Arc::new(Node::Leaf {
                summary: mem::replace(&mut self.leaf_summary, <T::Summary as Summary>::zero(cx)),
                items: mem::take(&mut self.leaf_items),
                item_summaries: mem::take(&mut self.leaf_item_summaries),
            })),
            cx,
        );
    }

    fn push_item(
        &mut self,
        item: &T,
        summary: &T::Summary,
        cx: <T::Summary as Summary>::Context<'_>,
    ) {
        self.leaf_items.push(item.clone());
        self.leaf_item_summaries.push(summary.clone());
        Summary::add_summary(&mut self.leaf_summary, summary, cx);
    }

    fn push_tree(
        &mut self,
        tree: &SumTree<T>,
        _: &T::Summary,
        cx: <T::Summary as Summary>::Context<'_>,
    ) {
        self.tree.append(tree.clone(), cx);
    }
}

struct SummarySeekAggregate<D>(D);

impl<'a, T: Item, D: Dimension<'a, T::Summary>> SeekAggregate<'a, T> for SummarySeekAggregate<D> {
    fn begin_leaf(&mut self) {}
    fn end_leaf(&mut self, _: <T::Summary as Summary>::Context<'_>) {}
    fn push_item(
        &mut self,
        _: &T,
        summary: &'a T::Summary,
        cx: <T::Summary as Summary>::Context<'_>,
    ) {
        self.0.add_summary(summary, cx);
    }
    fn push_tree(
        &mut self,
        _: &SumTree<T>,
        summary: &'a T::Summary,
        cx: <T::Summary as Summary>::Context<'_>,
    ) {
        self.0.add_summary(summary, cx);
    }
}

struct End<D>(PhantomData<D>);

impl<D> End<D> {
    fn new() -> Self {
        Self(PhantomData)
    }
}

impl<'a, S: Summary, D: Dimension<'a, S>> SeekTarget<'a, S, D> for End<D> {
    fn cmp(&self, _: &D, _: S::Context<'_>) -> Ordering {
        Ordering::Greater
    }
}

fn sum<'a, T, I>(iter: I, cx: T::Context<'_>) -> T
where
    T: 'a + Summary,
    I: Iterator<Item = &'a T>,
{
    let mut result = T::zero(cx);
    for value in iter {
        result.add_summary(value, cx);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{Bias, ContextLessSummary, Dimension, Edit, Item, KeyedItem, SumTree, TREE_BASE};
    use std::cmp::{self, Ordering};

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

    #[derive(Clone, Default, Debug)]
    pub struct IntegersSummary {
        count: usize,
        sum: usize,
        contains_even: bool,
        max: u8,
    }

    impl ContextLessSummary for IntegersSummary {
        fn add_summary(&mut self, other: &Self) {
            self.count += other.count;
            self.sum += other.sum;
            self.contains_even |= other.contains_even;
            self.max = cmp::max(self.max, other.max);
        }
    }

    impl Item for u8 {
        type Summary = IntegersSummary;
        fn summary(&self, _cx: ()) -> IntegersSummary {
            IntegersSummary {
                count: 1,
                sum: *self as usize,
                contains_even: (*self & 1) == 0,
                max: *self,
            }
        }
    }

    impl KeyedItem for u8 {
        type Key = u8;
        fn key(&self) -> u8 {
            *self
        }
    }

    #[derive(Ord, PartialOrd, Default, Eq, PartialEq, Clone, Debug)]
    struct Count(usize);

    impl<'a> Dimension<'a, IntegersSummary> for Count {
        fn zero(_cx: ()) -> Self {
            Default::default()
        }
        fn add_summary(&mut self, summary: &'a IntegersSummary, _cx: ()) {
            self.0 += summary.count;
        }
    }

    impl<'a> super::SeekTarget<'a, IntegersSummary, IntegersSummary> for Count {
        fn cmp(&self, cursor_location: &IntegersSummary, _: ()) -> Ordering {
            Ord::cmp(&self.0, &cursor_location.count)
        }
    }

    #[derive(Ord, PartialOrd, Default, Eq, PartialEq, Clone, Debug)]
    struct Sum(usize);

    impl<'a> Dimension<'a, IntegersSummary> for Sum {
        fn zero(_cx: ()) -> Self {
            Default::default()
        }
        fn add_summary(&mut self, summary: &'a IntegersSummary, _cx: ()) {
            self.0 += summary.sum;
        }
    }

    impl<'a> Dimension<'a, IntegersSummary> for u8 {
        fn zero(_cx: ()) -> Self {
            0
        }
        fn add_summary(&mut self, summary: &'a IntegersSummary, _: ()) {
            *self = summary.max;
        }
    }

    #[test]
    fn push_maintains_balance() {
        let mut tree: SumTree<TestItem> = SumTree::new(());
        for _ in 0..1000 {
            tree.push(TestItem, ());
        }
        assert_eq!(tree.summary().count, 1000);
        assert!(tree.max_children_count() <= 2 * TREE_BASE);
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
        assert!(tree1.max_items_count() <= 2 * TREE_BASE);
    }

    #[test]
    fn append_internal_to_leaf() {
        let mut leaf: SumTree<TestItem> = SumTree::new(());
        for _ in 0..(TREE_BASE - 1) {
            leaf.push(TestItem, ());
        }
        assert!(!leaf.is_internal());
        let mut internal: SumTree<TestItem> = SumTree::new(());
        for _ in 0..(4 * TREE_BASE + 1) {
            internal.push(TestItem, ());
        }
        assert!(internal.is_internal());
        leaf.append(internal, ());
        assert_eq!(leaf.summary().count, 5 * TREE_BASE);
        assert!(leaf.max_children_count() <= 2 * TREE_BASE);
        assert!(leaf.max_items_count() <= 2 * TREE_BASE);
    }

    #[test]
    fn seek_past_end_position_is_consistent() {
        let mut tree: SumTree<TestItem> = SumTree::new(());
        for _ in 0..100 {
            tree.push(TestItem, ());
        }
        let extent: usize = tree.extent(());
        assert_eq!(extent, 100);
        let mut cursor = tree.cursor::<usize>(());
        let found = cursor.seek(&200usize, Bias::Right);
        assert!(!found);
        assert!(cursor.item().is_none());
        assert_eq!(*cursor.start(), extent);
    }

    #[test]
    fn from_iter_builds_balanced_tree() {
        let tree = SumTree::from_iter(0u8..100, ());
        assert_eq!(tree.items(()), (0u8..100).collect::<Vec<_>>());
        assert!(tree.max_children_count() <= 2 * TREE_BASE);
        assert!(tree.max_items_count() <= 2 * TREE_BASE);
    }

    #[test]
    fn first_and_last() {
        let tree: SumTree<u8> = SumTree::default();
        assert_eq!(tree.first(), None);
        assert_eq!(tree.last(), None);

        let mut tree = SumTree::default();
        tree.extend(vec![10u8, 20, 30], ());
        assert_eq!(tree.first(), Some(&10));
        assert_eq!(tree.last(), Some(&30));
    }

    #[test]
    fn extend_and_items_roundtrip() {
        let mut tree = SumTree::<u8>::default();
        tree.extend(0..20, ());
        let mut tree2 = SumTree::default();
        tree2.extend(50..100, ());
        tree.append(tree2, ());
        assert_eq!(tree.items(()), (0..20).chain(50..100).collect::<Vec<u8>>());
    }

    #[test]
    fn iter_matches_items() {
        let mut tree = SumTree::<u8>::default();
        tree.extend(0..50, ());
        let via_iter: Vec<_> = tree.iter().cloned().collect();
        assert_eq!(via_iter, tree.items(()));
    }

    #[test]
    fn prev_and_next_interleaved() {
        let mut tree = SumTree::default();
        tree.extend(vec![1u8, 2, 3, 4, 5, 6], ());
        let mut cursor = tree.cursor::<IntegersSummary>(());

        cursor.next();
        assert_eq!(cursor.item(), Some(&1));
        assert_eq!(cursor.start().sum, 0);

        cursor.next();
        assert_eq!(cursor.item(), Some(&2));
        assert_eq!(cursor.start().sum, 1);

        cursor.next();
        assert_eq!(cursor.item(), Some(&3));
        assert_eq!(cursor.start().sum, 3);

        cursor.prev();
        assert_eq!(cursor.item(), Some(&2));
        assert_eq!(cursor.start().sum, 1);

        cursor.prev();
        assert_eq!(cursor.item(), Some(&1));
        assert_eq!(cursor.start().sum, 0);

        cursor.prev();
        assert_eq!(cursor.item(), None);
        assert_eq!(cursor.start().sum, 0);

        cursor.next();
        assert_eq!(cursor.item(), Some(&1));
        assert_eq!(cursor.start().sum, 0);
    }

    #[test]
    fn prev_from_end() {
        let mut tree = SumTree::default();
        tree.extend(vec![1u8, 2, 3], ());
        let mut cursor = tree.cursor::<IntegersSummary>(());

        cursor.prev();
        assert_eq!(cursor.item(), Some(&3));
        cursor.prev();
        assert_eq!(cursor.item(), Some(&2));
        cursor.prev();
        assert_eq!(cursor.item(), Some(&1));
        cursor.prev();
        assert_eq!(cursor.item(), None);
    }

    #[test]
    fn next_item_and_prev_item() {
        let mut tree = SumTree::default();
        tree.extend(vec![1u8, 2, 3, 4, 5, 6], ());
        let mut cursor = tree.cursor::<IntegersSummary>(());

        cursor.seek(&Count(2), Bias::Right);
        assert_eq!(cursor.item(), Some(&3));
        assert_eq!(cursor.prev_item(), Some(&2));
        assert_eq!(cursor.next_item(), Some(&4));

        cursor.seek(&Count(0), Bias::Right);
        assert_eq!(cursor.item(), Some(&1));
        assert_eq!(cursor.prev_item(), None);
        assert_eq!(cursor.next_item(), Some(&2));

        cursor.seek(&Count(5), Bias::Right);
        assert_eq!(cursor.item(), Some(&6));
        assert_eq!(cursor.prev_item(), Some(&5));
        assert_eq!(cursor.next_item(), None);
    }

    #[test]
    fn filter_cursor() {
        let mut tree = SumTree::<u8>::default();
        tree.extend(1..=10, ());
        let evens: Vec<_> = tree
            .filter::<_, Count>((), |summary| summary.contains_even)
            .copied()
            .collect();
        assert_eq!(evens, vec![2, 4, 6, 8, 10]);
    }

    #[test]
    fn filter_cursor_prev() {
        let mut tree = SumTree::<u8>::default();
        tree.extend(1..=10, ());
        let mut fc = tree.filter::<_, Count>((), |s| s.contains_even);
        fc.next();
        assert_eq!(fc.item(), Some(&2));
        fc.next();
        assert_eq!(fc.item(), Some(&4));
        fc.prev();
        assert_eq!(fc.item(), Some(&2));
    }

    #[test]
    fn cursor_summary_computation() {
        let mut tree = SumTree::<u8>::default();
        tree.extend(vec![1, 2, 3, 4, 5], ());
        let mut cursor = tree.cursor::<Count>(());
        cursor.seek(&Count(1), Bias::Right);
        let s: Sum = cursor.summary(&Count(4), Bias::Right);
        assert_eq!(s.0, 2 + 3 + 4);
    }

    #[test]
    fn keyed_item_insert_remove() {
        let mut tree = SumTree::<u8>::default();
        tree.edit(vec![Edit::Insert(1), Edit::Insert(2), Edit::Insert(0)], ());
        assert_eq!(tree.items(()), vec![0, 1, 2]);
        assert_eq!(tree.get(&0, ()), Some(&0));
        assert_eq!(tree.get(&1, ()), Some(&1));
        assert_eq!(tree.get(&2, ()), Some(&2));
        assert_eq!(tree.get(&4, ()), None);

        let removed = tree.edit(vec![Edit::Insert(2), Edit::Insert(4), Edit::Remove(0)], ());
        assert_eq!(tree.items(()), vec![1, 2, 4]);
        assert_eq!(removed, vec![0, 2]);
    }

    #[test]
    fn insert_or_replace() {
        let mut tree = SumTree::<u8>::default();
        assert_eq!(tree.insert_or_replace(3, ()), None);
        assert_eq!(tree.insert_or_replace(1, ()), None);
        assert_eq!(tree.insert_or_replace(3, ()), Some(3));
        assert_eq!(tree.items(()), vec![1, 3]);
    }

    #[test]
    fn cursor_iterator() {
        let mut tree = SumTree::<u8>::default();
        tree.extend(1..=5, ());
        let items: Vec<_> = tree.cursor::<()>(()).collect();
        assert_eq!(items, vec![&1, &2, &3, &4, &5]);
    }

    #[test]
    fn empty_tree_operations() {
        let tree = SumTree::<u8>::default();
        assert!(tree.is_empty());
        assert_eq!(tree.first(), None);
        assert_eq!(tree.last(), None);
        assert_eq!(tree.items(()), Vec::<u8>::new());
        let mut cursor = tree.cursor::<IntegersSummary>(());
        cursor.next();
        assert_eq!(cursor.item(), None);
        cursor.prev();
        assert_eq!(cursor.item(), None);
    }

    #[test]
    fn single_element_cursor() {
        let mut tree = SumTree::<u8>::default();
        tree.extend(vec![1], ());
        let mut cursor = tree.cursor::<IntegersSummary>(());

        cursor.next();
        assert_eq!(cursor.item(), Some(&1));
        assert_eq!(cursor.prev_item(), None);
        assert_eq!(cursor.next_item(), None);
        assert_eq!(cursor.start().sum, 0);

        cursor.next();
        assert_eq!(cursor.item(), None);
        assert_eq!(cursor.prev_item(), Some(&1));
        assert_eq!(cursor.start().sum, 1);

        cursor.prev();
        assert_eq!(cursor.item(), Some(&1));
        assert_eq!(cursor.prev_item(), None);
        assert_eq!(cursor.start().sum, 0);
    }

    #[test]
    fn slice_and_suffix() {
        let mut tree = SumTree::default();
        tree.extend(vec![1u8, 2, 3, 4, 5, 6], ());
        let mut cursor = tree.cursor::<Count>(());
        let slice = cursor.slice(&Count(2), Bias::Right);
        assert_eq!(slice.items(()), vec![1, 2]);
        assert_eq!(cursor.item(), Some(&3));
        let suffix = cursor.suffix();
        assert_eq!(suffix.items(()), vec![3, 4, 5, 6]);
    }

    #[test]
    fn update_first() {
        let mut tree = SumTree::<u8>::default();
        tree.extend(vec![1, 2, 3], ());
        tree.update_first(|item| *item = 10, ());
        assert_eq!(tree.items(()), vec![10, 2, 3]);
        assert_eq!(tree.summary().sum, 15);
    }

    #[test]
    fn default_impl() {
        let tree = SumTree::<u8>::default();
        assert!(tree.is_empty());
    }
}
