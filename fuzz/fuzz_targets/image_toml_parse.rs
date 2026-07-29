//! Fuzz target: image.toml parser
//!
//! Feeds random bytes into `stronghold_gateway::images::dsl::parse()`.
//! The parser must never panic — it should return an `Err` for invalid input.

#![no_main]

use libfuzzer_sys::fuzz_target;
use stronghold_gateway::images::dsl;

fuzz_target!(|data: &[u8]| {
    // Try to parse the fuzz input as a TOML string.
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = dsl::parse(s);
    }
});
