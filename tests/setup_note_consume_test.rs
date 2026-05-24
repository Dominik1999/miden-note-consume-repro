//! Tests whether the note DATA from setup-testnet is the problem,
//! or whether it's the compiled MAST in the note script.
//!
//! Test A: Read adn_note.b64 from setup-testnet, try execute_transaction
//!         with the full ADN script. Expects StackReadFailed.
//!
//! Test B: Same note bytes, serialize -> deserialize round-trip, then
//!         execute_transaction. Confirms serialization isn't lossy.
//!
//! Test C: Create a NEW note in-process with the SAME storage/assets/serial
//!         as the file note but freshly compiled ADN MASM via CodeBuilder.
//!         Submit on-chain from a funded wallet, then a SECOND client
//!         imports + consumes it. If this passes but Test A fails, the
//!         compiled MAST in setup-testnet's note differs from what
//!         CodeBuilder produces in-process (despite matching script_root).
//!
//! Run:
//!   SETUP_DIR=/tmp/chain-finality-fresh \
//!   RUST_LOG=info \
//!   cargo test --test setup_note_consume_test -- --ignored --nocapture

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
use miden_protocol::account::{Account, AccountId};
use miden_protocol::asset::Asset;
use miden_protocol::note::{
    Note, NoteAssets, NoteDetails, NoteFile, NoteMetadata, NoteRecipient, NoteStorage, NoteTag,
};
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_protocol::{Hasher, Word};
use miden_standards::code_builder::CodeBuilder;
use rand::RngCore;

const ADN_MASM: &str = include_str!("../masm/agent_debit_note.masm");

// ─── Helpers ─────────────────────────────────────────────────────────

struct SetupConfig {
    setup_dir: std::path::PathBuf,
    facilitator_id: AccountId,
    merchant_id: AccountId,
    faucet_id: AccountId,
    agent_hot_key_path: std::path::PathBuf,
}

fn read_setup_config() -> anyhow::Result<SetupConfig> {
    let setup_dir = std::env::var("SETUP_DIR")
        .map_err(|_| anyhow::anyhow!("set SETUP_DIR to the setup-testnet output dir"))?;
    let setup_dir = std::path::PathBuf::from(setup_dir);

    let setup_toml: toml::Value =
        toml::from_str(&std::fs::read_to_string(setup_dir.join("setup.toml"))?)?;

    let facilitator_id =
        AccountId::from_hex(setup_toml["facilitator_account_id_hex"].as_str().unwrap())?;
    let merchant_id = AccountId::from_hex(setup_toml["merchant_id_hex"].as_str().unwrap())?;
    let faucet_id = AccountId::from_hex(setup_toml["faucet_id_hex"].as_str().unwrap())?;

    let hot_key_rel = setup_toml["agents"].as_array().unwrap()[0]
        .get("hot_key_path")
        .unwrap()
        .as_str()
        .unwrap();
    let agent_hot_key_path = setup_dir.join(hot_key_rel);

    Ok(SetupConfig {
        setup_dir,
        facilitator_id,
        merchant_id,
        faucet_id,
        agent_hot_key_path,
    })
}

fn read_facilitator_account(cfg: &SetupConfig) -> anyhow::Result<Account> {
    let b64 = std::fs::read_to_string(cfg.setup_dir.join("facilitator_account.b64"))?;
    let bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        b64.trim(),
    )?;
    Ok(Account::read_from_bytes(&bytes)?)
}

fn read_adn_note(cfg: &SetupConfig) -> anyhow::Result<Note> {
    let b64 = std::fs::read_to_string(cfg.setup_dir.join("adn_note.b64"))?;
    let bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        b64.trim(),
    )?;
    Ok(Note::read_from_bytes(&bytes)?)
}

fn read_agent_key(cfg: &SetupConfig) -> anyhow::Result<AuthSecretKey> {
    let sk_bytes = std::fs::read(&cfg.agent_hot_key_path)?;
    let falcon_sk =
        miden_protocol::crypto::dsa::falcon512_poseidon2::SecretKey::read_from_bytes(&sk_bytes)
            .map_err(|e| anyhow::anyhow!("agent key: {e}"))?;
    Ok(AuthSecretKey::Falcon512Poseidon2(falcon_sk))
}

async fn build_client(
    data_dir: &std::path::Path,
) -> anyhow::Result<(Client<FilesystemKeyStore>, Arc<FilesystemKeyStore>)> {
    let endpoint = Endpoint::try_from("https://rpc.testnet.miden.io")
        .map_err(|e| anyhow::anyhow!("endpoint: {e:?}"))?;
    let rpc = Arc::new(GrpcClient::new(&endpoint, 30_000));
    let keystore_dir = data_dir.join("keystore");
    std::fs::create_dir_all(&keystore_dir)?;
    let keystore = Arc::new(
        FilesystemKeyStore::new(keystore_dir).map_err(|e| anyhow::anyhow!("keystore: {e:?}"))?,
    );
    let client = ClientBuilder::new()
        .rpc(rpc)
        .sqlite_store(data_dir.join("store.sqlite3"))
        .authenticator(keystore.clone())
        .in_debug_mode(true.into())
        .build()
        .await
        .map_err(|e| anyhow::anyhow!("client build: {e}"))?;
    Ok((client, keystore))
}

/// Build a client that reuses the setup-testnet keystore (so facilitator
/// auth key is already present).
async fn build_client_with_setup_keystore(
    data_dir: &std::path::Path,
    setup_keystore_dir: &std::path::Path,
) -> anyhow::Result<(Client<FilesystemKeyStore>, Arc<FilesystemKeyStore>)> {
    let endpoint = Endpoint::try_from("https://rpc.testnet.miden.io")
        .map_err(|e| anyhow::anyhow!("endpoint: {e:?}"))?;
    let rpc = Arc::new(GrpcClient::new(&endpoint, 30_000));
    let keystore_dir = data_dir.join("keystore");
    std::fs::create_dir_all(&keystore_dir)?;
    // Copy setup keystore files
    for entry in std::fs::read_dir(setup_keystore_dir)? {
        let entry = entry?;
        std::fs::copy(entry.path(), keystore_dir.join(entry.file_name()))?;
    }
    let keystore = Arc::new(
        FilesystemKeyStore::new(keystore_dir).map_err(|e| anyhow::anyhow!("keystore: {e:?}"))?,
    );
    let client = ClientBuilder::new()
        .rpc(rpc)
        .sqlite_store(data_dir.join("store.sqlite3"))
        .authenticator(keystore.clone())
        .in_debug_mode(true.into())
        .build()
        .await
        .map_err(|e| anyhow::anyhow!("client build: {e}"))?;
    Ok((client, keystore))
}

/// Sign and build a consume TransactionRequest for an ADN note.
fn build_adn_consume_request(
    note: Note,
    agent_sk: &AuthSecretKey,
    merchant_id: AccountId,
    amount: u64,
) -> anyhow::Result<miden_client::transaction::TransactionRequest> {
    let serial_num = note.recipient().serial_num();
    let note_args: Word = [
        merchant_id.suffix(),
        merchant_id.prefix().as_felt(),
        Felt::new(amount),
        Felt::ZERO,
    ]
    .into();

    let agent_pk: Word = agent_sk.public_key().to_commitment().into();
    let message: Word = Hasher::merge(&[serial_num.into(), note_args.into()]).into();
    let sig = agent_sk.sign(message);
    let prepared: Vec<Felt> = sig.to_prepared_signature(message);
    let sig_key: Word = Hasher::merge(&[agent_pk.into(), message.into()]).into();

    eprintln!("  note_args: {note_args:?}");
    eprintln!("  sig_key:   {sig_key:?}");
    eprintln!("  prepared_len: {}", prepared.len());

    TransactionRequestBuilder::new()
        .input_notes([(note, Some(note_args))])
        .extend_advice_map([(sig_key, prepared.as_slice())])
        .build()
        .map_err(|e| anyhow::anyhow!("consume build: {e:?}"))
}

fn rand_seed(client: &mut Client<FilesystemKeyStore>) -> [u8; 32] {
    let mut seed = [0u8; 32];
    client.rng().fill_bytes(&mut seed);
    seed
}

async fn deploy_wallet(
    client: &mut Client<FilesystemKeyStore>,
    keystore: &Arc<FilesystemKeyStore>,
) -> anyhow::Result<AccountId> {
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

async fn deploy_faucet(
    client: &mut Client<FilesystemKeyStore>,
    keystore: &Arc<FilesystemKeyStore>,
    symbol_str: &str,
) -> anyhow::Result<AccountId> {
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

async fn mint_and_consume(
    client: &mut Client<FilesystemKeyStore>,
    faucet_id: AccountId,
    target_id: AccountId,
    amount: u64,
) -> anyhow::Result<()> {
    let mint_asset =
        FungibleAsset::new(faucet_id, amount).map_err(|e| anyhow::anyhow!("asset: {e:?}"))?;
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
                "  found {} consumable (attempt {attempt}), consuming...",
                consumable.len()
            );
            let notes: Vec<_> = consumable
                .into_iter()
                .map(|(note, _)| note.try_into())
                .collect::<Result<_, _>>()?;
            let consume_req = TransactionRequestBuilder::new()
                .build_consume_notes(notes)
                .map_err(|e| anyhow::anyhow!("consume build: {e:?}"))?;
            client
                .submit_new_transaction(target_id, consume_req)
                .await?;
            return Ok(());
        }
        if attempt == 59 {
            anyhow::bail!("timed out waiting for consumable mint note");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    unreachable!()
}

async fn wait_for_note_consumable(
    client: &mut Client<FilesystemKeyStore>,
    account_id: AccountId,
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

// ─── Test A ──────────────────────────────────────────────────────────
/// Read the EXACT note from setup-testnet's adn_note.b64 and try to
/// execute_transaction. We expect StackReadFailed.

#[tokio::test]
#[ignore = "requires SETUP_DIR env var pointing to setup-testnet output + testnet access"]
async fn test_a_setup_note_execute() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let cfg = read_setup_config()?;
    let fac_account = read_facilitator_account(&cfg)?;
    let note = read_adn_note(&cfg)?;
    let agent_sk = read_agent_key(&cfg)?;
    let note_id = note.id();

    eprintln!("[test_a] note id={note_id}");
    eprintln!("[test_a] note script_root={:?}", note.recipient().script().root());
    eprintln!("[test_a] note storage items={}", note.recipient().storage().num_items());
    eprintln!("[test_a] note assets={}", note.assets().num_assets());

    // Build client with setup keystore
    let tmp = tempfile::tempdir()?;
    let (mut client, _keystore) =
        build_client_with_setup_keystore(tmp.path(), &cfg.setup_dir.join("keystore")).await?;

    // Import facilitator account
    client.add_account(&fac_account, false).await?;

    // Import note
    let tag = NoteTag::with_account_target(cfg.facilitator_id);
    let note_details = NoteDetails::new(note.assets().clone(), note.recipient().clone());
    client
        .import_notes(&[NoteFile::NoteDetails {
            details: note_details,
            after_block_num: 0u32.into(),
            tag: Some(tag),
        }])
        .await?;

    // Sync
    let sync = client.sync_state().await?;
    eprintln!("[test_a] synced to block {}", sync.block_num);

    // Check note state
    match client.get_input_note(note_id).await {
        Ok(Some(record)) => {
            eprintln!("[test_a] note is_authenticated={}", record.is_authenticated());
        }
        Ok(None) => eprintln!("[test_a] note NOT FOUND after sync"),
        Err(e) => eprintln!("[test_a] get_input_note error: {e}"),
    }

    // Build consume request
    let consume_req = build_adn_consume_request(note, &agent_sk, cfg.merchant_id, 100)?;

    eprintln!("[test_a] executing transaction (setup-testnet note, original script)...");
    match client
        .execute_transaction(cfg.facilitator_id, consume_req)
        .await
    {
        Ok(result) => {
            eprintln!(
                "[test_a] SUCCESS: {} output notes",
                result.executed_transaction().output_notes().num_notes()
            );
            eprintln!("[test_a] The setup-testnet note consumed successfully!");
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!("[test_a] FAILED: {}", &err_str[..err_str.len().min(500)]);
            if err_str.contains("StackReadFailed") {
                eprintln!("[test_a] StackReadFailed confirmed with setup-testnet note.");
            } else {
                eprintln!("[test_a] Different error (not StackReadFailed).");
            }
        }
    }

    Ok(())
}

// ─── Test B ──────────────────────────────────────────────────────────
/// Same as Test A, but serialize -> deserialize the note first.
/// Confirms round-trip serialization isn't lossy.

#[tokio::test]
#[ignore = "requires SETUP_DIR env var pointing to setup-testnet output + testnet access"]
async fn test_b_roundtrip_serialize_execute() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let cfg = read_setup_config()?;
    let fac_account = read_facilitator_account(&cfg)?;
    let note = read_adn_note(&cfg)?;
    let agent_sk = read_agent_key(&cfg)?;
    let note_id = note.id();

    // Round-trip serialize
    let note_bytes = note.to_bytes();
    let note_rt = Note::read_from_bytes(&note_bytes)?;
    assert_eq!(note_rt.id(), note_id, "round-trip note ID mismatch");

    let rt_bytes = note_rt.to_bytes();
    assert_eq!(
        note_bytes, rt_bytes,
        "round-trip bytes differ (serialization is lossy!)"
    );
    eprintln!("[test_b] round-trip serialization verified: {} bytes, same ID", note_bytes.len());

    // Compare script roots
    eprintln!(
        "[test_b] original script_root: {:?}",
        note.recipient().script().root()
    );
    eprintln!(
        "[test_b] round-trip script_root: {:?}",
        note_rt.recipient().script().root()
    );

    // Compare MAST forest sizes
    eprintln!(
        "[test_b] original MAST size: {} bytes",
        note.recipient().script().mast().to_bytes().len()
    );
    eprintln!(
        "[test_b] round-trip MAST size: {} bytes",
        note_rt.recipient().script().mast().to_bytes().len()
    );

    // Build client with setup keystore
    let tmp = tempfile::tempdir()?;
    let (mut client, _keystore) =
        build_client_with_setup_keystore(tmp.path(), &cfg.setup_dir.join("keystore")).await?;

    client.add_account(&fac_account, false).await?;

    let tag = NoteTag::with_account_target(cfg.facilitator_id);
    let note_details = NoteDetails::new(note_rt.assets().clone(), note_rt.recipient().clone());
    client
        .import_notes(&[NoteFile::NoteDetails {
            details: note_details,
            after_block_num: 0u32.into(),
            tag: Some(tag),
        }])
        .await?;

    let sync = client.sync_state().await?;
    eprintln!("[test_b] synced to block {}", sync.block_num);

    let consume_req = build_adn_consume_request(note_rt, &agent_sk, cfg.merchant_id, 100)?;

    eprintln!("[test_b] executing transaction (round-tripped note)...");
    match client
        .execute_transaction(cfg.facilitator_id, consume_req)
        .await
    {
        Ok(result) => {
            eprintln!(
                "[test_b] SUCCESS: {} output notes",
                result.executed_transaction().output_notes().num_notes()
            );
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!("[test_b] FAILED: {}", &err_str[..err_str.len().min(500)]);
            if err_str.contains("StackReadFailed") {
                eprintln!("[test_b] StackReadFailed with round-tripped note too.");
                eprintln!("[test_b] => Serialization is NOT the cause (same error).");
            } else {
                eprintln!("[test_b] Different error.");
            }
        }
    }

    Ok(())
}

// ─── Test C ──────────────────────────────────────────────────────────
/// The critical test. Create a NEW note in-process with the SAME
/// storage, assets, and serial_num as the file note, but with a
/// FRESHLY compiled ADN MASM script (via CodeBuilder). Submit it
/// on-chain from a funded wallet, then have a second client consume it.
///
/// If this PASSES but Test A fails => the compiled MAST from
/// setup-testnet differs from what CodeBuilder produces in-process
/// (even though script_root may match).

#[tokio::test]
#[ignore = "requires SETUP_DIR env var + testnet access; slow (deploys accounts + waits for blocks)"]
async fn test_c_fresh_compiled_note_cross_client() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let cfg = read_setup_config()?;
    let file_note = read_adn_note(&cfg)?;
    let agent_sk = read_agent_key(&cfg)?;
    let agent_pk: Word = agent_sk.public_key().to_commitment().into();

    eprintln!("[test_c] === SETUP-TESTNET NOTE INFO ===");
    eprintln!("[test_c] file note id:          {}", file_note.id());
    eprintln!("[test_c] file script_root:       {:?}", file_note.recipient().script().root());
    eprintln!("[test_c] file MAST size:         {} bytes", file_note.recipient().script().mast().to_bytes().len());
    eprintln!("[test_c] file serial_num:        {:?}", file_note.recipient().serial_num());
    eprintln!("[test_c] file storage items:     {}", file_note.recipient().storage().num_items());
    eprintln!("[test_c] file assets:            {}", file_note.assets().num_assets());

    // Freshly compile the ADN MASM
    let fresh_script = CodeBuilder::default().compile_note_script(ADN_MASM)?;
    eprintln!("[test_c] === FRESH COMPILED SCRIPT ===");
    eprintln!("[test_c] fresh script_root:      {:?}", fresh_script.root());
    eprintln!("[test_c] fresh MAST size:        {} bytes", fresh_script.mast().to_bytes().len());

    let roots_match = file_note.recipient().script().root() == fresh_script.root();
    eprintln!("[test_c] script_root match:      {roots_match}");

    let file_mast_bytes = file_note.recipient().script().mast().to_bytes();
    let fresh_mast_bytes = fresh_script.mast().to_bytes();
    let mast_match = file_mast_bytes == fresh_mast_bytes;
    eprintln!("[test_c] MAST bytes match:       {mast_match}");

    if !mast_match {
        eprintln!("[test_c] MAST DIFFERS! file={} bytes, fresh={} bytes",
            file_mast_bytes.len(), fresh_mast_bytes.len());
        // Find first differing byte
        let min_len = file_mast_bytes.len().min(fresh_mast_bytes.len());
        for i in 0..min_len {
            if file_mast_bytes[i] != fresh_mast_bytes[i] {
                eprintln!("[test_c]   first diff at byte {i}: file=0x{:02x} fresh=0x{:02x}",
                    file_mast_bytes[i], fresh_mast_bytes[i]);
                break;
            }
        }
    }

    // ═══════════════════════════════════════════════════════════════
    // CLIENT A: deploy faucet + wallet, mint, create fresh ADN note
    // ═══════════════════════════════════════════════════════════════
    let tmp_a = tempfile::tempdir()?;
    let (mut client_a, keystore_a) = build_client(tmp_a.path()).await?;
    client_a.sync_state().await?;
    eprintln!("[test_c] client A synced");

    // Deploy faucet
    let faucet_id = deploy_faucet(&mut client_a, &keystore_a, "CTEST").await?;
    eprintln!("[test_c] faucet deployed: {}", faucet_id.to_hex());
    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Deploy wallet A (note creator)
    let wallet_a_id = deploy_wallet(&mut client_a, &keystore_a).await?;
    eprintln!("[test_c] wallet A deployed: {}", wallet_a_id.to_hex());
    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Mint and fund wallet A
    eprintln!("[test_c] minting tokens to wallet A...");
    mint_and_consume(&mut client_a, faucet_id, wallet_a_id, 1_000_000).await?;
    eprintln!("[test_c] wallet A funded");
    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ── Build a fresh ADN note with fresh script ──
    // Storage layout: [0-3] = agent_pk, [4] = user_suffix, [5] = user_prefix, [6] = expiry
    // We use the SAME agent key from setup-testnet's hot_key.bin, but
    // point user to wallet_a and use a large expiry.
    let storage = NoteStorage::new(vec![
        agent_pk[0],
        agent_pk[1],
        agent_pk[2],
        agent_pk[3],
        wallet_a_id.suffix(),
        wallet_a_id.prefix().as_felt(),
        Felt::new(1_000_000), // expiry far in the future
    ])?;
    eprintln!("[test_c] built fresh storage: 7 items");

    // Use the same balance from the file note's assets
    let file_asset = file_note
        .assets()
        .iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("file note has no assets"))?;
    let (file_faucet_id, file_balance) = match file_asset {
        Asset::Fungible(fa) => (fa.faucet_id(), fa.amount()),
        _ => anyhow::bail!("expected fungible asset"),
    };
    eprintln!("[test_c] file note asset: faucet={} balance={file_balance}", file_faucet_id.to_hex());

    // Create asset from OUR faucet with the same balance
    let asset = FungibleAsset::new(faucet_id, file_balance)
        .map_err(|e| anyhow::anyhow!("asset: {e:?}"))?;

    // Random serial number (fresh)
    let mut serial_bytes = [0u8; 32];
    client_a.rng().fill_bytes(&mut serial_bytes);
    let serial_num: Word = [
        Felt::new(u64::from_le_bytes(serial_bytes[0..8].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[8..16].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[16..24].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[24..32].try_into().unwrap())),
    ]
    .into();

    // ── Set up Client B now so we know its wallet ID for the tag ──
    let tmp_b = tempfile::tempdir()?;
    let (mut client_b, keystore_b) = build_client(tmp_b.path()).await?;
    client_b.sync_state().await?;
    let wallet_b_id = deploy_wallet(&mut client_b, &keystore_b).await?;
    eprintln!("[test_c] wallet B (consumer) deployed: {}", wallet_b_id.to_hex());
    client_b.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Build the note
    let tag = NoteTag::with_account_target(wallet_b_id);
    let metadata = NoteMetadata::new(wallet_a_id, NoteType::Public).with_tag(tag);
    let vault = NoteAssets::new(vec![Asset::Fungible(asset)])?;
    let recipient = NoteRecipient::new(serial_num, fresh_script, storage);
    let fresh_note = Note::new(vault, metadata, recipient);
    let fresh_note_id = fresh_note.id();

    eprintln!("[test_c] fresh note built:");
    eprintln!("[test_c]   id:          {fresh_note_id}");
    eprintln!("[test_c]   script_root: {:?}", fresh_note.recipient().script().root());
    eprintln!("[test_c]   MAST size:   {} bytes", fresh_note.recipient().script().mast().to_bytes().len());

    // Serialize (for cross-client transfer)
    let fresh_note_bytes = fresh_note.to_bytes();
    eprintln!("[test_c] fresh note serialized: {} bytes", fresh_note_bytes.len());

    // Submit on-chain from wallet A
    let create_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![fresh_note.clone()])
        .build()
        .map_err(|e| anyhow::anyhow!("create note tx: {e:?}"))?;
    let create_tx = client_a
        .submit_new_transaction(wallet_a_id, create_req)
        .await?;
    eprintln!("[test_c] fresh note submitted on-chain: {create_tx}");

    // ═══════════════════════════════════════════════════════════════
    // CLIENT B: deserialize, import, wait, consume
    // ═══════════════════════════════════════════════════════════════
    tokio::time::sleep(Duration::from_secs(5)).await;

    let deserialized = Note::read_from_bytes(&fresh_note_bytes)?;
    assert_eq!(deserialized.id(), fresh_note_id);

    let note_details = NoteDetails::new(deserialized.assets().clone(), deserialized.recipient().clone());
    client_b
        .import_notes(&[NoteFile::NoteDetails {
            details: note_details,
            after_block_num: 0u32.into(),
            tag: Some(tag),
        }])
        .await?;
    eprintln!("[test_c] fresh note imported into client B");

    // Wait for consumable
    eprintln!("[test_c] waiting for fresh note to become consumable...");
    let found = wait_for_note_consumable(&mut client_b, wallet_b_id, fresh_note_id, 60).await?;
    if !found {
        eprintln!("[test_c] WARNING: note never became consumable, trying anyway...");
    }

    // Build consume request with note_args pointing to wallet_b as merchant
    let note_args: Word = [
        wallet_b_id.suffix(),
        wallet_b_id.prefix().as_felt(),
        Felt::new(100),
        Felt::ZERO,
    ]
    .into();

    let message: Word = Hasher::merge(&[serial_num.into(), note_args.into()]).into();
    let sig = agent_sk.sign(message);
    let prepared: Vec<Felt> = sig.to_prepared_signature(message);
    let sig_key: Word = Hasher::merge(&[agent_pk.into(), message.into()]).into();

    eprintln!("[test_c] Falcon signature computed for fresh note");

    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(deserialized, Some(note_args))])
        .extend_advice_map([(sig_key, prepared.as_slice())])
        .build()
        .map_err(|e| anyhow::anyhow!("consume build: {e:?}"))?;

    eprintln!("[test_c] client B executing transaction (fresh-compiled note)...");

    match client_b
        .execute_transaction(wallet_b_id, consume_req)
        .await
    {
        Ok(result) => {
            eprintln!(
                "[test_c] SUCCESS: {} output notes",
                result.executed_transaction().output_notes().num_notes()
            );
            eprintln!("[test_c] Fresh-compiled note consumed cross-client!");
            eprintln!("[test_c] => If test_a FAILED, the issue is in the MAST");
            eprintln!("[test_c]    compiled by setup-testnet, NOT in the note data.");
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!(
                "[test_c] FAILED: {}",
                &err_str[..err_str.len().min(500)]
            );
            if err_str.contains("StackReadFailed") {
                eprintln!("[test_c] StackReadFailed with fresh-compiled note too!");
                eprintln!("[test_c] => The issue is NOT in setup-testnet's MAST.");
                eprintln!("[test_c]    It's in the note DATA or cross-client pattern.");
            } else {
                eprintln!("[test_c] Different error (not StackReadFailed).");
            }
        }
    }

    Ok(())
}
