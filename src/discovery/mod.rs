#![allow(clippy::too_many_arguments)]

pub mod gateway;
pub mod messages;
pub mod messenger;
pub mod peers;

use std::net::{Ipv4Addr, SocketAddrV4};

use bincode::config::{BigEndian, Configuration, Fixint};

pub const ENCODING_CONFIG: Configuration<BigEndian, Fixint> = bincode::config::standard()
    .with_big_endian()
    .with_fixed_int_encoding();

pub const LINK_PORT: u16 = 20808;
pub const MULTICAST_IP_ANY: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), LINK_PORT);
pub const MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(224, 76, 78, 75);
