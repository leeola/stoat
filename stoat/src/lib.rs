pub mod buffer {
    pub struct Buffer;
}
pub mod session {}

#[derive(Default)]
pub struct Stoat;
impl Stoat {
    pub fn new() -> Self {
        Self
    }
}
