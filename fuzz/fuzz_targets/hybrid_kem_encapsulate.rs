//! Fuzz target: hybrid KEM encapsulate/decapsulate
//!
//! Generates random "public keys" (wrong sizes, wrong bytes) and feeds them
//! to `encapsulate()`. The function must never panic — it should return
//! an `Err` for invalid input sizes.

#![no_main]

use libfuzzer_sys::fuzz_target;
use stronghold_gateway::crypto::hybrid_kem;

fuzz_target!(|data: &[u8]| {
    // Split the fuzz data into two "public keys" of arbitrary sizes.
    // encapsulate() must reject wrong sizes without panicking.
    if data.len() < 2 {
        return;
    }
    let split = (data[0] as usize) % data.len();
    let (x_pub, m_pub) = data[1..].split_at(split.min(data.len() - 1));

    // This must return an Err (wrong size), not panic.
    let _ = hybrid_kem::encapsulate(x_pub, m_pub);
});
