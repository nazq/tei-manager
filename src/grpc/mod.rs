//! gRPC multiplexer for routing requests to multiple TEI instances
//!
//! This module provides a high-performance gRPC proxy that routes requests to backend TEI instances
//! based on instance name, model ID, or index. Designed for zero-copy forwarding and lock-free connection pooling.

pub mod multiplexer;
pub mod pool;
pub mod server;

// Include generated proto code
pub mod proto {
    pub mod tei {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/tei.v1.rs"));
        }
    }

    pub mod multiplexer {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/tei_multiplexer.v1.rs"));
        }
    }
}
