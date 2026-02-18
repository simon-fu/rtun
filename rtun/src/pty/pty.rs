use bytes::Bytes;

#[derive(Debug, Clone)]
pub enum PtyEvent {
    StdinData(Bytes),
    Resize(PtySize),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}
