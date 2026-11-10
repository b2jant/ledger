# LEDGER

LEDGER compiles local repository evidence into context for coding assistants. It provides a Unix CLI and a stdio MCP server with `ledger_context`, `ledger_expand`, and the `enhance` / `plan` prompts. Lexical search, scoped parsing, contract evidence, and Git/Jujutsu history help identify files to edit, related code, and candidate verification commands.

This release focuses on dependable CLI/MCP delivery. A dashboard, embeddings, collaborative annotations, and `ledger ship` remain future work. See [ARCHITECTURE.md](ARCHITECTURE.md) for implementation boundaries and target design.

## Install locally

Use macOS or Linux with a Rust toolchain and a native C compiler (needed by bundled SQLite and parsers). The checked-in CI configuration uses Rust 1.97.1; the workspace declares Rust 1.89 as its minimum, which is not separately exercised by that workflow. Windows is unsupported because the daemon uses Unix sockets and Unix file locking.

From this source directory:

```sh
cargo install --locked --path crates/ledger-cli --bin ledger
ledger --help
```

Cargo normally installs into `~/.cargo/bin`; ensure that directory is on your PATH. This is a local source installation, not a claim that a package or downloadable release has been published. See the [Cargo installation documentation](https://doc.rust-lang.org/cargo/commands/cargo-install.html).

## CLI

```sh
ledger --workspace /absolute/path/to/repo init
ledger --workspace /absolute/path/to/repo query 'fix signature validation' --anchor verify_signature
ledger --workspace /absolute/path/to/repo query 'fix signature validation' --fresh
ledger --workspace /absolute/path/to/repo index
ledger --workspace /absolute/path/to/repo status
ledger --workspace /absolute/path/to/repo expand 'snap:HANDLE_FROM_QUERY' 'slice:src/lib.rs:1-40'
ledger --workspace /absolute/path/to/repo stop
ledger --workspace /absolute/path/to/repo restart
```

`--workspace` defaults to the current directory. `init` creates/indexes the local database through the owner daemon and adds database/runtime exclusions to `.gitignore`. `init --install-skills` additionally writes optional repository-local prompt instructions in `.claude/commands/ledger-enhance.md` and `.agents/skills/ledger-enhance/SKILL.md`; it does not configure an MCP client.

Normal queries use the available snapshot and live source checks without waiting for an unconditional index pass. `--fresh` explicitly waits for a synchronized index generation, as does `index`. While running, the daemon reconciles allowed files and VCS state every two seconds. This is polling, with coalesced jobs and parse reuse for unchanged content, not an event watcher or a guaranteed two-second freshness deadline. Large scans take longer. New/deleted files and edits to previously unselected files are discovered by reconciliation.

Copy handles and expansion IDs from actual responses. Existing advertised expansion types include `body:`, `slice:`, `callers:`, and historical `diff:`; a separate `contract:` grammar is not implemented. Complete expansion returns content within the requested budget and may advertise continuations. Delta compares against full content actually rendered in the original context response. Retrying an unchanged expansion returns the same result; expanding does not mutate that baseline. Handles are bounded, process-local metadata and may expire or become invalid after restart; re-query for a new handle.

## Connect an MCP client

Configure any MCP client that supports launching a local stdio process with these values, using the actual absolute executable and repository paths:

```text
transport: stdio
command: /absolute/path/to/ledger
arguments: ["--workspace", "/absolute/path/to/repo", "mcp"]
```

These are generic launch settings, not a provider-specific configuration file. Consult your client's own documentation for its configuration format. LEDGER negotiates MCP version `2025-06-18`, exposes two read tools (`ledger_context`, `ledger_expand`) and two prompts (`enhance`, `plan`). A prompt provides repository grounding for the assistant you already use; LEDGER does not run a generative model.

MCP uses newline-delimited UTF-8 JSON-RPC on stdin/stdout. Only protocol responses go to stdout; errors and diagnostics go to stderr. Stdio sessions have no idle read timeout. Incoming frames are limited to 1 MiB and encoded responses to 4 MiB. Oversized or incomplete frames end the session with an error. See the official [stdio transport](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports) and [version negotiation](https://modelcontextprotocol.io/specification/2025-06-18/basic/lifecycle) specifications.

Each operation connects to a shared owner daemon for the canonical workspace root. A dormant owner exits after 15 minutes with no active IPC connection; polling does not extend that deadline. An existing MCP proxy stays bound to its observed owner identity and reports a connection/identity error after an owner exits or changes. Restart that MCP process and obtain fresh context handles. To run an owner with a different idle interval explicitly:

```sh
ledger --workspace /absolute/path/to/repo daemon --idle-seconds 3600
```

## Ownership and local data

The owner holds `.ledger/runtime/daemon.lock` before opening or migrating `.ledger/ledger.db`. CLI/MCP connection, startup, permission, and identity failures never fall back to an unlocked in-process database writer. The library's explicit `EmbeddedClient` remains available for isolated tests and benchmarks.

Handshakes identify workspace, SQLite schema, wire protocol, executable build, database incarnation, and owner lifetime. Requests carry that identity and responses echo it on the same socket. Executable hashing happens once per process. Stale clients cannot silently bind their handles to a replacement owner/database. Replacing the database file under a running owner is unsupported and detected; stop first before backup/restore/removal. Do not edit database files in place.

`stop` and `restart` negotiate shutdown only with a validated same-workspace daemon advertising the supported shutdown capability. A build/schema mismatch can trigger that same graceful handoff automatically. Legacy incompatible daemons require their original executable's shutdown mechanism or a natural idle exit; LEDGER does not force-kill them or migrate under an active owner. Shutdown stops admission/polling, drains accepted requests and queued/running indexing jobs, checkpoints, removes its socket, then releases the lock. A client can report a slow-drain timeout while the owner continues holding the lock safely.

Runtime directories use mode `0700` and sockets `0600`. Long workspace paths use a short per-user socket path beneath `/tmp/ledger-rt-*`; the ownership lock remains inside the workspace. These controls are for a local user account, not isolation from hostile processes running as that same user.

## Scope and security limits

LEDGER applies filename/path filtering, ignore rules, workspace containment checks and nested repository boundaries to source access and evidence expansion. Examples of excluded paths include `.env` variants (with example/template exceptions), private-key/credential filenames, VCS internals, `.ledger`, and build artifacts. Historical paths are checked lexically so deleted allowed files can still provide history. Rust, TypeScript/JavaScript, Python and Go receive scoped parsing; unsupported or malformed source can have lexical-only coverage. Relationship confidence and coverage notes describe limits; candidate test commands are evidence, not tests LEDGER has executed.

The filter does **not** scan or redact arbitrary secret values inside otherwise allowed source or documentation. Allowed files, commit messages, and local telemetry may contain sensitive information. Query summaries and evidence metadata are retained in the local SQLite database. Review the repository and the assistant/client receiving its output accordingly. Repository content and history remain untrusted data; evidence labels do not prevent prompt injection. LEDGER itself requires no cloud model service for retrieval, but an attached assistant may transmit tool output under its own policies.

## Troubleshooting

- **macOS link error mentioning an unknown SDK architecture:** an inherited `SDKROOT` can select a newer SDK than the active Xcode linker supports. Select the active Xcode SDK for one command, for example `SDKROOT="$(env -u SDKROOT xcrun --sdk macosx --show-sdk-path)" cargo test --locked --workspace`. This does not change the system's developer-tool selection.
- **Owner startup failed:** inspect `.ledger/runtime/daemon.log`, check directory/socket permissions and run `ledger daemon` in the foreground. Do not delete a live owner's lock file; ownership follows the locked file, not its name.
- **Build/schema/protocol mismatch:** try `ledger restart`. If the daemon predates safe shutdown negotiation, stop it with its original executable or wait for idle exit, then retry. Unrelated-workspace responses fail safely.
- **Database/owner changed or unknown handle:** restart the MCP proxy and run a new context query. Handles are not durable across daemon lifetimes or database replacement.
- **Recently added code missing:** wait for polling, inspect coverage/status, or run `index` / `query --fresh`. Ignored, nested-repository or excluded files remain outside scope.
- **Frame too large:** split requests, reduce anchors/expansion batches and token budgets. IPC frames are limited to 4 MiB, with five-second total request-read and response-write deadlines. Synchronous client operations have a 120-second response deadline; a client timeout does not abandon daemon writes or release ownership.

## Contributing with Jujutsu

Use Jujutsu (`jj`) for all changes and branches in this repository, as required by
[AGENTS.md](AGENTS.md). GitHub hosts the Git history; local work uses a colocated
Jujutsu/Git workspace. Install `jj` with `brew install jj` on macOS, or follow the
[official installation instructions](https://docs.jj-vcs.dev/latest/install-and-setup/).

For a new checkout:

```sh
jj git clone --colocate https://github.com/b2jant/ledger.git LEDGER
cd LEDGER
jj config set --repo user.name "Your Name"
jj config set --repo user.email "you@example.com"
```

The original development folder is already initialized. In an existing Git-only
clone, run `jj git init --colocate` once. Keep `.jj/` local.

To start and publish a feature:

```sh
jj status
jj git fetch --remote origin
jj new main@origin
# Edit files and run the relevant checks.
jj diff
jj commit -m "Describe the change"
jj bookmark create feature/my-change -r @-
jj git push --remote origin --bookmark feature/my-change
```

Bookmarks become branches on GitHub. After further commits, advance the existing
bookmark with `jj bookmark set feature/my-change -r @-` before pushing it again.
`jj commit` leaves a new empty working-copy commit at `@`; `@-` is its completed
parent. Use `jj` for history changes and synchronization; read-only Git tools and
GitHub CLI remain available. See the [Jujutsu GitHub guide](https://docs.jj-vcs.dev/latest/github/).

## Reproduce checks and fixture measurements

When upgrading LEDGER itself, follow the standing [LEDGER-first workflow](AGENTS.md)
and record useful results, misses and necessary fallback searches in the
[usage feedback log](LEDGER_FEEDBACK.md). Suggestions must be checked against actual
source and tests; a successful retrieval is not proof that it found everything.

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo run --locked --release -p ledger-cli -- bench
```

`bench` builds temporary synthetic repositories and reports local B0/L1/L2/L3 retrieval measurements. Recall is against fixture expectations; exploration-round counts are estimates. This is not an end-to-end assistant success evaluation, a broad secret-detection test, or a general memory/latency guarantee. Record the toolchain, machine, build profile and complete output when comparing runs.

CI is configured for Ubuntu and macOS and includes real-process CLI/MCP smoke tests. The manual binary-artifact workflow builds with a pinned toolchain and lockfile, records build metadata, and creates deterministic archive metadata plus SHA-256 checksums. It does not publish a release or claim bit-identical binaries across different compiler/OS environments. Workflows follow official [Rust CI guidance](https://docs.github.com/en/actions/tutorials/build-and-test-code/rust) and the supported [runner labels](https://docs.github.com/en/actions/how-tos/write-workflows/choose-where-workflows-run/choose-the-runner-for-a-job). They require a GitHub repository to execute; adding them locally does not run CI.
