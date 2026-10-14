pub mod frame;
pub(crate) mod screen;
pub mod session;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub generation: u64,
    pub seq: u64,
    pub cols: u16,
    pub rows: u16,
    pub ansi: Vec<u8>,
}
