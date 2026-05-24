//! Cross-client P2ID vs Custom note comparison test.
//!
//! Two tests that both use TWO separate miden-clients connected to testnet:
//!
//! 1. `p2id_cross_client` — standard P2ID note (expected to PASS)
//! 2. `custom_note_cross_client` — custom secret_hash note (expected to FAIL
//!    with StackReadFailed due to missing account seed in advice map)
//!
//! Run with:
//!   RUST_LOG=miden_client::transaction=info cargo test --test p2id_cross_client_test -- --ignored --nocapture

use std::sync::Arc;
use std::time::Duration;

use miden_client::account::component::{AuthControlled, BasicFungibleFaucet, BasicWallet};
use miden_client::account::{AccountBuilder, AccountStorageMode, AccountType};
use miden_client::asset::{FungibleAsset, TokenSymbol};
use miden_client::auth::{AuthSchemeId, AuthSecretKey, AuthSingleSig};
use miden_client::builder::ClientBuilder;
use miden_client::keystore::{FilesystemKeyStore, Keystore};
use miden_client::note::NoteType;
use miden_client::rpc::{Endpoint, GrpcClient};
use miden_client::transaction::{PaymentNoteDescription, TransactionRequestBuilder};
use miden_client::{Client, Felt};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::asset::Asset;
use miden_protocol::note::{
    Note, NoteAssets, NoteDetails, NoteFile, NoteMetadata, NoteRecipient, NoteStorage, NoteTag,
};
use miden_protocol::{Hasher, Word};
use miden_standards::code_builder::CodeBuilder;
use rand::RngCore;

const NOTE_MASM: &str = include_str!("../masm/secret_hash_note.masm");

/// The secret that the note consumer must know.
fn secret() -> Word {
    [Felt::new(42), Felt::new(43), Felt::new(44), Felt::new(45)].into()
}

/// The digest of the secret, stored in the note's storage.
fn secret_digest() -> Word {
    let s: [Felt; 4] = secret().into();
    Hasher::hash_elements(&s)
}

async fn build_client(
    data_dir: &std::path::Path,
) -> anyhow::Result<(Client<FilesystemKeyStore>, Arc<FilesystemKeyStore>)> {
    let endpoint = Endpoint::try_from("https://rpc.testnet.miden.io")
        .map_err(|e| anyhow::anyhow!("endpoint: {e:?}"))?;
    let rpc = Arc::new(GrpcClient::new(&endpoint, 30_000));
    let keystore = Arc::new(
        FilesystemKeyStore::new(data_dir.join("keystore"))
            .map_err(|e| anyhow::anyhow!("keystore: {e:?}"))?,
    );
    let store = data_dir.join("store.sqlite3");
    let client = ClientBuilder::new()
        .rpc(rpc)
        .sqlite_store(store)
        .authenticator(keystore.clone())
        .in_debug_mode(true.into())
        .build()
        .await
        .map_err(|e| anyhow::anyhow!("client build: {e}"))?;
    Ok((client, keystore))
}

fn rand_seed(client: &mut Client<FilesystemKeyStore>) -> [u8; 32] {
    let mut seed = [0u8; 32];
    client.rng().fill_bytes(&mut seed);
    seed
}

/// Deploy a faucet, return its AccountId.
async fn deploy_faucet(
    client: &mut Client<FilesystemKeyStore>,
    keystore: &Arc<FilesystemKeyStore>,
    symbol_str: &str,
) -> anyhow::Result<miden_protocol::account::AccountId> {
    let seed = rand_seed(client);
    let key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let symbol =
        TokenSymbol::new(symbol_str).map_err(|e| anyhow::anyhow!("TokenSymbol: {e:?}"))?;
    let faucet = AccountBuilder::new(seed)
        .account_type(AccountType::FungibleFaucet)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(
            BasicFungibleFaucet::new(symbol, 6, Felt::new(1_000_000_000))
                .map_err(|e| anyhow::anyhow!("BasicFungibleFaucet: {e:?}"))?,
        )
        .with_component(AuthControlled::allow_all())
        .build()
        .map_err(|e| anyhow::anyhow!("faucet build: {e:?}"))?;
    let faucet_id = faucet.id();
    client.add_account(&faucet, false).await?;
    keystore
        .add_key(&key, faucet_id)
        .await
        .map_err(|e| anyhow::anyhow!("faucet keystore: {e:?}"))?;
    Ok(faucet_id)
}

/// Deploy a wallet, return its AccountId.
async fn deploy_wallet(
    client: &mut Client<FilesystemKeyStore>,
    keystore: &Arc<FilesystemKeyStore>,
) -> anyhow::Result<miden_protocol::account::AccountId> {
    let seed = rand_seed(client);
    let key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let wallet = AccountBuilder::new(seed)
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(BasicWallet)
        .build()
        .map_err(|e| anyhow::anyhow!("wallet build: {e:?}"))?;
    let wallet_id = wallet.id();
    client.add_account(&wallet, false).await?;
    keystore
        .add_key(&key, wallet_id)
        .await
        .map_err(|e| anyhow::anyhow!("wallet keystore: {e:?}"))?;
    Ok(wallet_id)
}

/// Mint tokens and wait for the mint note to become consumable, then consume it.
async fn mint_and_consume(
    client: &mut Client<FilesystemKeyStore>,
    faucet_id: miden_protocol::account::AccountId,
    target_id: miden_protocol::account::AccountId,
    amount: u64,
) -> anyhow::Result<()> {
    let mint_asset = FungibleAsset::new(faucet_id, amount)
        .map_err(|e| anyhow::anyhow!("FungibleAsset: {e:?}"))?;
    let mint_req = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(mint_asset, target_id, NoteType::Public, client.rng())
        .map_err(|e| anyhow::anyhow!("mint tx build: {e:?}"))?;
    let mint_tx = client.submit_new_transaction(faucet_id, mint_req).await?;
    eprintln!("  mint tx submitted: {mint_tx}");

    // Wait for consumable, then consume
    for attempt in 0..60 {
        client.sync_state().await?;
        let consumable = client.get_consumable_notes(Some(target_id)).await?;
        if !consumable.is_empty() {
            eprintln!(
                "  found {} consumable notes (attempt {attempt}), consuming...",
                consumable.len()
            );
            let notes: Vec<_> = consumable
                .into_iter()
                .map(|(note, _)| note.try_into())
                .collect::<Result<_, _>>()?;
            let consume_req = TransactionRequestBuilder::new()
                .build_consume_notes(notes)
                .map_err(|e| anyhow::anyhow!("consume build: {e:?}"))?;
            let consume_tx = client
                .submit_new_transaction(target_id, consume_req)
                .await?;
            eprintln!("  mint note consumed: {consume_tx}");
            return Ok(());
        }
        if attempt == 59 {
            anyhow::bail!("timed out waiting for consumable mint note");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    unreachable!()
}

/// Wait for a specific note to become consumable for a given account.
async fn wait_for_note_consumable(
    client: &mut Client<FilesystemKeyStore>,
    account_id: miden_protocol::account::AccountId,
    note_id: miden_protocol::note::NoteId,
    max_attempts: usize,
) -> anyhow::Result<bool> {
    for attempt in 0..max_attempts {
        client.sync_state().await?;
        let consumable = client.get_consumable_notes(Some(account_id)).await?;
        if consumable.iter().any(|(n, _)| n.id() == note_id) {
            eprintln!("  note {note_id} is consumable (attempt {attempt})");
            return Ok(true);
        }
        if attempt == max_attempts - 1 {
            eprintln!("  WARNING: note {note_id} never became consumable after {max_attempts} attempts");
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    Ok(false)
}

// ═══════════════════════════════════════════════════════════════════════════
// TEST 1: Standard P2ID note across two clients (expected to PASS)
// ═══════════════════════════════════════════════════════════════════════════

/// Cross-client P2ID test:
///   Client A: deploys faucet + wallet A, mints tokens, consumes mint note,
///             sends a standard P2ID note to wallet B.
///   Client B: deploys wallet B, syncs, discovers the P2ID note, consumes it.
///
/// This test verifies that standard P2ID notes work across separate clients
/// on testnet. We expect this to PASS.
#[tokio::test]
#[ignore = "requires network access to Miden testnet"]
async fn p2id_cross_client() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    // ── Client A setup ──
    let tmp_a = tempfile::tempdir()?;
    let (mut client_a, keystore_a) = build_client(tmp_a.path()).await?;
    client_a.sync_state().await?;
    eprintln!("[p2id] step 0: client A synced");

    // Deploy faucet via client A
    let faucet_id = deploy_faucet(&mut client_a, &keystore_a, "PTEST").await?;
    eprintln!("[p2id] step 1: faucet deployed: {}", faucet_id.to_hex());

    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Deploy wallet A via client A
    let wallet_a_id = deploy_wallet(&mut client_a, &keystore_a).await?;
    eprintln!("[p2id] step 2: wallet A deployed: {}", wallet_a_id.to_hex());

    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Mint tokens to wallet A and consume
    eprintln!("[p2id] step 3: minting tokens to wallet A...");
    mint_and_consume(&mut client_a, faucet_id, wallet_a_id, 10_000).await?;
    eprintln!("[p2id] step 3: wallet A funded");

    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ── Client B setup ──
    let tmp_b = tempfile::tempdir()?;
    let (mut client_b, keystore_b) = build_client(tmp_b.path()).await?;
    client_b.sync_state().await?;
    eprintln!("[p2id] step 4: client B synced");

    // Deploy wallet B via client B
    let wallet_b_id = deploy_wallet(&mut client_b, &keystore_b).await?;
    eprintln!("[p2id] step 4: wallet B deployed: {}", wallet_b_id.to_hex());

    client_b.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ── Client A sends P2ID note to wallet B ──
    eprintln!("[p2id] step 5: client A sending P2ID note to wallet B...");
    let send_amount = 1_000u64;
    let send_asset = FungibleAsset::new(faucet_id, send_amount)
        .map_err(|e| anyhow::anyhow!("FungibleAsset: {e:?}"))?;

    let payment = PaymentNoteDescription::new(
        vec![Asset::Fungible(send_asset)],
        wallet_a_id,
        wallet_b_id,
    );
    let p2id_req = TransactionRequestBuilder::new()
        .build_pay_to_id(payment, NoteType::Public, client_a.rng())
        .map_err(|e| anyhow::anyhow!("build_pay_to_id: {e:?}"))?;
    let p2id_tx = client_a.submit_new_transaction(wallet_a_id, p2id_req).await?;
    eprintln!("[p2id] step 5: P2ID tx submitted: {p2id_tx}");

    // ── Client B discovers and consumes the P2ID note ──
    eprintln!("[p2id] step 6: client B waiting for P2ID note...");
    for attempt in 0..60 {
        client_b.sync_state().await?;
        let consumable = client_b.get_consumable_notes(Some(wallet_b_id)).await?;
        if !consumable.is_empty() {
            eprintln!(
                "[p2id] step 6: found {} consumable notes for wallet B (attempt {attempt})",
                consumable.len()
            );

            let notes: Vec<_> = consumable
                .into_iter()
                .map(|(note, _)| note.try_into())
                .collect::<Result<_, _>>()?;

            let consume_req = TransactionRequestBuilder::new()
                .build_consume_notes(notes)
                .map_err(|e| anyhow::anyhow!("consume build: {e:?}"))?;

            match client_b
                .submit_new_transaction(wallet_b_id, consume_req)
                .await
            {
                Ok(tx_id) => {
                    eprintln!("[p2id] step 6: P2ID consume SUCCEEDED: {tx_id}");
                    eprintln!("[p2id] Standard P2ID works cross-client on testnet (as expected).");
                }
                Err(e) => {
                    let err_str = format!("{e:?}");
                    eprintln!("[p2id] step 6: P2ID consume FAILED: {err_str}");
                    eprintln!("[p2id] UNEXPECTED: standard P2ID should work cross-client!");
                    anyhow::bail!("P2ID cross-client consume failed: {e}");
                }
            }
            break;
        }
        if attempt == 59 {
            anyhow::bail!("timed out waiting for P2ID note on client B");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    eprintln!("[p2id] DONE: P2ID cross-client test passed.");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// TEST 2: Custom secret_hash note across two clients (expected to FAIL)
// ═══════════════════════════════════════════════════════════════════════════

/// Cross-client custom note test:
///   Client A: deploys faucet + wallet A, mints tokens, consumes mint note,
///             creates a custom secret_hash note via `own_output_notes`.
///   Client B: deploys wallet B, imports the note, syncs, tries to consume
///             with `input_notes([(note, Some(secret))])`.
///
/// This test checks whether the custom note has the same missing-account-seed
/// issue as observed in testnet debugging (8 advice map keys vs 9 offline).
#[tokio::test]
#[ignore = "requires network access to Miden testnet"]
async fn custom_note_cross_client() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    // ── Client A setup ──
    let tmp_a = tempfile::tempdir()?;
    let (mut client_a, keystore_a) = build_client(tmp_a.path()).await?;
    client_a.sync_state().await?;
    eprintln!("[custom] step 0: client A synced");

    // Deploy faucet via client A
    let faucet_id = deploy_faucet(&mut client_a, &keystore_a, "CTEST").await?;
    eprintln!("[custom] step 1: faucet deployed: {}", faucet_id.to_hex());

    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Deploy wallet A via client A
    let wallet_a_id = deploy_wallet(&mut client_a, &keystore_a).await?;
    eprintln!("[custom] step 2: wallet A deployed: {}", wallet_a_id.to_hex());

    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Mint tokens to wallet A and consume
    eprintln!("[custom] step 3: minting tokens to wallet A...");
    mint_and_consume(&mut client_a, faucet_id, wallet_a_id, 10_000).await?;
    eprintln!("[custom] step 3: wallet A funded");

    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ── Client A creates the custom note ──
    let note_script = CodeBuilder::default().compile_note_script(NOTE_MASM)?;
    eprintln!("[custom] step 4: custom note script compiled");

    let asset = FungibleAsset::new(faucet_id, 1000)
        .map_err(|e| anyhow::anyhow!("asset: {e:?}"))?;
    let storage = NoteStorage::new(vec![
        secret_digest()[0],
        secret_digest()[1],
        secret_digest()[2],
        secret_digest()[3],
    ])?;

    let mut serial_bytes = [0u8; 32];
    client_a.rng().fill_bytes(&mut serial_bytes);
    let serial_num: Word = [
        Felt::new(u64::from_le_bytes(serial_bytes[0..8].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[8..16].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[16..24].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[24..32].try_into().unwrap())),
    ]
    .into();

    // ── Client B setup (before creating the note, so we know wallet B's ID) ──
    let tmp_b = tempfile::tempdir()?;
    let (mut client_b, keystore_b) = build_client(tmp_b.path()).await?;
    client_b.sync_state().await?;
    eprintln!("[custom] step 4b: client B synced");

    // Deploy wallet B via client B
    let wallet_b_id = deploy_wallet(&mut client_b, &keystore_b).await?;
    eprintln!("[custom] step 4b: wallet B deployed: {}", wallet_b_id.to_hex());

    client_b.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Build the custom note targeting wallet B
    let tag = NoteTag::with_account_target(wallet_b_id);
    let metadata = NoteMetadata::new(wallet_a_id, NoteType::Public).with_tag(tag);
    let vault = NoteAssets::new(vec![Asset::Fungible(asset)])?;
    let recipient = NoteRecipient::new(serial_num, note_script, storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();
    eprintln!("[custom] step 5: custom note built, id={note_id}");

    // Submit the note on-chain via own_output_notes from wallet A
    let create_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![note.clone()])
        .build()
        .map_err(|e| anyhow::anyhow!("create note tx: {e:?}"))?;
    let create_tx = client_a
        .submit_new_transaction(wallet_a_id, create_req)
        .await?;
    eprintln!("[custom] step 5: custom note submitted on-chain: {create_tx}");

    // ── Client B imports and consumes the custom note ──
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Import the note into client B's store
    let note_details = NoteDetails::new(note.assets().clone(), note.recipient().clone());
    let note_file = NoteFile::NoteDetails {
        details: note_details,
        after_block_num: 0u32.into(),
        tag: Some(tag),
    };
    client_b.import_notes(&[note_file]).await?;
    eprintln!("[custom] step 6: note imported into client B");

    // Wait for note to become consumable on client B
    eprintln!("[custom] step 6: waiting for custom note to become consumable on client B...");
    let found = wait_for_note_consumable(&mut client_b, wallet_b_id, note_id, 60).await?;
    if !found {
        eprintln!("[custom] WARNING: note never became consumable, trying to consume anyway...");
    }

    // Try to consume the custom note on client B
    eprintln!("[custom] step 7: client B attempting to consume custom note with secret...");
    eprintln!("[custom]   (check PREPARED_ADVICE log for map_keys count)");

    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(note, Some(secret()))])
        .build()
        .map_err(|e| anyhow::anyhow!("consume custom note tx: {e:?}"))?;

    match client_b
        .submit_new_transaction(wallet_b_id, consume_req)
        .await
    {
        Ok(tx_id) => {
            eprintln!("[custom] step 7: UNEXPECTED SUCCESS: tx_id={tx_id}");
            eprintln!("[custom]   Custom note consumed successfully cross-client!");
            eprintln!("[custom]   The bug may have been fixed, or this note type is not affected.");
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!("[custom] step 7: FAILED:");
            eprintln!("[custom]   {}", &err_str[..err_str.len().min(500)]);
            if err_str.contains("StackReadFailed") {
                eprintln!("[custom]   Confirmed: StackReadFailed error!");
                eprintln!("[custom]   Custom note fails cross-client on testnet.");
                eprintln!("[custom]   Compare PREPARED_ADVICE map_keys count with P2ID test above.");
            } else {
                eprintln!("[custom]   Different error than StackReadFailed.");
            }
            // Don't fail the test -- we're observing, not asserting
        }
    }

    eprintln!("\n[custom] DONE. Compare PREPARED_ADVICE map_keys between p2id and custom tests.");
    eprintln!("[custom] If P2ID has 9 keys and custom has 8, the missing key is account seed.");
    Ok(())
}
