# miden-note-consume-repro

Minimal reproduction of a bug where consuming a custom note works with
MockChain but fails with the real miden-client (`StackReadFailed`).

## The Bug

A custom Miden note script that uses `active_note::get_storage` and
`wallet::add_assets_to_account` executes correctly in MockChain tests but
fails when consumed via the real `miden-client` (0.14.5) against testnet.

**Error:**
```
TransactionExecutorError(
  TransactionProgramExecutionFailed(
    AdviceError { err: StackReadFailed }
  )
)
```

**Key observations:**
- MockChain tests pass (the note script is correct)
- A noop transaction (no input notes) succeeds with the same client setup
- The error occurs whether the note is authenticated or unauthenticated
- The error occurs with or without custom advice map entries
- The error suggests the VM's advice provider is not correctly populated
  when the real client executes a custom note script

## How to Reproduce

### MockChain test (passes)

```bash
cargo test --test mock_test -- --nocapture
```

Both tests should pass:
- `mock_consume_secret_hash_note` — correct secret, note consumed
- `mock_wrong_secret_rejected` — wrong secret, correctly rejected

### Real client test (fails with StackReadFailed)

```bash
cargo test --test real_test -- --ignored --nocapture
```

- `real_consume_secret_hash_note` — reproduces the StackReadFailed error
- `real_noop_transaction_succeeds` — proves client setup is correct

## Note Script

The custom note script (`masm/secret_hash_note.masm`) is minimal:

1. Hashes the note_args (the secret)
2. Loads the expected digest from note storage via `active_note::get_storage`
3. Compares the two digests
4. If they match, calls `wallet::add_assets_to_account`

This is the pattern from the Miden docs custom note tutorial.

## Environment

- miden-client: 0.14.9
- miden-client-sqlite-store: 0.14.9
- miden-protocol: 0.14.5
- miden-testing: 0.14.6
- miden-standards: 0.14.5
- Testnet: https://rpc.testnet.miden.io

### Known Issues

**Bug 1 (miden-client 0.14.5): StackReadFailed on custom note consumption**
Custom note scripts that use `active_note::get_storage` work in MockChain but
fail with `StackReadFailed` when consumed via the real miden-client.

**Bug 2 (miden-client 0.14.9): MMR sync panic**
`sync_state()` panics with `MMR peaks stored for a block header must use that
block number as the forest` during the sync poll loop. This prevents the real
test from reaching the note consumption step. This may be a regression in
0.14.9 — sync works fine in 0.14.5.

## Expected Behavior

Both MockChain and real client tests should pass — the note script is
valid and the secret is correct.

## Actual Behavior

- MockChain: PASS
- Real client: FAIL with `StackReadFailed`

The `StackReadFailed` error comes from the VM's advice provider, suggesting
that the real client's transaction executor does not correctly populate the
advice stack/map for custom note scripts that use `active_note::get_storage`.
MockChain's `build_tx_context` apparently handles this differently.
