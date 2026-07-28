# Stronghold Phone Enrollment PWA

This directory contains the phone-side Progressive Web App (PWA) for
Stronghold. It is a single, dependency-free HTML file (`enroll.html`) plus
a small set of static assets that turn it into an installable, offline-capable
PWA.

The page is served by the gateway at `GET /setup` via
`include_str!("../../../phone/enroll.html")` in
`gateway/src/routes/phone.rs`. The other files in this directory are served
by the gateway's static file router at `GET /static/*` (see
`gateway/src/routes/mod.rs`).

## Files

| File                | Served at              | Purpose                                              |
|---------------------|------------------------|------------------------------------------------------|
| `enroll.html`       | `/setup`               | Single-file PWA: WebAuthn enrollment, sessions dashboard, pending approvals, REVOKE button, SSE client |
| `manifest.json`     | `/static/manifest.json`| PWA manifest (name, icons, `display: standalone`)    |
| `sw.js`             | `/static/sw.js`        | Service worker (app-shell cache, offline fallback)   |
| `icon.svg`          | `/static/icon.svg`     | App icon (any-purpose, scales to any size)           |
| `icon-maskable.svg` | `/static/icon-maskable.svg` | Maskable icon with safe-zone padding             |
| `pq-wasm/`          | (future)               | Post-quantum WASM bundle for push decryption (W8-T3) |

No build step is required. All JS is inline in `enroll.html`; no bundler,
no framework, no transpiler.

## Feature checklist (Wave 8 DoD)

- **W8-T1 — WebAuthn enrollment** — `navigator.credentials.create()` with
  `authenticatorSelection.authenticatorAttachment = 'platform'` and
  `userVerification = 'required'`. Supports ES256 (-7) and RS256 (-257).
  Attestation set to `'none'` for privacy (we verify UV, not attestation).
- **W8-T2 — WebAuthn approval** — `navigator.credentials.get()` with
  `userVerification: 'required'`. Assertion payload
  (`credential_id`, `authenticator_data`, `client_data_json`, `signature`)
  POSTed to `/phone/decide`.
- **W8-T4 — Active sessions dashboard** — `#sessions-list` populated by SSE
  `session_started` / `session_updated` events. Each card has a REVOKE
  button that POSTs to `/phone/revoke`.
- **W8-T5 — Pending approvals list** — `#pending-list` populated by SSE
  `approval_request` events. Approve / Deny buttons per request.
- **W8-T6 — PWA manifest + service worker** — `manifest.json` with
  `display: standalone`, name, icons (any + maskable). `sw.js` pre-caches
  the app shell, network-first for navigations, cache-first for static
  assets, and passes through SSE / WebSocket / non-GET requests.
- **W8-T8 — Mobile UX polish** — All tap targets are ≥44pt (48pt on coarse
  pointers), `prefers-color-scheme: dark/light` auto-theming,
  `navigator.vibrate()` haptic feedback on Approve/Deny/Revoke/Anomaly,
  ARIA labels + live regions throughout, `prefers-reduced-motion` honored.
- **W8-T9 — Anomaly alert UI** — SSE `anomaly` events render as a
  session card with anomaly styling and a Revoke button.

## SSE design notes

`EventSource` cannot send custom `Authorization` headers (it only supports
cookies via `withCredentials`). The gateway's `/phone/pending` endpoint
requires a `Bearer <phone_token>` header, so we use a `fetch()`-based
streaming reader that parses SSE frames manually. This gives us:

1. **Authorization header** — token sent on every reconnect.
2. **Heartbeat watchdog** — if no bytes arrive within 45 s (the gateway
   sends a `data: heartbeat` every 30 s), the reader is cancelled and
   reconnect fires.
3. **Exponential backoff** — 1 s → 2 s → 4 s → … → 30 s cap.
4. **Comment-line handling** — SSE `:` comments are treated as keepalive.

## Local development

The page is served from the gateway. To run locally against a dev gateway:

```bash
cd /root/stronghold
cargo run --workspace --features no-sev-snp -- gateway
# Then open https://localhost:8443/setup in a mobile browser
# (accept the self-signed cert warning in dev)
```

For mobile testing on a real phone, point your phone at the dev box:

```
https://45.63.97.103:8443/setup
```

WebAuthn requires either `https://` or `http://localhost`. The gateway
serves TLS on 8443 by default.

## Browser support matrix

| Browser             | WebAuthn | SSE-over-fetch | PWA install | vibrate |
|---------------------|----------|----------------|-------------|---------|
| Safari iOS 15.4+    | ✅       | ✅             | ✅          | ⚠️ (no API on iOS, silently ignored) |
| Chrome Android      | ✅       | ✅             | ✅          | ✅      |
| Firefox Android     | ✅       | ✅             | ✅          | ✅      |
| Safari macOS 15.4+  | ✅       | ✅             | ✅          | n/a     |
| Chrome macOS        | ✅       | ✅             | ✅          | n/a     |

iOS does not expose `navigator.vibrate`; calls are wrapped in `try/catch`
and silently ignored, so the rest of the UX is unaffected.

## Security notes

- The phone token (`stronghold_phone_token`) is stored in `localStorage`.
  On iOS Safari, this is scoped to the PWA's origin and is not shared
  between the browser tab and the installed PWA. If a user installs the
  PWA and later enrolls a second device, they must re-enroll inside the
  PWA context.
- WebAuthn credentials are stored in the platform authenticator
  (Secure Enclave on iOS, Titan/StrongBox on Android). The private key
  never leaves the device.
- The page sets `attestation: 'none'` for enrollment — we do not collect
  authenticator attestation. User verification (Face ID / Touch ID /
  PIN) is required for both enrollment and approval.
