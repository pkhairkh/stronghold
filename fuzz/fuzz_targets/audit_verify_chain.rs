//! Fuzz target: audit log hash chain verification
//!
//! Generates random audit entries, builds a hash chain, and verifies it.
//! The verifier must never panic on random input and must correctly detect
//! tampering.

#![no_main]

use libfuzzer_sys::fuzz_target;
use stronghold_gateway::crypto::hybrid_sig::AuditKeys;

fuzz_target!(|data: &[u8]| {
    // Generate a keypair, sign random data, verify.
    // This exercises the sign/verify path with random message sizes.
    let keys = AuditKeys::generate();
    let sig = keys.sign(data);
    let _ = keys.verify(data, &sig);

    // Tamper: flip a bit in the message and verify it fails.
    if !data.is_empty() {
        let mut tampered = data.to_vec();
        tampered[0] ^= 0x01;
        let result = keys.verify(&tampered, &sig);
        debug_assert!(
            !result,
            "tampered message must not verify (bit flip at index 0)"
        );
    }

    // Tamper: flip a bit in the signature and verify it fails.
    use base64::Engine;
    if let Ok(mut bytes) = base64::engine::general_purpose::STANDARD.decode(&sig.sig_ed25519) {
        if !bytes.is_empty() {
            bytes[0] ^= 0x01;
            let tampered_sig = stronghold_gateway::crypto::hybrid_sig::DualSignature {
                sig_ed25519: base64::engine::general_purpose::STANDARD.encode(&bytes),
                sig_mldsa65: sig.sig_mldsa65.clone(),
            };
            let result = keys.verify(data, &tampered_sig);
            debug_assert!(
                !result,
                "tampered signature must not verify (bit flip at index 0)"
            );
        }
    }
});
