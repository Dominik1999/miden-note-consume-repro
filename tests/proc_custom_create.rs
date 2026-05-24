//! Process 1: Create a custom secret_hash_note on testnet and save artifacts to disk.
//!
//! Deploys faucet + wallet A, creates wallet B (not added to client), mints + funds
//! wallet A, builds a custom note with secret_hash_note.masm, submits via own_output_notes,
//! waits for on-chain commit, exports as NoteFile::NoteWithProof, and saves everything
//! to /tmp/proc_custom/.
//!
//! NoteType is controlled by env var PRIVATE: set PRIVATE=1 for NoteType::Private,
//! otherwise NoteType::Public.
//!
//! Run with:
//!   RUST_LOG=info cargo test --test proc_custom_create -- --ignored --nocapture
//!   PRIVATE=1 RUST_LOG=info cargo test --test proc_custom_create -- --ignored --nocapture

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
use miden_client::store::{NoteExportType, NoteFilter};
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::{Client, Felt};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::asset::Asset;
use miden_protocol::note::*;
use miden_protocol::utils::serde::Serializable;
use miden_protocol::{Hasher, Word};
use miden_standards::code_builder::CodeBuilder;
use rand::RngCore;

const NOTE_MASM: &str = include_str!("../masm/secret_hash_note.masm");
const OUTPUT_DIR: &str = "/tmp/proc_custom";

fn secret() -> Word {
    [Felt::new(42), Felt::new(43), Felt::new(44), Felt::new(45)].into()
}

fn secret_digest() -> Word {
    let s: [Felt; 4] = secret().into();
    Hasher::hash_elements(&s)
}

fn note_type_from_env() -> NoteType {
    match std::env::var("NOTE_PRIVATE").as_deref() {
        Ok("1") => NoteType::Private,
        _ => NoteType::Public,
    }
}

async fn build_client(
    dir: &std::path::Path,
) -> anyhow::Result<(Client<FilesystemKeyStore>, Arc<FilesystemKeyStore>)> {
    let endpoint = Endpoint::try_from("https://rpc.testnet.miden.io")
        .map_err(|e| anyhow::anyhow!("endpoint: {e:?}"))?;
    let rpc = Arc::new(GrpcClient::new(&endpoint, 600_000));
    let keystore_dir = dir.join("keystore");
    std::fs::create_dir_all(&keystore_dir)?;
    let ks = Arc::new(
        FilesystemKeyStore::new(keystore_dir).map_err(|e| anyhow::anyhow!("keystore: {e:?}"))?,
    );
    let c = ClientBuilder::new()
        .rpc(rpc)
        .sqlite_store(dir.join("store.sqlite3"))
        .authenticator(ks.clone())
        .in_debug_mode(true.into())
        .build()
        .await
        .map_err(|e| anyhow::anyhow!("client build: {e}"))?;
    Ok((c, ks))
}

fn rand_seed(client: &mut Client<FilesystemKeyStore>) -> [u8; 32] {
    let mut seed = [0u8; 32];
    client.rng().fill_bytes(&mut seed);
    seed
}

#[tokio::test]
#[ignore = "requires network access to Miden testnet"]
async fn proc_custom_create_and_save() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let note_type = note_type_from_env();
    eprintln!("[create] NoteType = {:?}", note_type);

    // Clean + create output dir
    let output_dir = std::path::Path::new(OUTPUT_DIR);
    if output_dir.exists() {
        std::fs::remove_dir_all(output_dir)?;
    }
    std::fs::create_dir_all(output_dir)?;
    std::fs::create_dir_all(output_dir.join("keystore"))?;

    // Client setup
    let tmp = tempfile::tempdir()?;
    let (mut client, keystore) = build_client(tmp.path()).await?;
    client.sync_state().await?;
    eprintln!("[create] step 0: client synced");

    // 1. Deploy faucet
    let faucet_seed = rand_seed(&mut client);
    let faucet_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let symbol = TokenSymbol::new("CPROC").map_err(|e| anyhow::anyhow!("symbol: {e:?}"))?;
    let faucet = AccountBuilder::new(faucet_seed)
        .account_type(AccountType::FungibleFaucet)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            faucet_key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(
            BasicFungibleFaucet::new(symbol, 6, Felt::new(1_000_000_000))
                .map_err(|e| anyhow::anyhow!("faucet component: {e:?}"))?,
        )
        .with_component(AuthControlled::allow_all())
        .build()
        .map_err(|e| anyhow::anyhow!("faucet build: {e:?}"))?;
    let faucet_id = faucet.id();
    client.add_account(&faucet, false).await?;
    keystore
        .add_key(&faucet_key, faucet_id)
        .await
        .map_err(|e| anyhow::anyhow!("faucet key: {e:?}"))?;
    eprintln!("[create] step 1: faucet deployed: {}", faucet_id.to_hex());
    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 2. Deploy wallet A
    let wallet_a_seed = rand_seed(&mut client);
    let wallet_a_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let wallet_a = AccountBuilder::new(wallet_a_seed)
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
        .map_err(|e| anyhow::anyhow!("wallet A key: {e:?}"))?;
    eprintln!("[create] step 2: wallet A deployed: {}", wallet_a_id.to_hex());
    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 3. Create wallet B (do NOT add to client)
    let wallet_b_seed = rand_seed(&mut client);
    let wallet_b_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let wallet_b = AccountBuilder::new(wallet_b_seed)
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
    // Save wallet B key to keystore so the consumer process can use it
    keystore
        .add_key(&wallet_b_key, wallet_b_id)
        .await
        .map_err(|e| anyhow::anyhow!("wallet B key: {e:?}"))?;
    eprintln!(
        "[create] step 3: wallet B created (NOT in client): {}",
        wallet_b_id.to_hex()
    );

    // 4. Mint tokens to wallet A and consume the mint note
    eprintln!("[create] step 4: minting tokens to wallet A...");
    let mint_asset = FungibleAsset::new(faucet_id, 10_000)
        .map_err(|e| anyhow::anyhow!("mint asset: {e:?}"))?;
    let mint_req = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(mint_asset, wallet_a_id, NoteType::Public, client.rng())
        .map_err(|e| anyhow::anyhow!("mint req: {e:?}"))?;
    let mint_tx = client.submit_new_transaction(faucet_id, mint_req).await?;
    eprintln!("[create] step 4: mint tx submitted: {mint_tx}");

    for attempt in 0..60 {
        client.sync_state().await?;
        let consumable = client.get_consumable_notes(Some(wallet_a_id)).await?;
        if !consumable.is_empty() {
            eprintln!(
                "[create] step 4: mint note consumable (attempt {attempt}), consuming..."
            );
            let notes: Vec<_> = consumable
                .into_iter()
                .map(|(note, _)| note.try_into())
                .collect::<Result<_, _>>()?;
            let consume_req = TransactionRequestBuilder::new()
                .build_consume_notes(notes)
                .map_err(|e| anyhow::anyhow!("consume mint: {e:?}"))?;
            let consume_tx = client
                .submit_new_transaction(wallet_a_id, consume_req)
                .await?;
            eprintln!("[create] step 4: mint note consumed: {consume_tx}");
            break;
        }
        if attempt == 59 {
            anyhow::bail!("timed out waiting for consumable mint note");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    eprintln!("[create] step 4: wallet A funded");

    // 5. Build custom secret_hash_note
    let note_script = CodeBuilder::default().compile_note_script(NOTE_MASM)?;

    let digest = secret_digest();
    let digest_felts: [Felt; 4] = digest.into();
    let storage = NoteStorage::new(digest_felts.to_vec())?;

    let mut serial_bytes = [0u8; 32];
    client.rng().fill_bytes(&mut serial_bytes);
    let serial: Word = [
        Felt::new(u64::from_le_bytes(serial_bytes[0..8].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[8..16].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[16..24].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[24..32].try_into().unwrap())),
    ]
    .into();

    let send_asset = FungibleAsset::new(faucet_id, 1_000)
        .map_err(|e| anyhow::anyhow!("send asset: {e:?}"))?;
    let tag = NoteTag::with_account_target(wallet_b_id);
    let metadata = NoteMetadata::new(wallet_a_id, note_type).with_tag(tag);
    let vault = NoteAssets::new(vec![Asset::Fungible(send_asset)])?;
    let recipient = NoteRecipient::new(serial, note_script, storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();
    eprintln!(
        "[create] step 5: custom note built, id={note_id}, type={:?}",
        note_type
    );

    // 6. Submit via own_output_notes
    let create_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![note.clone()])
        .build()
        .map_err(|e| anyhow::anyhow!("own_output_notes tx build: {e:?}"))?;
    let create_tx = client
        .submit_new_transaction(wallet_a_id, create_req)
        .await?;
    eprintln!("[create] step 6: custom note submitted on-chain: {create_tx}");

    // 7. Wait for on-chain commit, sync to get inclusion proof
    eprintln!("[create] step 7: waiting for on-chain commit...");
    tokio::time::sleep(Duration::from_secs(10)).await;
    for attempt in 0..30 {
        client.sync_state().await?;
        // Check if we have the output note with proof
        let output_notes = client.get_output_notes(NoteFilter::All).await?;
        let has_proof = output_notes.iter().any(|n| n.id() == note_id);
        if has_proof {
            eprintln!(
                "[create] step 7: note found in output notes (attempt {attempt})"
            );
            break;
        }
        if attempt == 29 {
            eprintln!("[create] WARNING: note not found in output notes after 30 sync attempts");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    // 8. Export as NoteFile (NoteWithProof if public, NoteDetails if private)
    let output_notes = client.get_output_notes(NoteFilter::All).await?;
    let output_note_record = output_notes
        .into_iter()
        .find(|n| n.id() == note_id)
        .ok_or_else(|| anyhow::anyhow!("output note record not found for {note_id}"))?;
    eprintln!(
        "[create] step 8: found output note record, id={}",
        output_note_record.id()
    );

    // Try NoteWithProof first; fall back to NoteDetails for private notes
    let note_file = match output_note_record
        .clone()
        .into_note_file(&NoteExportType::NoteWithProof)
    {
        Ok(nf) => {
            eprintln!("[create] step 8: exported as NoteWithProof");
            nf
        }
        Err(e) => {
            eprintln!(
                "[create] step 8: NoteWithProof export failed ({e:?}), falling back to NoteDetails"
            );
            output_note_record
                .into_note_file(&NoteExportType::NoteDetails)
                .map_err(|e| anyhow::anyhow!("NoteDetails export also failed: {e:?}"))?
        }
    };

    // Save note.mno
    let note_file_bytes = note_file.to_bytes();
    std::fs::write(output_dir.join("note.mno"), &note_file_bytes)?;
    eprintln!(
        "[create] step 8: note.mno saved ({} bytes)",
        note_file_bytes.len()
    );

    // Also save raw Note bytes for consumption via input_notes
    let note_bytes = note.to_bytes();
    let note_b64 =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &note_bytes);
    std::fs::write(output_dir.join("note.b64"), &note_b64)?;
    eprintln!("[create]   note.b64 saved ({} bytes b64)", note_b64.len());

    // Save wallet B account
    let wallet_b_bytes = wallet_b.to_bytes();
    let wallet_b_b64 =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &wallet_b_bytes);
    std::fs::write(output_dir.join("wallet_b.b64"), &wallet_b_b64)?;
    eprintln!(
        "[create]   wallet_b.b64 saved ({} bytes b64)",
        wallet_b_b64.len()
    );

    // Copy keystore files
    let src_keystore = tmp.path().join("keystore");
    let dst_keystore = output_dir.join("keystore");
    for entry in std::fs::read_dir(&src_keystore)? {
        let entry = entry?;
        std::fs::copy(entry.path(), dst_keystore.join(entry.file_name()))?;
        eprintln!("[create]   keystore: copied {:?}", entry.file_name());
    }

    // Save setup.toml
    let setup_toml = format!(
        "wallet_b_id_hex = \"{}\"\nfaucet_id_hex = \"{}\"\nnote_type = \"{:?}\"\n",
        wallet_b_id.to_hex(),
        faucet_id.to_hex(),
        note_type,
    );
    std::fs::write(output_dir.join("setup.toml"), &setup_toml)?;
    eprintln!("[create]   setup.toml written");

    eprintln!("[create] DONE. Artifacts saved to {OUTPUT_DIR}");
    eprintln!("[create] Note ID: {note_id}");
    eprintln!("[create] Wallet B ID: {}", wallet_b_id.to_hex());
    eprintln!(
        "[create] NoteType: {:?}",
        note_type
    );
    eprintln!("[create] Now run proc_custom_consume to consume from a separate process.");

    Ok(())
}
