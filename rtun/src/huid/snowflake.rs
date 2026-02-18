use anyhow::Result;

// snowflake revised edition
// reserved: 1 bit
// time-id: 39bit(origin 41bit), duration years = 2^39/1000/60/60/24/365 ≈ 17
// node-id: 12bit(origin 10bit), max nodes = 2^12 = 4096
// seq-id: 12bit, max qps = 2^12 = 4096/ms = 4096000/sec
#[derive(Debug, Default)]
pub struct SnowflakeId {
    // node_id: u64,
    last_ts: u64,
    last_seq: u64,
}

const SNOWFLAKE_BASE: u64 = 1609430400000; // 2021-01-01 00:00:00
const SNOWFLAKE_MAX_SEQ: u64 = 1 << 12;

impl SnowflakeId {
    // pub fn new(node_id: u64) -> Self {
    //     Self {
    //         node_id: node_id << 12,
    //         last_ts: 0,
    //         last_seq: 0,
    //     }
    // }

    pub fn next(&mut self, node_id: u64) -> Result<u64, u64> {
        let now = std::time::SystemTime::now();
        let since_the_epoch = now
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Time went backwards");

        let ts =
            since_the_epoch.as_secs() * 1000 + (since_the_epoch.subsec_nanos() as u64 / 1_000_000);
        let ts = ts - SNOWFLAKE_BASE;

        if ts <= self.last_ts {
            self.last_seq += 1;
            if self.last_seq >= SNOWFLAKE_MAX_SEQ {
                let mut nanos = since_the_epoch.subsec_nanos() as u64;
                nanos = nanos - 1_000_000 * (nanos / 1_000_000);
                nanos = 1_000_000 - nanos;
                nanos = nanos + (self.last_ts - ts) * 1_000_000;
                return Err(nanos);
            }
        } else {
            self.last_ts = ts;
            self.last_seq = 0;
        }

        let mid = (self.last_ts << 24) | node_id | self.last_seq;
        Ok(mid)
    }

    pub fn next_or_wait(&mut self, node_id: u64) -> u64 {
        loop {
            match self.next(node_id) {
                Ok(n) => {
                    return n;
                }
                Err(nanos) => {
                    std::thread::sleep(std::time::Duration::from_nanos(nanos));
                }
            }
        }
    }

    pub fn next_or_borrow(&mut self, node_id: u64) -> u64 {
        let now = std::time::SystemTime::now();
        let since_the_epoch = now
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Time went backwards");
        let ts =
            since_the_epoch.as_secs() * 1000 + (since_the_epoch.subsec_nanos() as u64 / 1_000_000);
        let ts = ts - SNOWFLAKE_BASE;

        if ts <= self.last_ts {
            self.last_seq += 1;
            if self.last_seq >= SNOWFLAKE_MAX_SEQ {
                self.last_ts += 1;
                self.last_seq = 0;
            }
        } else {
            self.last_ts = ts;
            self.last_seq = 0;
        }

        let mid = (self.last_ts << 24) | node_id | self.last_seq;
        mid
    }
}
