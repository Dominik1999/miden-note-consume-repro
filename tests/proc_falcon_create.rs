//! Process 1: Create a Falcon-verified note and save artifacts to /tmp/proc_falcon/.
//!
//! Deploys faucet + wallet A, creates wallet B (not registered in client),
//! mints + funds wallet A, then builds a falcon_p2id_note.masm custom note.
//!
//! Artifacts saved:
//!   - note.mno          — NoteFile serialized (NoteWithProof for public, NoteDetails for private)
//!   - wallet_b.b64       — wallet B account bytes (base64)
//!   - keystore/*         — filesystem keystore files
//!   - setup.toml         — wallet_b_id_hex, faucet_id_hex
//!   - agent_sk.bin       — AuthSecretKey serialized bytes (for consume process to sign)
//!
//! Set PRIVATE=1 to use NoteType::Private, otherwise NoteType::Public.
//!
//! Run with:
//!   RUST_LOG=info cargo test --test proc_falcon_create -- --ignored --nocapture

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
use miden_client::store::NoteExportType;
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::{Client, Felt};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::account::AccountId;
use miden_protocol::asset::Asset;
use miden_protocol::note::{
    Note, NoteAssets, NoteMetadata, NoteRecipient, NoteStorage, NoteTag,
};
use miden_protocol::utils::serde::Serializable;
use miden_standards::code_builder::CodeBuilder;
use rand::RngCore;

const FALCON_P2ID_MASM: &str = include_str!("../masm/falcon_p2id_note.masm");
const OUTPUT_DIR: &str = "/tmp/proc_falcon";

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

fn rand_seed(client: &mut Client<FilesystemKeyStore>) -> [u8; 32] {
    let mut seed = [0u8; 32];
    client.rng().fill_bytes(&mut seed);
    seed
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

#[tokio::test]
#[ignore = "requires network access to Miden testnet"]
async fn proc_falcon_create() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let is_private = std::env::var("PRIVATE").map(|v| v == "1").unwrap_or(false);
    let note_type = if is_private {
        NoteType::Private
    } else {
        NoteType::Public
    };
    eprintln!("[create] note_type={note_type:?} (PRIVATE={})", if is_private { "1" } else { "0" });

    // Clean + create output dir
    let output_dir = std::path::Path::new(OUTPUT_DIR);
    if output_dir.exists() {
        std::fs::remove_dir_all(output_dir)?;
    }
    std::fs::create_dir_all(output_dir)?;
    std::fs::create_dir_all(output_dir.join("keystore"))?;

    // ── Client setup ──
    let tmp = tempfile::tempdir()?;
    let (mut client, keystore) = build_client(tmp.path()).await?;
    client.sync_state().await?;
    eprintln!("[create] step 0: client synced");

    // Deploy faucet
    let faucet_id = deploy_faucet(&mut client, &keystore, "FALK").await?;
    eprintln!("[create] step 1: faucet deployed: {}", faucet_id.to_hex());
    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Deploy wallet A (sender)
    let wallet_a_id = deploy_wallet(&mut client, &keystore).await?;
    eprintln!("[create] step 2: wallet A deployed: {}", wallet_a_id.to_hex());
    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Create wallet B (receiver) — NOT registered in this client
    let wallet_b_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let wallet_b_account = AccountBuilder::new(rand_seed(&mut client))
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            wallet_b_key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(BasicWallet)
        .build()
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let wallet_b_id = wallet_b_account.id();
    // Save wallet B key to the keystore dir we'll export
    keystore
        .add_key(&wallet_b_key, wallet_b_id)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    eprintln!(
        "[create] step 3: wallet B created (NOT in client): {}",
        wallet_b_id.to_hex()
    );
    client.sync_state().await?;

    // Mint tokens to wallet A and consume
    eprintln!("[create] step 4: minting tokens to wallet A...");
    mint_and_consume(&mut client, faucet_id, wallet_a_id, 10_000).await?;
    eprintln!("[create] step 4: wallet A funded");
    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ── Generate agent keypair ──
    let agent_sk = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let agent_pk: miden_protocol::Word = agent_sk.public_key().to_commitment().into();

    // ── Compile falcon_p2id_note.masm ──
    let note_script = CodeBuilder::default().compile_note_script(FALCON_P2ID_MASM)?;
    eprintln!("[create] step 5: note script compiled");

    // ── Build the note ──
    let amount = 500u64;
    let asset =
        FungibleAsset::new(faucet_id, amount).map_err(|e| anyhow::anyhow!("asset: {e:?}"))?;

    let storage = NoteStorage::new(vec![
        agent_pk[0],
        agent_pk[1],
        agent_pk[2],
        agent_pk[3],
        wallet_b_id.suffix(),
        wallet_b_id.prefix().as_felt(),
    ])?;

    let mut serial_bytes = [0u8; 32];
    client.rng().fill_bytes(&mut serial_bytes);
    let serial_num: miden_protocol::Word = [
        Felt::new(u64::from_le_bytes(serial_bytes[0..8].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[8..16].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[16..24].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[24..32].try_into().unwrap())),
    ]
    .into();

    let tag = NoteTag::with_account_target(wallet_b_id);
    let metadata = NoteMetadata::new(wallet_a_id, note_type).with_tag(tag);
    let vault = NoteAssets::new(vec![Asset::Fungible(asset)])?;
    let recipient = NoteRecipient::new(serial_num, note_script, storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();
    eprintln!("[create] step 6: note built, id={note_id}, type={note_type:?}");

    // Submit note on-chain via own_output_notes
    let create_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![note.clone()])
        .build()
        .map_err(|e| anyhow::anyhow!("own_output_notes build: {e:?}"))?;
    let create_tx = client
        .submit_new_transaction(wallet_a_id, create_req)
        .await?;
    eprintln!("[create] step 7: note submitted on-chain: {create_tx}");

    // Wait for on-chain confirmation + sync to get inclusion proof
    eprintln!("[create] step 8: waiting for on-chain confirmation...");
    tokio::time::sleep(Duration::from_secs(10)).await;
    client.sync_state().await?;

    // For public notes, wait for inclusion proof; for private, we export as NoteDetails
    let output_notes = client
        .get_output_notes(miden_client::store::NoteFilter::All)
        .await?;
    eprintln!("[create] found {} output notes", output_notes.len());

    // Find our note
    let note_record = output_notes
        .into_iter()
        .find(|n| n.id() == note_id)
        .ok_or_else(|| anyhow::anyhow!("our note {note_id} not found in output notes"))?;
    eprintln!("[create] found our note record, id={}", note_record.id());

    // Export as NoteFile
    let export_type = if is_private {
        NoteExportType::NoteDetails
    } else {
        // For public notes, try NoteWithProof (needs inclusion proof from sync)
        // If it fails, retry with more syncs
        NoteExportType::NoteWithProof
    };

    let note_file = if is_private {
        note_record.into_note_file(&export_type)
            .map_err(|e| anyhow::anyhow!("into_note_file: {e:?}"))?
    } else {
        // For public, may need to wait for inclusion proof
        let mut result = None;
        let output_notes_retry = client
            .get_output_notes(miden_client::store::NoteFilter::All)
            .await?;
        let record = output_notes_retry
            .into_iter()
            .find(|n| n.id() == note_id)
            .ok_or_else(|| anyhow::anyhow!("note not found on retry"))?;
        match record.into_note_file(&NoteExportType::NoteWithProof) {
            Ok(nf) => {
                result = Some(nf);
            }
            Err(e) => {
                eprintln!("[create] NoteWithProof failed ({e:?}), retrying with more syncs...");
                for attempt in 0..30 {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    client.sync_state().await?;
                    let notes = client
                        .get_output_notes(miden_client::store::NoteFilter::All)
                        .await?;
                    if let Some(rec) = notes.into_iter().find(|n| n.id() == note_id) {
                        if let Ok(nf) = rec.into_note_file(&NoteExportType::NoteWithProof) {
                            eprintln!("[create] NoteWithProof succeeded on attempt {attempt}");
                            result = Some(nf);
                            break;
                        }
                    }
                }
            }
        }
        result.ok_or_else(|| anyhow::anyhow!("failed to get NoteWithProof after retries"))?
    };

    let note_file_bytes = note_file.to_bytes();
    std::fs::write(output_dir.join("note.mno"), &note_file_bytes)?;
    eprintln!(
        "[create] note.mno written ({} bytes)",
        note_file_bytes.len()
    );

    // Save wallet B account (base64)
    let wallet_b_bytes = wallet_b_account.to_bytes();
    let wallet_b_b64 =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &wallet_b_bytes);
    std::fs::write(output_dir.join("wallet_b.b64"), &wallet_b_b64)?;
    eprintln!("[create] wallet_b.b64 written");

    // Copy keystore files
    let src_keystore = tmp.path().join("keystore");
    let dst_keystore = output_dir.join("keystore");
    for entry in std::fs::read_dir(&src_keystore)? {
        let entry = entry?;
        std::fs::copy(entry.path(), dst_keystore.join(entry.file_name()))?;
        eprintln!("[create] keystore: copied {:?}", entry.file_name());
    }

    // Save setup.toml
    let setup_toml = format!(
        "wallet_b_id_hex = \"{}\"\nfaucet_id_hex = \"{}\"\nnote_type = \"{}\"\n",
        wallet_b_id.to_hex(),
        faucet_id.to_hex(),
        if is_private { "private" } else { "public" },
    );
    std::fs::write(output_dir.join("setup.toml"), &setup_toml)?;
    eprintln!("[create] setup.toml written");

    // Save agent_sk (AuthSecretKey serialized)
    let agent_sk_bytes = agent_sk.to_bytes();
    std::fs::write(output_dir.join("agent_sk.bin"), &agent_sk_bytes)?;
    eprintln!(
        "[create] agent_sk.bin written ({} bytes)",
        agent_sk_bytes.len()
    );

    eprintln!("[create] ══════════════════════════════════════════════════════");
    eprintln!("[create] DONE. Artifacts saved to {OUTPUT_DIR}");
    eprintln!("[create] Note ID: {note_id}");
    eprintln!("[create] Wallet B ID: {}", wallet_b_id.to_hex());
    eprintln!("[create] Note type: {note_type:?}");
    eprintln!("[create] Now run proc_falcon_consume to consume from a separate process.");
    eprintln!("[create] ══════════════════════════════════════════════════════");

    Ok(())
}
