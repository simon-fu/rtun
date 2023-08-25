use crate::channel::ChId;


#[derive(Debug, Clone)]
pub struct NextChId {
    next_id: ChId,
}

impl Default for NextChId {
    fn default() -> Self {
        Self { next_id: ChId(0) }
    }
}

impl NextChId {
    pub fn next_ch_id(&mut self) -> ChId {
        self.next_id.0 += 1;
        self.next_id
    }
}
