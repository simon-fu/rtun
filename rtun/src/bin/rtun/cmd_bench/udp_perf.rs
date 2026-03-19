use std::fmt::{Display, Formatter};

use clap::ValueEnum;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum UdpPerfMode {
    Forward,
    Reverse,
    Bidir,
}

impl Display for UdpPerfMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forward => write!(f, "forward"),
            Self::Reverse => write!(f, "reverse"),
            Self::Bidir => write!(f, "bidir"),
        }
    }
}
