# Project Notes

## Request Diagnostics

The Rust request logger builds bounded structural diagnostics from the final serialized Kiro request in `src/request_log/request.rs`. Request-body capture remains opt-in. Tool diagnostics count calls/results, correlate their IDs with short SHA-256 fingerprints, and report unmatched, duplicate, and out-of-order entries without persisting raw tool IDs or arguments. The admin UI renders the summary and bounded pair details from `admin-ui/src/components/request-logs.tsx`.

## Verification

- Rust tests: `cargo test`
- Admin UI: `cd admin-ui && pnpm build`
