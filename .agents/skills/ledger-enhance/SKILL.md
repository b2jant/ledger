---
name: ledger-enhance
description: Ground a rough coding request into exact repository files to edit, contracts/callers to reference, and verification tests using the LEDGER MCP server.
---

1. Call `ledger_context(task, mode: "enhance")` first.
2. Summarize the grounded `FILES_TO_EDIT`, `FILES_TO_REFERENCE_AND_RESPECT`, and `VERIFICATION_COMMANDS`.
3. Use `ledger_expand(handle, items)` to retrieve any omitted function bodies before editing.
4. Fall back to native search only if `COVERAGE_NOTES` indicates missing coverage.
