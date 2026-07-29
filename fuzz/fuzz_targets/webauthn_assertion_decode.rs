//! Fuzz target: WebAuthn assertion decoding
//!
//! Feeds random bytes into the WebAuthn client_data_json and
//! authenticator_data parsers. The parsers must never panic — they
//! should return an `Err` for malformed input.

#![no_main]

use libfuzzer_sys::fuzz_target;
use stronghold_gateway::crypto::webauthn;
use stronghold_gateway::routes::phone::WebAuthnAssertion;

fuzz_target!(|data: &[u8]| {
    // Construct an assertion with random base64-encoded fields.
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data);

    let assertion = WebAuthnAssertion {
        credential_id: b64.clone(),
        authenticator_data: b64.clone(),
        client_data_json: b64,
        signature: String::new(),
    };

    // parse_authenticator_data must never panic.
    let _ = webauthn::parse_authenticator_data(&assertion);

    // parse_and_validate_client_data must never panic.
    let _ = webauthn::parse_and_validate_client_data(
        &assertion,
        &[0u8; 32],
        "https://localhost:8443",
    );
});
