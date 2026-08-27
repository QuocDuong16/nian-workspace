//! Transport selection (spec section 12): stdio by default, streamable HTTP
//! as an explicit opt-in.

pub mod http;
pub mod stdio;
