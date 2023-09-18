

// use stun_codec::define_attribute_enums;
// use stun_codec::rfc5389::attributes::*;
// use stun_codec::rfc5766::attributes::*;

// define_attribute_enums!(
//     Attribute, AttributeDecoder, AttributeEncoder,
//     [
//         // RFC 5389
//         MappedAddress, Username, MessageIntegrity, ErrorCode,
//         UnknownAttributes, Realm, Nonce, XorMappedAddress,
//         Software, AlternateServer, Fingerprint,

//         // RFC 5766
//         ChannelNumber, Lifetime, XorPeerAddress, Data,
//         XorRelayAddress, EvenPort, RequestedTransport,
//         DontFragment, ReservationToken
//     ]
// );

// #[macro_use] extern crate trackable;

use stun_codec::define_attribute_enums;
use stun_codec::rfc5389::attributes::*;
use stun_codec::rfc5245::attributes::*;
// use stun_codec::rfc5766::attributes::*;


define_attribute_enums!(
    Attribute, AttributeDecoder, AttributeEncoder,
    [
        // RFC 5389
        MappedAddress, Username, MessageIntegrity, ErrorCode,
        UnknownAttributes, Realm, Nonce, XorMappedAddress,
        Software, AlternateServer, Fingerprint,

        // RFC rfc5245
        Priority, UseCandidate, IceControlled, IceControlling
        
        // // RFC rfc5766
        // ChannelNumber, Lifetime, XorPeerAddress, Data,
        // XorRelayAddress, EvenPort, RequestedTransport,
        // DontFragment, ReservationToken
    ]
);
