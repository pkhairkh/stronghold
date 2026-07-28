# PQC WASM Bundle

This directory contains the WebAssembly bundle for post-quantum cryptography
used in the phone enrollment page.

## What's here

The phone enrollment page (`../enroll.html`) needs to perform ML-KEM-768
key generation and decapsulation in the browser. This is done via the
[`@noble/post-quantum`](https://github.com/paulmillr/noble-post-quantum)
library, compiled to WASM.

## Building

```bash
npm install @noble/post-quantum
npm run build  # bundles to pq-wasm.js (~12KB gzipped)
```

## Usage

The enrollment page loads `pq-wasm.js` and uses it to:

1. Generate X25519 + ML-KEM-768 keypairs at enrollment time
2. Upload the public halves to the gateway
3. Store the private halves in the browser's IndexedDB (non-extractable)

When a push notification arrives, the phone uses the private halves to
decapsulate the shared secret and decrypt the payload.
