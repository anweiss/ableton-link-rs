pub mod gateway;
pub mod messages;
pub mod messenger;
pub mod payload;
pub mod peers;

use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    result,
};

use bincode::config::{BigEndian, Configuration, Fixint};

use crate::link::error::Error;

pub type Result<T> = result::Result<T, Error>;

pub const ENCODING_CONFIG: Configuration<BigEndian, Fixint> = bincode::config::standard()
    .with_big_endian()
    .with_fixed_int_encoding();

pub const IP_ANY: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 20808));
pub const MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(224, 76, 78, 75);
pub const LINK_PORT: u16 = 20808;
