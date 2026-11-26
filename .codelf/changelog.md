# Changelog

## Request tool pairing diagnostics

- Added ordered `tool_use` / `tool_result` ID correlation to request diagnostics.
- Added pair status counts and bounded per-entry paths with short ID fingerprints; raw IDs and tool inputs remain excluded.
- Detects missing IDs/results, orphan or duplicate results, repeated call IDs, and result order mismatches.
- Added admin UI summary and tests for parallel order, duplicate/orphan results, and split results across consecutive user messages.
