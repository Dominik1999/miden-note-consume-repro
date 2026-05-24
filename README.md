# miden-note-consume-repro

Minimal reproduction of a bug where consuming a custom note works with
`MockRpcApi`/`MockChain` but fails with `StackReadFailed` against the
real Miden testnet.

**Issue:** [0xMiden/miden-client#2201](https://github.com/0xMiden/miden-client/issues/2201)

## The Bug

A custom Miden note script that uses `active_note::get_storage`,
`falcon512_poseidon2::verify`, `wallet::add_assets_to_account`,
`p2id::new`, and `note::build_recipient` works correctly in all
offline/MockChain tests but fails with `StackReadFailed` when consumed
via `miden-client` against the real testnet.

**Error:**
```
TransactionExecutorError(
  TransactionProgramExecutionFailed(
    AdviceError { err: StackReadFailed }
  )
)
```

The error occurs during local VM execution (before proving or submission).

## Test Results

| Test | RPC Backend | Result |
|------|------------|--------|
| MockChain: simple hash note (2 tests) | MockChain | PASS |
| MockChain: Falcon + P2ID (2 tests) | MockChain | PASS |
| MockChain: Falcon + P2ID + has_mapkey | MockChain | PASS |
| MockChain: full ADN MASM | MockChain | PASS |
| Real testnet: simple hash note | Testnet | PASS |
| Real testnet: Falcon + P2ID | Testnet | PASS |
| Real testnet: Falcon + P2ID + has_mapkey | Testnet | PASS |
| Real testnet: full ADN MASM (same client) | Testnet | PASS |
| Real testnet: cross-client ADN (same process) | Testnet | PASS |
| Offline cross-client ADN | MockRpcApi | PASS |
| **Testnet cross-client (setup-testnet files)** | **Testnet** | **FAIL** |

## What Was Ruled Out

- **Note authentication**: Note IS authenticated (Committed state, has inclusion proof)
- **Advice map / Falcon signature**: Fails even without advice map entries
- **Account deployment**: Fails even after deploying facilitator on-chain + waiting 3 blocks
- **Note import method**: Fails whether imported via `NoteFile::NoteDetails` or discovered via sync
- **Tokio runtime**: Fails on both single-threaded and multi-threaded runtimes
- **Serialization**: `Note::to_bytes()` / `Note::read_from_bytes()` roundtrips correctly
- **MASM script**: Same ADN MASM passes all offline tests and same-client testnet tests

## The Core Difference

The ONLY difference between the passing offline test and the failing testnet
test is the RPC backend:

- `MockRpcApi` (backed by `MockChain`) → **PASS**
- `GrpcClient` (real testnet) → **FAIL**

Something in how the real testnet's sync response data populates the
executor's advice provider differs from `MockRpcApi`, causing
`StackReadFailed` during custom note script execution.

## How to Reproduce

### MockChain tests (all pass, no network needed)

```bash
cargo test --test mock_test -- --nocapture
# 6 tests, all pass
```

### Offline cross-client test (passes, no network needed)

```bash
cargo test --test offline_cross_client_test -- --nocapture
# Uses MockRpcApi, two separate clients, serialize/deserialize note between them
# ~260s (includes STARK proving)
```

### Real testnet tests (pass — same client creates + consumes)

```bash
cargo test --test real_test -- --ignored --nocapture
# 5 tests: simple note, Falcon+P2ID, has_mapkey, full ADN, cross-client
# All pass (~400s each, deploys accounts on testnet)
```

### Facilitator repro test (FAILS — the bug)

```bash
# Step 1: Create testnet state (one-time, ~30s)
# Requires the miden-x402-experiment1 repo's setup-testnet binary
cd /path/to/miden-x402-experiment1
./target/release/setup-testnet \
  --agents 1 --mint-amount 1000000 \
  --adn --adn-amount 100000 \
  --out-dir /tmp/adn-state

# Step 2: Reproduce the bug (<1s)
cd /path/to/miden-note-consume-repro
SETUP_DIR=/tmp/adn-state cargo test --test facilitator_repro_test -- --ignored --nocapture
```

The facilitator_repro_test reads `facilitator_account.b64` and `adn_note.b64`
from the setup output, creates a fresh `miden-client`, imports the account +
note, syncs from testnet, and tries to consume. This matches exactly what the
facilitator server does in production.

## Note Scripts

- `masm/secret_hash_note.masm` — Simple hash preimage check (works everywhere)
- `masm/falcon_p2id_note.masm` — Falcon sig + P2ID output (works everywhere)
- `masm/falcon_p2id_hasmapkey_note.masm` — Same with `adv.has_mapkey` (works everywhere)
- `masm/agent_debit_note.masm` — Full AgentDebitNote: Falcon sig, block height check, P2ID + remainder output notes (fails only on testnet cross-client)

## Environment

- miden-client: v0.14.9 from `main` branch (commit `bfe962ae`, not crates.io release)
- miden-client-sqlite-store: same git source
- miden-protocol: 0.14.5 (crates.io)
- miden-testing: 0.14.6 (crates.io)
- miden-standards: 0.14.5 (crates.io)
- Testnet: https://rpc.testnet.miden.io
- Platform: macOS (Apple Silicon)

```toml
[dependencies]
miden-client = { git = "https://github.com/0xMiden/miden-client.git", branch = "main", features = ["testing", "tonic"] }
miden-client-sqlite-store = { git = "https://github.com/0xMiden/miden-client.git", branch = "main" }
miden-protocol = "0.14"
miden-testing = "0.14"
miden-standards = "0.14"
```

## Context

This repro was created while building [x402 payments on Miden](https://github.com/Dominik1999/miden-x402-experiment1)
using AgentDebitNote — a custom note with Falcon-512 signature verification,
dual-signature enforcement, and self-reproducing remainder notes.
