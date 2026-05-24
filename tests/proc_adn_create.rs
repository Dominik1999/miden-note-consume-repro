//! Process 1: Create a full ADN (Agent Debit Note) on testnet and save
//! artifacts to disk for a separate consumer process.
//!
//! Deploys faucet + wallet A + wallet B (wallet B NOT in client A),
//! mints + funds wallet A, creates ADN note with agent_debit_note.masm,
//! submits via own_output_notes, exports as NoteFile::NoteWithProof,
//! and saves all artifacts to /tmp/proc_adn/.
//!
//! NoteType: set PRIVATE=1 for Private, otherwise Public.
//!
//! Run with:
//!   RUST_LOG=info cargo test --test proc_adn_create -- --ignored --nocapture
//!   PRIVATE=1 RUST_LOG=info cargo test --test proc_adn_create -- --ignored --nocapture

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

const ADN_MASM: &str = include_str!("../masm/agent_debit_note.masm");
const OUTPUT_DIR: &str = "/tmp/proc_adn";

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
async fn proc_adn_create() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let note_type = if std::env::var("PRIVATE").unwrap_or_default() == "1" {
        eprintln!("[create] NoteType: PRIVATE");
        NoteType::Private
    } else {
        eprintln!("[create] NoteType: PUBLIC");
        NoteType::Public
    };

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
    let faucet_id = deploy_faucet(&mut client, &keystore, "PADN").await?;
    eprintln!("[create] step 1: faucet deployed: {}", faucet_id.to_hex());
    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Deploy wallet A (the "user" / reclaim target)
    let wallet_a_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let wallet_a = AccountBuilder::new(rand_seed(&mut client))
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            wallet_a_key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(BasicWallet)
        .build()
        .map_err(|e| anyhow::anyhow!("wallet A build: {e:?}"))?;
    let wallet_a_id = wallet_a.id();
    client.add_account(&wallet_a, false).await?;
    keystore
        .add_key(&wallet_a_key, wallet_a_id)
        .await
        .map_err(|e| anyhow::anyhow!("wallet A keystore: {e:?}"))?;
    eprintln!(
        "[create] step 2: wallet A deployed: {}",
        wallet_a_id.to_hex()
    );
    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Create wallet B (receiver) -- NOT registered in this client
    let wallet_b_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let wallet_b = AccountBuilder::new(rand_seed(&mut client))
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            wallet_b_key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(BasicWallet)
        .build()
        .map_err(|e| anyhow::anyhow!("wallet B build: {e:?}"))?;
    let wallet_b_id = wallet_b.id();
    // Save wallet B key to keystore so the consume process can use it
    keystore
        .add_key(&wallet_b_key, wallet_b_id)
        .await
        .map_err(|e| anyhow::anyhow!("wallet B keystore: {e:?}"))?;
    eprintln!(
        "[create] step 3: wallet B created (NOT in this client): {}",
        wallet_b_id.to_hex()
    );

    // Mint tokens to wallet A and consume
    eprintln!("[create] step 4: minting tokens to wallet A...");
    mint_and_consume(&mut client, faucet_id, wallet_a_id, 10_000).await?;
    eprintln!("[create] step 4: wallet A funded");
    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ── Generate agent signing key ──
    let agent_sk = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let agent_pk: miden_protocol::Word = agent_sk.public_key().to_commitment().into();

    // ── Compile ADN note script ──
    let note_script = CodeBuilder::default().compile_note_script(ADN_MASM)?;
    eprintln!("[create] step 5: ADN note script compiled");

    // ── Build ADN note ──
    let balance = 1000u64;
    let asset =
        FungibleAsset::new(faucet_id, balance).map_err(|e| anyhow::anyhow!("asset: {e:?}"))?;

    // Storage: 7 items [agent_pk(4), wallet_a_suffix, wallet_a_prefix, expiry=1_000_000]
    let storage = NoteStorage::new(vec![
        agent_pk[0],
        agent_pk[1],
        agent_pk[2],
        agent_pk[3],
        wallet_a_id.suffix(),
        wallet_a_id.prefix().as_felt(),
        Felt::new(1_000_000),
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
    eprintln!("[create] step 5: ADN note built, id={note_id}");

    // ── Submit ADN note on-chain via own_output_notes ──
    let create_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![note.clone()])
        .build()
        .map_err(|e| anyhow::anyhow!("create note tx: {e:?}"))?;
    let create_tx = client
        .submit_new_transaction(wallet_a_id, create_req)
        .await?;
    eprintln!("[create] step 6: ADN note submitted on-chain: {create_tx}");

    // Wait for on-chain confirmation and sync
    eprintln!("[create] step 6: waiting for on-chain confirmation...");
    tokio::time::sleep(Duration::from_secs(10)).await;
    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    client.sync_state().await?;

    // ── Export note as NoteFile::NoteWithProof ──
    let output_notes = client
        .get_output_notes(miden_client::store::NoteFilter::All)
        .await?;
    eprintln!(
        "[create] step 7: found {} output notes total",
        output_notes.len()
    );

    // Find the ADN output note (match by note ID)
    let adn_output = output_notes
        .into_iter()
        .find(|n| n.id() == note_id)
        .ok_or_else(|| anyhow::anyhow!("ADN output note not found in store by id={note_id}"))?;
    eprintln!(
        "[create] step 7: found ADN output note, state={:?}",
        adn_output.state()
    );

    // Try NoteWithProof first, fall back to NoteDetails
    let note_file = match adn_output.clone().into_note_file(&NoteExportType::NoteWithProof) {
        Ok(nf) => {
            eprintln!("[create] step 7: exported as NoteFile::NoteWithProof");
            nf
        }
        Err(e) => {
            eprintln!(
                "[create] step 7: NoteWithProof export failed ({e:?}), trying NoteDetails..."
            );
            // If no inclusion proof yet, sync more and retry
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_secs(5)).await;
                client.sync_state().await?;
            }
            let output_notes2 = client
                .get_output_notes(miden_client::store::NoteFilter::All)
                .await?;
            let adn_output2 = output_notes2
                .into_iter()
                .find(|n| n.id() == note_id)
                .ok_or_else(|| anyhow::anyhow!("ADN output note lost after re-sync"))?;
            match adn_output2.clone().into_note_file(&NoteExportType::NoteWithProof) {
                Ok(nf) => {
                    eprintln!("[create] step 7: exported as NoteWithProof after extra syncs");
                    nf
                }
                Err(_) => {
                    eprintln!("[create] step 7: still no proof, falling back to NoteDetails");
                    adn_output2.into_note_file(&NoteExportType::NoteDetails)?
                }
            }
        }
    };

    // Serialize NoteFile
    let note_file_bytes = note_file.to_bytes();
    let note_file_b64 =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &note_file_bytes);
    std::fs::write(output_dir.join("note.mno"), &note_file_b64)?;
    eprintln!(
        "[create] step 8: note.mno saved ({} bytes b64)",
        note_file_b64.len()
    );

    // Save wallet B account bytes (base64)
    let wallet_b_bytes = wallet_b.to_bytes();
    let wallet_b_b64 =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &wallet_b_bytes);
    std::fs::write(output_dir.join("wallet_b.b64"), &wallet_b_b64)?;
    eprintln!(
        "[create] step 8: wallet_b.b64 saved ({} bytes b64)",
        wallet_b_b64.len()
    );

    // Copy keystore files
    let src_keystore = tmp.path().join("keystore");
    let dst_keystore = output_dir.join("keystore");
    for entry in std::fs::read_dir(&src_keystore)? {
        let entry = entry?;
        let dst = dst_keystore.join(entry.file_name());
        std::fs::copy(entry.path(), &dst)?;
        eprintln!("[create]   keystore: copied {:?}", entry.file_name());
    }

    // Save agent_sk.bin (raw Falcon512 secret key bytes)
    let agent_sk_bytes = agent_sk.to_bytes();
    std::fs::write(output_dir.join("agent_sk.bin"), &agent_sk_bytes)?;
    eprintln!(
        "[create] step 8: agent_sk.bin saved ({} bytes)",
        agent_sk_bytes.len()
    );

    // Save setup.toml with wallet_b_id_hex + serial_num
    let serial_felts: [Felt; 4] = serial_num.into();
    let setup_toml = format!(
        "wallet_b_id_hex = \"{}\"\n\
         serial_num_hex = [\"0x{:x}\", \"0x{:x}\", \"0x{:x}\", \"0x{:x}\"]\n",
        wallet_b_id.to_hex(),
        serial_felts[0].as_canonical_u64(),
        serial_felts[1].as_canonical_u64(),
        serial_felts[2].as_canonical_u64(),
        serial_felts[3].as_canonical_u64(),
    );
    std::fs::write(output_dir.join("setup.toml"), &setup_toml)?;
    eprintln!("[create] step 8: setup.toml saved");

    eprintln!("[create] ========================================");
    eprintln!("[create] DONE. Artifacts saved to {OUTPUT_DIR}");
    eprintln!("[create] Note ID: {note_id}");
    eprintln!("[create] Wallet B ID: {}", wallet_b_id.to_hex());
    eprintln!(
        "[create] Now run: cargo test --test proc_adn_consume -- --ignored --nocapture"
    );
    eprintln!("[create] ========================================");

    Ok(())
}
