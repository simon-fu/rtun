use parking_lot::Mutex;

use crate::huid::snowflake::SnowflakeId;

use super::HUId;

pub fn gen_huid() -> HUId {
    lazy_static::lazy_static!(
        static ref INSTANCE: Mutex<SnowflakeId> = Default::default();
    );
    let mut data = INSTANCE.lock();
    let n = data.next_or_borrow(get_node_id());
    return HUId::from(n);
}

#[inline]
fn get_node_id() -> u64 {
    0
}
