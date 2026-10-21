//! The shared harness for thoradm's tests
//!
//! Each test binary includes this module and uses the parts it needs, so items one binary
//! doesn't use are expected.
#![allow(dead_code)]

pub mod archives;
pub mod classes;
pub mod cluster;
pub mod columns;
pub mod corrupt;
pub mod cycle;
pub mod enums;
pub mod faulty_sink;
pub mod fixture;
pub mod flush_guard;
pub mod harness;
pub mod kill;
pub mod kinds;
pub mod legacy;
pub mod mock_s3;
pub mod names;
pub mod objects;
pub mod oracle;
pub mod proxy;
pub mod records;
pub mod schema;
pub mod seed;
pub mod shapes;
pub mod state;
