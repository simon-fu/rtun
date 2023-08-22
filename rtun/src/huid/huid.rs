
use std::{convert::TryFrom, sync::{Arc, atomic::AtomicU64}, time::{SystemTime, UNIX_EPOCH}};

use anyhow::bail;

lazy_static::lazy_static!(
    pub static ref CHARS: [char; 62] = [
        'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 
        'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 
        'U', 'V', 'W', 'X', 'Y', 'Z',
        'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 
        'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's', 't', 
        'u', 'v', 'w', 'x', 'y', 'z',
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9',
    ];

    pub static ref REV_U8: [u8; 256] = {
        let mut data = [255_u8; 256];
        for n in 0..CHARS.len() {
            let c = CHARS[n];
            let index = (c as u8) as usize;
            data[index] = n as u8;
        }
        data
    };
    
    static ref MAX: u64 = 62*62*62*62*62*62*62*62;
);

#[derive(Default, PartialEq, Eq, PartialOrd, Ord, Clone, Hash, Copy)]
pub struct HUId {
    id: u64
}

impl HUId {

    pub fn random() -> Self {
        Self::from(random_id())
    }

    #[inline]
    pub fn to_u64(&self) -> u64 {
        let mut n = 0;
        let mut multi = 1_u64;
        let mut id = self.id;
        for _ in 0..8 {
            let index = (id & 0xFF) as usize;
            let v = REV_U8[index] as u64;
            n += multi * v;
            id >>= 8;
            multi *= CHARS.len() as u64;
        }
        n
    }

    #[inline]
    pub fn chars(&self) -> [char; 8] {
        [
            (self.id >> 56 & 0xFF) as u8 as char,
            (self.id >> 48 & 0xFF) as u8 as char,
            (self.id >> 40 & 0xFF) as u8 as char,
            (self.id >> 32 & 0xFF) as u8 as char,
            (self.id >> 24 & 0xFF) as u8 as char,
            (self.id >> 16 & 0xFF) as u8 as char,
            (self.id >>  8 & 0xFF) as u8 as char,
            (self.id       & 0xFF) as u8 as char,
        ]
    }

    #[inline]
    pub fn to_bytes(&self) -> [u8; 8] {
        [
            (self.id >> 56 & 0xFF) as u8 ,
            (self.id >> 48 & 0xFF) as u8 ,
            (self.id >> 40 & 0xFF) as u8 ,
            (self.id >> 32 & 0xFF) as u8 ,
            (self.id >> 24 & 0xFF) as u8 ,
            (self.id >> 16 & 0xFF) as u8 ,
            (self.id >>  8 & 0xFF) as u8 ,
            (self.id       & 0xFF) as u8 ,
        ]
    }

    #[inline]
    pub fn to_string(&self) -> String {
        self.chars().iter().collect()
    }

    fn fmt_me(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let buf = self.chars();
        for c in buf.iter() {
            write!(f, "{}", c)?;
        }
        Ok(())
    }

}

impl AsRef<HUId> for HUId {
    fn as_ref(&self) -> &HUId {
        self
    }
}

impl From<u64> for HUId {
    fn from(mut n: u64) -> Self {
        let mut id = 0;
        for _ in 0..8 {
            id >>= 8;
            let index = (n % CHARS.len() as u64) as usize;
            let c = CHARS[index] as u64;
            id = id | (c << 56);
            n = n / (CHARS.len() as u64);
        }
        Self {id}
    }
}

impl TryFrom<&String> for HUId {
    type Error = anyhow::Error;

    #[inline]
    fn try_from(value: &String) -> Result<Self, Self::Error> {
        Self::try_from(&value[..])
    }
}

impl TryFrom<&str> for HUId {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        if value.len() != 8 {
            bail!("expect 8 chars but {}", value.len());
        }

        let mut n = 0;
        let mut multi = 1_u64;
        for c in value.chars().rev() {
            let index = (c as u8) as usize;
            let v = REV_U8[index] as u64;
            if (v as usize) >= CHARS.len() {
                bail!("invalid chars {}", c);
            }
            n += multi * v;
            multi *= CHARS.len() as u64;
        }

        Ok(Self::from(n))
    }
}

impl Into<u64> for HUId {
    #[inline]
    fn into(self) -> u64 {
        self.to_u64()
    }
}

impl Into<u64> for &HUId {
    #[inline]
    fn into(self) -> u64 {
        self.to_u64()
    }
}

impl Into<String> for HUId {
    #[inline]
    fn into(self) -> String {
        self.to_string()
    }
}

impl Into<String> for &HUId {
    #[inline]
    fn into(self) -> String {
        self.to_string()
    }
}

impl std::fmt::Display for HUId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.fmt_me(f)
    }
}

impl std::fmt::Debug for HUId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "\"")?;
        self.fmt_me(f)?;
        write!(f, "\"")
    }
}

fn random_id() -> u64 {
    SystemTime::now()
    .duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64
}

pub trait NextId {
    fn next_id(&self) -> HUId;
}

pub type ArcNextId = Arc<dyn NextId + Send + Sync>;

#[derive(Debug, Default)]
pub struct OrderBuild {
    id: AtomicU64,
}

impl OrderBuild {
    pub fn start_from_random() -> Self {
        Self{
            id: AtomicU64::new(random_id()) 
        }
    }    
}

impl NextId for OrderBuild {
    fn next_id(&self) -> HUId {
        let n = self.id.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % (*MAX);
        HUId::from(n)
    }
}

impl Iterator for OrderBuild {
    type Item = HUId;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        Some(self.next_id())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_print_default() {
        let builder = OrderBuild::default();
        for _ in 0 .. 200 {
            let uid = builder.next_id();
            println!("{}, {:#X}, {}, {:?}", uid, uid.id, uid.to_string(), builder);
            assert!(uid.to_u64() == HUId::from(uid.to_u64()).to_u64());
            assert!(uid.to_string() == HUId::try_from(&uid.to_string()).unwrap().to_string());
        }
    }

    #[test]
    fn test_print_random() {
        let builder = OrderBuild::start_from_random();
        for _ in 0 .. 200 {
            let uid = builder.next_id();
            println!("{:#X}, {}, {:?}", uid.id, uid.to_string(), builder);
            assert!(uid.to_u64() == HUId::from(uid.to_u64()).to_u64());
        }
    }

    #[test]
    fn test_basic() {
        let builder = OrderBuild::default();
        
        {
            let mut i = 0_usize;
            while i < 26 {
                assert!(CHARS[i] == ('A' as u8 + (i-0) as u8) as char);
                i += 1;
            }

            while i < 52 {
                assert!(CHARS[i] == ('a' as u8 + (i-26) as u8) as char);
                i += 1;
            }

            while i < 62 {
                assert!(CHARS[i] == ('0' as u8 + (i-52) as u8) as char);
                i += 1;
            }
        }

        assert!(builder.next_id().to_string() == "AAAAAAAA");
        assert!(builder.next_id().to_string() == "AAAAAAAB");
        assert!(builder.next_id().to_string() == "AAAAAAAC");
        assert!(builder.next_id().to_string() == "AAAAAAAD");
        assert!(builder.next_id().to_string() == "AAAAAAAE");
        assert!(builder.next_id().to_string() == "AAAAAAAF");
        assert!(builder.next_id().to_string() == "AAAAAAAG");
        assert!(builder.next_id().to_string() == "AAAAAAAH");
        assert!(builder.next_id().to_string() == "AAAAAAAI");
        assert!(builder.next_id().to_string() == "AAAAAAAJ");
        assert!(builder.next_id().to_string() == "AAAAAAAK");
    }
}
