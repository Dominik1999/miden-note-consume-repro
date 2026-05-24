//! Process 1: Create a hasmapkey Falcon note on testnet and save artifacts to disk.
//!
//! This test deploys a faucet + wallet A + wallet B, mints tokens to wallet A,
//! then creates a custom hasmapkey note (falcon_p2id_hasmapkey_note.masm) from
//! wallet A targeting wallet B. Artifacts are serialized to `/tmp/proc_hasmapkey/`
//! so that proc_hasmapkey_consume can import and consume them in a separate process.
//!
//! NoteType is controlled by env var NOTE_PRIVATE: set to "1" for Private, default is Public.
//! Exports the note as NoteFile::NoteWithProof (Public) or NoteFile::NoteDetails (Private).
//! Also saves agent_sk.bin for the consume process.
//!
//! Run with:
//!   RUST_LOG=info cargo test --test proc_hasmapkey_create -- --ignored --nocapture
//!   NOTE_PRIVATE=1 RUST_LOG=info cargo test --test proc_hasmapkey_create -- --ignored --nocapture

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
use miden_protocol::account::AccountId;
use miden_protocol::asset::Asset;
use miden_protocol::note::{
    Note, NoteAssets, NoteDetails, NoteFile, NoteMetadata, NoteRecipient, NoteStorage, NoteTag,
};
use miden_protocol::utils::serde::Serializable;
use miden_protocol::Word;
use miden_standards::code_builder::CodeBuilder;
use rand::RngCore;

const HASMAPKEY_MASM: &str = include_str!("../masm/falcon_p2id_hasmapkey_note.masm");
const OUTPUT_DIR: &str = "/tmp/proc_hasmapkey";

fn is_private() -> bool {
    std::env::var("NOTE_PRIVATE").map(|v| v == "1").unwrap_or(false)
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
) -> anyhow::Result<(AccountId, miden_protocol::account::Account)> {
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
    Ok((wallet_id, wallet))
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
async fn hasmapkey_create_and_save() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let private = is_private();
    let note_type = if private {
        NoteType::Private
    } else {
        NoteType::Public
    };
    eprintln!(
        "[create] NoteType: {} (NOTE_PRIVATE={})",
        if private { "Private" } else { "Public" },
        if private { "1" } else { "0" }
    );

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
    let faucet_id = deploy_faucet(&mut client, &keystore, "HMPROC").await?;
    eprintln!(
        "[create] step 1: faucet deployed: {}",
        faucet_id.to_hex()
    );
    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Deploy wallet A (sender)
    let (wallet_a_id, _wallet_a) = deploy_wallet(&mut client, &keystore).await?;
    eprintln!(
        "[create] step 2: wallet A deployed: {}",
        wallet_a_id.to_hex()
    );
    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Create wallet B (receiver) -- NOT registered in this client
    let wallet_b_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let wallet_b_account = AccountBuilder::new({
        let mut s = [0u8; 32];
        client.rng().fill_bytes(&mut s);
        s
    })
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
    // Save wallet B key to keystore (for the consume process)
    keystore
        .add_key(&wallet_b_key, wallet_b_id)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    eprintln!(
        "[create] step 3: wallet B created (NOT registered): {}",
        wallet_b_id.to_hex()
    );
    client.sync_state().await?;

    // Mint tokens to wallet A and consume the mint note
    eprintln!("[create] step 4: minting tokens to wallet A...");
    mint_and_consume(&mut client, faucet_id, wallet_a_id, 10_000).await?;
    eprintln!("[create] step 4: wallet A funded");
    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ── Create the hasmapkey note ──
    let agent_sk = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let agent_pk: Word = agent_sk.public_key().to_commitment().into();

    let note_script = CodeBuilder::default().compile_note_script(HASMAPKEY_MASM)?;
    eprintln!("[create] step 5: hasmapkey note script compiled");

    let balance = 1000u64;
    let asset =
        FungibleAsset::new(faucet_id, balance).map_err(|e| anyhow::anyhow!("asset: {e:?}"))?;

    // Storage: 6 items = [agent_pk[0..4], target_suffix, target_prefix]
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
    let serial_num: Word = [
        Felt::new(u64::from_le_bytes(
            serial_bytes[0..8].try_into().unwrap(),
        )),
        Felt::new(u64::from_le_bytes(
            serial_bytes[8..16].try_into().unwrap(),
        )),
        Felt::new(u64::from_le_bytes(
            serial_bytes[16..24].try_into().unwrap(),
        )),
        Felt::new(u64::from_le_bytes(
            serial_bytes[24..32].try_into().unwrap(),
        )),
    ]
    .into();

    let tag = NoteTag::with_account_target(wallet_b_id);
    let metadata = NoteMetadata::new(wallet_a_id, note_type).with_tag(tag);
    let vault = NoteAssets::new(vec![Asset::Fungible(asset)])?;
    let recipient = NoteRecipient::new(serial_num, note_script, storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();
    eprintln!("[create] step 5: hasmapkey note built, id={note_id}");

    // Serialize raw note for the consume process (always needed for input_notes)
    let note_bytes = note.to_bytes();
    eprintln!(
        "[create] step 5: note serialized ({} bytes)",
        note_bytes.len()
    );

    // Submit note on-chain via own_output_notes
    let create_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![note.clone()])
        .build()
        .map_err(|e| anyhow::anyhow!("own_output_notes build: {e:?}"))?;
    let create_tx = client
        .submit_new_transaction(wallet_a_id, create_req)
        .await?;
    eprintln!("[create] step 6: note submitted on-chain: {create_tx}");

    // Wait for on-chain confirmation
    eprintln!("[create] step 7: waiting for on-chain confirmation...");
    tokio::time::sleep(Duration::from_secs(5)).await;
    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    client.sync_state().await?;
    eprintln!("[create] step 7: client synced after submission");

    // ── Export note as NoteFile ──
    // For PUBLIC notes: export as NoteFile::NoteWithProof (the inclusion proof
    // is available after the note is committed on-chain).
    // For PRIVATE notes: export as NoteFile::NoteDetails (no on-chain proof).
    if !private {
        // For public notes, we try to get the inclusion proof by importing into
        // the same client and syncing. The note was submitted as own_output_notes
        // so we need to import it as input note to get the proof via sync.
        let note_details_for_import = NoteDetails::new(
            note.assets().clone(),
            note.recipient().clone(),
        );
        client
            .import_notes(&[NoteFile::NoteDetails {
                details: note_details_for_import,
                after_block_num: 0u32.into(),
                tag: Some(tag),
            }])
            .await?;
        eprintln!("[create] step 7: note imported into creator client for proof retrieval");

        // Sync to get the inclusion proof
        let mut got_proof = false;
        for attempt in 0..30 {
            client.sync_state().await?;
            if let Ok(Some(record)) = client.get_input_note(note_id).await {
                if record.is_authenticated() {
                    eprintln!(
                        "[create] step 7: note authenticated with proof (attempt {attempt})"
                    );
                    // Export as NoteFile::NoteWithProof
                    let proof = record.inclusion_proof().cloned()
                        .ok_or_else(|| anyhow::anyhow!("note is authenticated but has no inclusion proof"))?;
                    let record_note: Note = record.try_into()
                        .map_err(|e: miden_client::store::NoteRecordError| {
                            anyhow::anyhow!("Note convert: {e:?}")
                        })?;
                    let note_file = NoteFile::NoteWithProof(record_note, proof);
                    let note_file_bytes = note_file.to_bytes();
                    let note_file_b64 = base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        &note_file_bytes,
                    );
                    std::fs::write(output_dir.join("hasmapkey_note.mno"), &note_file_b64)?;
                    eprintln!(
                        "[create]   hasmapkey_note.mno: {} bytes (b64) — NoteFile::NoteWithProof",
                        note_file_b64.len()
                    );
                    got_proof = true;
                    break;
                }
            }
            if attempt == 29 {
                eprintln!("[create] step 7: WARNING: note never got authenticated, falling back to raw Note");
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        if !got_proof {
            // Fallback: save raw note bytes
            let note_b64 = base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &note_bytes,
            );
            std::fs::write(output_dir.join("hasmapkey_note.b64"), &note_b64)?;
            eprintln!(
                "[create]   hasmapkey_note.b64: {} bytes (b64) — raw Note fallback",
                note_b64.len()
            );
        }
    } else {
        // Private: export as NoteFile::NoteDetails
        let note_details = NoteDetails::new(
            note.assets().clone(),
            note.recipient().clone(),
        );
        let note_file = NoteFile::NoteDetails {
            details: note_details,
            after_block_num: 0u32.into(),
            tag: Some(tag),
        };
        let note_file_bytes = note_file.to_bytes();
        let note_file_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &note_file_bytes,
        );
        std::fs::write(output_dir.join("hasmapkey_note.mno"), &note_file_b64)?;
        eprintln!(
            "[create]   hasmapkey_note.mno: {} bytes (b64) — NoteFile::NoteDetails (private)",
            note_file_b64.len()
        );
    }

    // Always save raw note bytes too (needed for input_notes in consume)
    let note_b64 =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &note_bytes);
    std::fs::write(output_dir.join("hasmapkey_note.b64"), &note_b64)?;
    eprintln!(
        "[create]   hasmapkey_note.b64: {} bytes (b64) — raw Note",
        note_b64.len()
    );

    // Save wallet B account
    let wallet_b_bytes = wallet_b_account.to_bytes();
    let wallet_b_b64 =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &wallet_b_bytes);
    std::fs::write(output_dir.join("wallet_b.b64"), &wallet_b_b64)?;
    eprintln!(
        "[create]   wallet_b.b64: {} bytes (b64), account_id={}",
        wallet_b_b64.len(),
        wallet_b_id.to_hex()
    );

    // Save agent secret key
    let agent_sk_bytes = agent_sk.to_bytes();
    std::fs::write(output_dir.join("agent_sk.bin"), &agent_sk_bytes)?;
    eprintln!(
        "[create]   agent_sk.bin: {} bytes",
        agent_sk_bytes.len()
    );

    // Save serial_num (needed for signature computation in consume)
    let serial_data = serial_num.to_bytes();
    std::fs::write(output_dir.join("serial_num.bin"), &serial_data)?;
    eprintln!("[create]   serial_num.bin: {} bytes", serial_data.len());

    // Copy keystore files
    let src_keystore = tmp.path().join("keystore");
    let dst_keystore = output_dir.join("keystore");
    for entry in std::fs::read_dir(&src_keystore)? {
        let entry = entry?;
        let dst = dst_keystore.join(entry.file_name());
        std::fs::copy(entry.path(), &dst)?;
        eprintln!("[create]   keystore: copied {:?}", entry.file_name());
    }

    // Save setup.toml
    let setup_toml = format!(
        "wallet_b_id_hex = \"{}\"\nfaucet_id_hex = \"{}\"\nnote_private = {}\n",
        wallet_b_id.to_hex(),
        faucet_id.to_hex(),
        private,
    );
    std::fs::write(output_dir.join("setup.toml"), &setup_toml)?;
    eprintln!("[create]   setup.toml written");

    eprintln!("[create] DONE. Artifacts saved to {OUTPUT_DIR}");
    eprintln!("[create] Note ID: {note_id}");
    eprintln!("[create] Wallet B ID: {}", wallet_b_id.to_hex());
    eprintln!(
        "[create] Now run proc_hasmapkey_consume to consume the note from a separate process."
    );

    Ok(())
}
