#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiffStatus {
    #[default]
    Unchanged,
    Added,
    Modified,
}
