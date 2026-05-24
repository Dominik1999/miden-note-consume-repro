//! ADN variant test: simplified ADN (no remainder note) cross-client on testnet.
//!
//! Tests whether removing the remainder note creation (section 7) from the ADN
//! MASM fixes the StackReadFailed error that occurs during cross-client consumption.
//!
//! The test creates everything in-process (no SETUP_DIR files needed for accounts/notes):
//!   Client A: deploys faucet + wallet, mints tokens, creates a simplified ADN note
//!   Client B: imports the note from serialized bytes, syncs, attempts to consume
//!
//! Run with:
//!   RUST_LOG=info cargo test --test adn_variant_test -- --ignored --nocapture

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
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::{Client, Felt};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::asset::Asset;
use miden_protocol::note::{
    Note, NoteAssets, NoteDetails, NoteFile, NoteMetadata, NoteRecipient, NoteStorage, NoteTag,
};
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_protocol::{Hasher, Word};
use miden_standards::code_builder::CodeBuilder;
use rand::RngCore;

const ADN_NO_REMAINDER_MASM: &str = include_str!("../masm/adn_no_remainder.masm");
const ADN_FULL_MASM: &str = include_str!("../masm/agent_debit_note.masm");

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

async fn deploy_wallet(
    client: &mut Client<FilesystemKeyStore>,
    keystore: &Arc<FilesystemKeyStore>,
) -> anyhow::Result<(miden_protocol::account::AccountId, AuthSecretKey)> {
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
    Ok((wallet_id, key))
}

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

/// Helper: run the cross-client ADN test with a given MASM source.
/// Returns Ok(true) if consume succeeded, Ok(false) if it failed with an error
/// (logged but not propagated), or Err if setup failed.
async fn run_adn_cross_client_test(
    label: &str,
    masm_source: &str,
    debit_amount: u64,
    note_balance: u64,
) -> anyhow::Result<bool> {
    // ═══════════════════════════════════════════════════════════════
    // CLIENT A: creates accounts + note
    // ═══════════════════════════════════════════════════════════════
    let tmp_a = tempfile::tempdir()?;
    let (mut client_a, keystore_a) = build_client(tmp_a.path()).await?;
    client_a.sync_state().await?;
    eprintln!("[{label}] step 0: client A synced");

    // Deploy faucet
    let faucet_id = deploy_faucet(&mut client_a, &keystore_a, "AVTEST").await?;
    eprintln!("[{label}] step 1: faucet deployed: {}", faucet_id.to_hex());

    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Deploy wallet A (the note creator / "agent")
    let (wallet_a_id, _wallet_a_key) = deploy_wallet(&mut client_a, &keystore_a).await?;
    eprintln!("[{label}] step 2: wallet A (agent) deployed: {}", wallet_a_id.to_hex());

    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Mint tokens to wallet A
    eprintln!("[{label}] step 3: minting tokens to wallet A...");
    mint_and_consume(&mut client_a, faucet_id, wallet_a_id, 10_000).await?;
    eprintln!("[{label}] step 3: wallet A funded");

    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ── Client B setup (before note creation so we know facilitator ID) ──
    let tmp_b = tempfile::tempdir()?;
    let (mut client_b, keystore_b) = build_client(tmp_b.path()).await?;
    client_b.sync_state().await?;
    eprintln!("[{label}] step 4: client B synced");

    // Deploy facilitator wallet (client B's account — will consume the note)
    let (facilitator_id, _facilitator_key) = deploy_wallet(&mut client_b, &keystore_b).await?;
    eprintln!("[{label}] step 4: facilitator deployed on client B: {}", facilitator_id.to_hex());

    // Deploy merchant wallet on client B (P2ID recipient)
    let (merchant_id, _merchant_key) = deploy_wallet(&mut client_b, &keystore_b).await?;
    eprintln!("[{label}] step 4: merchant deployed on client B: {}", merchant_id.to_hex());

    client_b.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Fund the facilitator so it exists on-chain
    eprintln!("[{label}] step 4b: minting to facilitator so it exists on-chain...");
    // We need the faucet on client B too -- but faucet is on client A.
    // Instead, we import the facilitator account into client A and mint from there.
    // Actually, let's deploy a second faucet on client B for this purpose.
    let faucet_b_id = deploy_faucet(&mut client_b, &keystore_b, "BVTST").await?;
    client_b.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    mint_and_consume(&mut client_b, faucet_b_id, facilitator_id, 5_000).await?;
    eprintln!("[{label}] step 4b: facilitator funded");
    client_b.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ── Create the ADN note on client A ──
    let adn_agent_sk = AuthSecretKey::new_falcon512_poseidon2_with_rng(client_a.rng());
    let adn_agent_pk: Word = adn_agent_sk.public_key().to_commitment().into();
    let note_script = CodeBuilder::default().compile_note_script(masm_source)?;
    eprintln!("[{label}] step 5: note script compiled");

    let asset = FungibleAsset::new(faucet_id, note_balance)
        .map_err(|e| anyhow::anyhow!("asset: {e:?}"))?;

    // Storage: 7 items — agent_pk[4], user_suffix, user_prefix, expiry
    let storage = NoteStorage::new(vec![
        adn_agent_pk[0],
        adn_agent_pk[1],
        adn_agent_pk[2],
        adn_agent_pk[3],
        wallet_a_id.suffix(),          // user_suffix (index 4)
        wallet_a_id.prefix().as_felt(), // user_prefix (index 5)
        Felt::new(1_000_000),           // expiry_block_height (index 6) — far future
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

    // Tag targets the facilitator account
    let tag = NoteTag::with_account_target(facilitator_id);
    let metadata = NoteMetadata::new(wallet_a_id, NoteType::Public).with_tag(tag);
    let vault = NoteAssets::new(vec![Asset::Fungible(asset)])?;
    let recipient = NoteRecipient::new(serial_num, note_script, storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();
    eprintln!("[{label}] step 5: note built, id={note_id}");

    // Serialize the note (simulates what setup-testnet does)
    let note_bytes = note.to_bytes();
    eprintln!("[{label}] step 5: note serialized ({} bytes)", note_bytes.len());

    // Submit the note on-chain from wallet A
    let create_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![note.clone()])
        .build()
        .map_err(|e| anyhow::anyhow!("create note tx: {e:?}"))?;
    let create_tx = client_a
        .submit_new_transaction(wallet_a_id, create_req)
        .await?;
    eprintln!("[{label}] step 5: note submitted on-chain: {create_tx}");

    // Wait for the note to land on-chain
    tokio::time::sleep(Duration::from_secs(6)).await;
    client_a.sync_state().await?;

    // ═══════════════════════════════════════════════════════════════
    // CLIENT B: deserialize note and consume (facilitator pattern)
    // ═══════════════════════════════════════════════════════════════

    // Deserialize the note from bytes
    let deserialized_note = Note::read_from_bytes(&note_bytes)
        .map_err(|e| anyhow::anyhow!("note decode: {e}"))?;
    assert_eq!(deserialized_note.id(), note_id);
    eprintln!("[{label}] step 6: note deserialized from bytes");

    // Import the note into client B
    let note_details = NoteDetails::new(
        deserialized_note.assets().clone(),
        deserialized_note.recipient().clone(),
    );
    client_b
        .import_notes(&[NoteFile::NoteDetails {
            details: note_details,
            after_block_num: 0u32.into(),
            tag: Some(tag),
        }])
        .await?;
    eprintln!("[{label}] step 6: note imported into client B");

    // Sync and wait for note to become consumable
    for attempt in 0..60 {
        client_b.sync_state().await?;
        let consumable = client_b.get_consumable_notes(Some(facilitator_id)).await?;
        if consumable.iter().any(|(n, _)| n.id() == note_id) {
            eprintln!("[{label}] step 6: note is consumable (attempt {attempt})");
            break;
        }
        if attempt == 59 {
            eprintln!("[{label}] WARNING: note never became consumable, trying anyway...");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    // Final sync before consume
    client_b.sync_state().await?;

    // Build note_args and Falcon signature
    let note_args: Word = [
        merchant_id.suffix(),
        merchant_id.prefix().as_felt(),
        Felt::new(debit_amount),
        Felt::ZERO,
    ]
    .into();
    let message: Word = Hasher::merge(&[serial_num.into(), note_args.into()]).into();
    let sig = adn_agent_sk.sign(message);
    let prepared: Vec<Felt> = sig.to_prepared_signature(message);
    let sig_key: Word = Hasher::merge(&[adn_agent_pk.into(), message.into()]).into();

    eprintln!("[{label}] step 7: consuming from client B (cross-client)...");
    eprintln!("[{label}]   sig_key={sig_key:?}");
    eprintln!("[{label}]   prepared_len={}", prepared.len());

    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(deserialized_note, Some(note_args))])
        .extend_advice_map([(sig_key, prepared.as_slice())])
        .build()
        .map_err(|e| anyhow::anyhow!("consume tx: {e:?}"))?;

    match client_b
        .submit_new_transaction(facilitator_id, consume_req)
        .await
    {
        Ok(tx_id) => {
            eprintln!("[{label}] SUCCESS! tx_id={tx_id}");
            Ok(true)
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!("[{label}] FAILED: {}", &err_str[..err_str.len().min(500)]);
            if err_str.contains("StackReadFailed") {
                eprintln!("[{label}] Confirmed: StackReadFailed error!");
            }
            Ok(false)
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// TEST: Simplified ADN (no remainder note) cross-client
// ═══════════════════════════════════════════════════════════════════════════

/// Tests the simplified ADN (section 7 removed) cross-client on testnet.
///
/// If this PASSES but real_adn_cross_client FAILS, then the remainder note
/// creation (build_recipient + output_note::create + second move_asset_to_note)
/// is the root cause of the StackReadFailed error.
#[tokio::test]
#[ignore = "requires network access to Miden testnet"]
async fn adn_no_remainder_cross_client() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let debit_amount = 100u64;
    let note_balance = 1000u64;

    let result =
        run_adn_cross_client_test("no-remainder", ADN_NO_REMAINDER_MASM, debit_amount, note_balance)
            .await?;

    eprintln!("\n========================================");
    if result {
        eprintln!("RESULT: ADN without remainder note PASSES cross-client.");
        eprintln!("=> The remainder note creation (section 7) is likely the culprit.");
    } else {
        eprintln!("RESULT: ADN without remainder note STILL FAILS cross-client.");
        eprintln!("=> The issue is NOT in section 7. Look at Falcon sig / P2ID / get_storage.");
    }
    eprintln!("========================================");

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// TEST: Full ADN (with remainder) cross-client — control group
// ═══════════════════════════════════════════════════════════════════════════

/// Control test: runs the full ADN MASM cross-client.
/// This should reproduce the StackReadFailed for comparison.
#[tokio::test]
#[ignore = "requires network access to Miden testnet"]
async fn adn_full_cross_client_control() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let debit_amount = 100u64;
    let note_balance = 1000u64;

    let result =
        run_adn_cross_client_test("full-adn", ADN_FULL_MASM, debit_amount, note_balance).await?;

    eprintln!("\n========================================");
    if result {
        eprintln!("RESULT: Full ADN PASSES cross-client (unexpected if bug exists).");
    } else {
        eprintln!("RESULT: Full ADN FAILS cross-client (expected — this is the bug).");
    }
    eprintln!("========================================");

    Ok(())
}
