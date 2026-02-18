//
// 2179561608 1 udp 2130706431 192.168.1.17 58312 typ host

use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, SocketAddr};

// use super::IceError;
use anyhow::{bail, Result};

/// ICE candidates are network addresses used to connect to a peer.
///
/// There are different kinds of ICE candidates. The simplest kind is a
/// host candidate which is a socket address on a local (host) network interface.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Candidate {
    /// An arbitrary string used in the freezing algorithm to
    /// group similar candidates.
    ///
    /// It is the same for two candidates that
    /// have the same type, base IP address, protocol (UDP, TCP, etc.),
    /// and STUN or TURN server.  If any of these are different, then the
    /// foundation will be different.
    ///
    /// For remote, this is communicated,  and locally it's calculated.
    foundation: Option<String>, // 1-32 "ice chars", ALPHA / DIGIT / "+" / "/"

    /// A component is a piece of a data stream.
    ///
    /// A data stream may require multiple components, each of which has to
    /// work in order for the data stream as a whole to work.  For RTP/RTCP
    /// data streams, unless RTP and RTCP are multiplexed in the same port,
    /// there are two components per data stream -- one for RTP, and one
    /// for RTCP.
    component_id: u16, // 1 for RTP, 2 for RTCP

    /// Protocol for the candidate.
    proto: String, // "udp" or "tcp"

    /// Priority.
    ///
    /// For remote, this is communicated, and locally it's (mostly) calculated.
    /// For local peer reflexive it is set.
    prio: Option<u32>, // 1-10 digits

    /// The actual address to use. This might be a host address, server reflex, relay etc.
    addr: SocketAddr, // ip/port

    /// The base on the local host.
    ///
    /// "Base" refers to the address an agent sends from for a
    /// particular candidate.  Thus, as a degenerate case, host candidates
    /// also have a base, but it's the same as the host candidate.
    base: Option<SocketAddr>, // the "base" used for local candidates.

    /// Type of candidate.
    kind: CandidateKind, // host/srflx/prflx/relay

    /// Relay address.
    ///
    /// For server reflexive candidates, this is the address/port of the server.
    raddr: Option<SocketAddr>, // ip/port

    /// Ufrag.
    ///
    /// This is used to tie an ice candidate to a specific ICE session. It's important
    /// when trickle ICE is used in conjunction with ice restart, since it must be
    /// possible the ice agent to know whether a candidate appearing belongs to
    /// the current or previous session.
    ///
    /// This value is only set for incoming candidates. Once we use the candidate inside
    /// pairs, the field is blanked to not be confusing during ice-restarts.
    ufrag: Option<String>,

    /// The ice agent might assign a local preference if we have multiple candidates
    /// that are the same type.
    local_preference: Option<u32>,

    /// If we discarded this candidate (for example due to being redundant
    /// against another candidate).
    discarded: bool,
}

impl fmt::Debug for Candidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Candidate({}={}", self.kind, self.addr)?;
        if let Some(base) = self.base {
            if base != self.addr {
                write!(f, " base={base}")?;
            }
        }
        if let Some(raddr) = self.raddr {
            write!(f, " raddr={raddr}")?;
        }
        write!(f, " prio={}", self.prio())?;
        if self.discarded {
            write!(f, " discarded")?;
        }
        write!(f, ")")
    }
}

impl Candidate {
    #[allow(clippy::too_many_arguments)]
    fn new(
        foundation: Option<String>,
        component_id: u16,
        proto: String,
        prio: Option<u32>,
        addr: SocketAddr,
        base: Option<SocketAddr>,
        kind: CandidateKind,
        raddr: Option<SocketAddr>,
        ufrag: Option<String>,
    ) -> Self {
        Candidate {
            foundation,
            component_id,
            proto,
            prio,
            addr,
            base,
            kind,
            raddr,
            ufrag,
            local_preference: None,
            discarded: false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[doc(hidden)]
    pub fn parsed(
        foundation: String,
        component_id: u16,
        proto: String,
        prio: u32,
        addr: SocketAddr,
        kind: CandidateKind,
        raddr: Option<SocketAddr>,
        ufrag: Option<String>,
    ) -> Self {
        Candidate::new(
            Some(foundation),
            component_id,
            proto,
            Some(prio),
            addr,
            None,
            kind,
            raddr,
            ufrag,
        )
    }

    /// Creates a host ICE candidate.
    ///
    /// Host candidates are local sockets directly on the host.
    pub fn host(addr: SocketAddr) -> Result<Self> {
        if !is_valid_ip(addr.ip()) {
            // return Err(IceError::BadCandidate(format!("invalid ip {}", addr.ip())));
            bail!("invalid ip {}", addr.ip())
        }

        Ok(Candidate::new(
            None,
            1, // only RTP
            "udp".into(),
            None,
            addr,
            Some(addr),
            CandidateKind::Host,
            None,
            None,
        ))
    }

    /// Creates a peer reflexive ICE candidate.
    ///
    /// Peer reflexive candidates are NAT:ed addresses discovered via STUN
    /// binding responses. `addr` is the discovered address. `base` is the local
    /// (host) address inside the NAT we used to get this response.
    pub fn peer_reflexive(
        addr: SocketAddr,
        base: SocketAddr,
        prio: u32,
        found: Option<String>,
        ufrag: Option<String>,
    ) -> Self {
        Candidate::new(
            found,
            1, // only RTP
            "udp".into(),
            Some(prio),
            addr,
            Some(base),
            CandidateKind::PeerReflexive,
            None,
            ufrag,
        )
    }

    // #[cfg(test)]
    // pub(crate) fn test_peer_rflx(addr: SocketAddr, base: SocketAddr) -> Self {
    //     Candidate::new(
    //         None,
    //         1, // only RTP
    //         "udp".into(),
    //         None,
    //         addr,
    //         Some(base),
    //         CandidateKind::PeerReflexive,
    //         None,
    //         None,
    //     )
    // }

    /// Candidate foundation.
    ///
    /// For local candidates this is calculated.
    pub(crate) fn foundation(&self) -> String {
        if let Some(v) = &self.foundation {
            return v.clone();
        }

        // Two candidates have the same foundation when all of the
        // following are true:
        let mut hasher = DefaultHasher::new();

        //  o  They have the same type (host, relayed, server reflexive, or peer
        //     reflexive).
        self.kind.hash(&mut hasher);

        //  o  Their bases have the same IP address (the ports can be different).
        self.base().ip().hash(&mut hasher);

        //  o  For reflexive and relayed candidates, the STUN or TURN servers
        //     used to obtain them have the same IP address (the IP address used
        //     by the agent to contact the STUN or TURN server).
        if let Some(raddr) = self.raddr {
            raddr.ip().hash(&mut hasher);
        }

        //  o  They were obtained using the same transport protocol (TCP, UDP).
        self.proto.hash(&mut hasher);

        let hash = hasher.finish();

        hash.to_string()
    }

    /// Returns the priority value for the specified ICE candidate.
    ///
    /// The priority is a positive integer between 1 and 2^31 - 1 (inclusive), calculated
    /// according to the ICE specification defined in RFC 8445, Section 5.1.2.
    pub fn prio(&self) -> u32 {
        self.do_prio(false)
    }

    // pub(crate) fn prio_prflx(&self) -> u32 {
    //     self.do_prio(true)
    // }

    fn do_prio(&self, as_prflx: bool) -> u32 {
        // Remote candidates have their prio calculated on their side.
        if let Some(prio) = &self.prio {
            return *prio;
        }

        // The RECOMMENDED values for type preferences are 126 for host
        // candidates, 110 for peer-reflexive candidates, 100 for server-
        // reflexive candidates, and 0 for relayed candidates.
        let type_preference = if as_prflx {
            110
        } else {
            match self.kind {
                CandidateKind::Host => 126,
                CandidateKind::PeerReflexive => 110,
                CandidateKind::ServerReflexive => 100,
                CandidateKind::Relayed => 0,
            }
        };

        // The recommended formula combines a preference for the candidate type
        // (server reflexive, peer reflexive, relayed, and host), a preference
        // for the IP address for which the candidate was obtained, and a
        // component ID using the following formula:
        //
        // priority = (2^24)*(type preference) +
        //     (2^8)*(local preference) +
        //     (2^0)*(256 - component ID)
        let prio =
            type_preference << 24 | self.local_preference() << 8 | (256 - self.component_id as u32);

        // https://datatracker.ietf.org/doc/html/rfc8445#section-5.1.2
        // MUST be a positive integer between 1 and (2**31 - 1)
        assert!(prio >= 1 && prio < 2_u32.pow(31));

        prio
    }

    pub(crate) fn local_preference(&self) -> u32 {
        self.local_preference
            .unwrap_or_else(|| if self.addr.is_ipv6() { 65_535 } else { 65_534 })
    }

    pub fn component_id(&self) -> u16 {
        self.component_id
    }

    /// Returns the address for the specified ICE candidate.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Returns a reference to the String containing the transport protocol of
    /// the ICE candidate. For example tcp/udp/..
    pub fn proto(&self) -> &String {
        &self.proto
    }

    pub fn base(&self) -> SocketAddr {
        self.base.unwrap_or(self.addr)
    }

    pub fn raddr(&self) -> Option<SocketAddr> {
        self.raddr
    }

    pub fn kind(&self) -> CandidateKind {
        self.kind
    }

    pub fn to_cstr(&self) -> String {
        CandidateString(self).to_string()
    }

    // pub(crate) fn set_local_preference(&mut self, v: u32) {
    //     self.local_preference = Some(v);
    // }

    // pub(crate) fn set_discarded(&mut self) {
    //     self.discarded = true;
    // }

    // pub(crate) fn discarded(&self) -> bool {
    //     self.discarded
    // }

    // pub(crate) fn set_ufrag(&mut self, ufrag: &str) {
    //     self.ufrag = Some(ufrag.into());
    // }

    // #[doc(hidden)]
    // pub fn ufrag(&self) -> Option<&str> {
    //     self.ufrag.as_deref()
    // }

    // pub(crate) fn clear_ufrag(&mut self) {
    //     self.ufrag = None;
    // }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CandidateKind {
    Host,
    PeerReflexive,
    ServerReflexive,
    Relayed,
}

impl fmt::Display for CandidateKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let x = match self {
            CandidateKind::Host => "host",
            CandidateKind::PeerReflexive => "prflx",
            CandidateKind::ServerReflexive => "srflx",
            CandidateKind::Relayed => "relay",
        };
        write!(f, "{x}")
    }
}

fn is_valid_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            !v.is_link_local() && !v.is_broadcast() && !v.is_multicast() && !v.is_unspecified()
        }
        IpAddr::V6(v) => !v.is_multicast() && !v.is_unspecified(),
    }
}

// TODO: maybe a bit strange this is used for SDP serializing?
impl fmt::Display for Candidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "a=candidate:{} {} {} {} {} {} typ {}",
            self.foundation(),
            self.component_id,
            self.proto,
            self.prio(),
            self.addr.ip(),
            self.addr.port(),
            self.kind
        )?;
        if let Some((raddr, rport)) = self.raddr.as_ref().map(|r| (r.ip(), r.port())) {
            write!(f, " raddr {raddr} rport {rport}")?;
        }
        if let Some(ufrag) = &self.ufrag {
            write!(f, " ufrag {ufrag}")?;
        }
        write!(f, "\r\n")
    }
}

struct CandidateString<'a>(&'a Candidate);

impl<'a> fmt::Display for CandidateString<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "candidate:{} {} {} {} {} {} typ {}",
            self.0.foundation(),
            self.0.component_id,
            self.0.proto,
            self.0.prio(),
            self.0.addr.ip(),
            self.0.addr.port(),
            self.0.kind
        )?;
        if let Some((raddr, rport)) = self.0.raddr.as_ref().map(|r| (r.ip(), r.port())) {
            write!(f, " raddr {raddr} rport {rport}")?;
        }
        if let Some(ufrag) = &self.0.ufrag {
            write!(f, " ufrag {ufrag}")?;
        }
        Ok(())
    }
}

pub fn server_reflexive(addr: SocketAddr, base: SocketAddr, componet_id: Option<u16>) -> Candidate {
    Candidate::new(
        None,
        componet_id.unwrap_or(1), // default RTP
        "udp".into(),
        None,
        addr,
        Some(base),
        CandidateKind::ServerReflexive,
        Some(base),
        None,
    )
}

use {
    combine::error::*,
    combine::parser::char::*,
    combine::parser::combinator::*,
    combine::stream::StreamErrorFor,
    combine::*,
    combine::{ParseError, Parser, Stream},
};

pub fn parse_candidate(s: &str) -> Result<Candidate> {
    let r = candidate_parser().parse(s)?;
    Ok(r.0)
}

#[doc(hidden)]
fn candidate_parser<Input>() -> impl Parser<Input, Output = Candidate>
where
    Input: Stream<Token = char>,
    Input::Error: ParseError<Input::Token, Input::Range, Input::Position>,
{
    // a=candidate:1 1 udp 2113929471 203.0.113.100 10100 typ host
    // a=candidate:1 2 udp 2113929470 203.0.113.100 10101 typ host
    // a=candidate:1 1 udp 1845494015 198.51.100.100 11100 typ srflx raddr 203.0.113.100 rport 10100
    // a=candidate:1 1 udp 255 192.0.2.100 12100 typ relay raddr 198.51.100.100 rport 11100
    let port = || {
        not_sp::<Input>().and_then(|s| {
            s.parse::<u16>()
                .map_err(StreamErrorFor::<Input>::message_format)
        })
    };

    let ip_addr = || {
        not_sp().and_then(|s| {
            s.parse::<IpAddr>()
                .map_err(StreamErrorFor::<Input>::message_format)
        })
    };

    let kind = choice((
        string("host").map(|_| CandidateKind::Host),
        string("prflx").map(|_| CandidateKind::PeerReflexive),
        string("srflx").map(|_| CandidateKind::ServerReflexive),
        string("relay").map(|_| CandidateKind::Relayed),
    ));

    (
        not_sp(),
        token(' '),
        not_sp().and_then(|s| {
            s.parse::<u16>()
                .map_err(StreamErrorFor::<Input>::message_format)
        }),
        token(' '),
        not_sp(),
        token(' '),
        not_sp().and_then(|s| {
            s.parse::<u32>()
                .map_err(StreamErrorFor::<Input>::message_format)
        }),
        token(' '),
        ip_addr(),
        token(' '),
        port(),
        string(" typ "),
        kind,
        optional((
            attempt(string(" raddr ")),
            ip_addr(),
            string(" rport "),
            port(),
        )),
        optional((attempt(string(" ufrag ")), not_sp())),
    )
        .map(
            |(found, _, comp_id, _, proto, _, prio, _, addr, _, port, _, kind, raddr, ufrag)| {
                Candidate::parsed(
                    found,
                    comp_id,
                    proto,
                    prio, // remote candidates calculate prio on their side
                    SocketAddr::from((addr, port)),
                    kind,
                    raddr.map(|(_, addr, _, port)| SocketAddr::from((addr, port))),
                    ufrag.map(|(_, u)| u),
                )
            },
        )
}

/// Not SP, \r or \n
fn not_sp<Input>() -> impl Parser<Input, Output = String>
where
    Input: Stream<Token = char>,
    Input::Error: ParseError<Input::Token, Input::Range, Input::Position>,
{
    many1(satisfy(|c| c != ' ' && c != '\r' && c != '\n'))
}

#[cfg(test)]
mod test {

    use crate::ice::ice_candidate::parse_candidate;

    use super::server_reflexive;

    #[test]
    fn test_candidate() {
        // from chrome 115: candidate:4290534361 1 udp 1685987071 221.221.148.232 61615 typ srflx raddr 192.168.1.17 rport 61615 generation 0 ufrag si79 network-id
        // from webrtc-ice = "=0.9.1": 3395998451 1 udp 1694498815 221.221.148.232 58195 typ srflx raddr 0.0.0.0 rport 58195

        let base = "192.168.1.17:61615".parse().unwrap();
        let addr = "221.221.148.232:61615".parse().unwrap();
        let src = server_reflexive(addr, base, None);
        let cstr = src.to_cstr();
        assert_eq!(cstr, "candidate:8989210792329146065 1 udp 1694498559 221.221.148.232 61615 typ srflx raddr 192.168.1.17 rport 61615");

        let dst = parse_candidate(&cstr).unwrap();
        assert_eq!(src.addr(), dst.addr());
        assert_eq!(src.prio(), dst.prio());
        assert_eq!(src.raddr, dst.raddr);
    }
}
