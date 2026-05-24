//! Process 1: Create a standard P2ID note on testnet and save artifacts to disk.
//!
//! This test deploys a faucet + wallet A + wallet B, mints tokens to wallet A,
//! then sends a standard P2ID note from wallet A to wallet B. The note and
//! wallet B account are serialized to `/tmp/p2id_cross_process/` so that a
//! separate process (p2id_consume_process) can import and consume them.
//!
//! Run with:
//!   RUST_LOG=info cargo test --test p2id_create_process -- --ignored --nocapture

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
use miden_protocol::account::AccountId;
use miden_protocol::asset::Asset;
use miden_protocol::note::Note;
use miden_protocol::utils::serde::Serializable;
use rand::RngCore;

const OUTPUT_DIR: &str = "/tmp/p2id_cross_process";

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
async fn p2id_create_and_save() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

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
    let faucet_id = deploy_faucet(&mut client, &keystore, "XPROC").await?;
    eprintln!("[create] step 1: faucet deployed: {}", faucet_id.to_hex());
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

    // Create wallet B (receiver) — but do NOT register it in this client
    // so the P2ID note doesn't get auto-consumed by this process
    let wallet_b_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let wallet_b_account = AccountBuilder::new({
        let mut s = [0u8; 32]; client.rng().fill_bytes(&mut s); s
    })
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            wallet_b_key.public_key().to_commitment().into(), AuthSchemeId::Falcon512Poseidon2))
        .with_component(BasicWallet)
        .build().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let wallet_b_id = wallet_b_account.id();
    // Save wallet B key to keystore (for the consume process)
    keystore.add_key(&wallet_b_key, wallet_b_id).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    eprintln!(
        "[create] step 3: wallet B created (NOT registered in this client): {}",
        wallet_b_id.to_hex()
    );
    client.sync_state().await?;

    // Mint tokens to wallet A and consume the mint note
    eprintln!("[create] step 4: minting tokens to wallet A...");
    mint_and_consume(&mut client, faucet_id, wallet_a_id, 10_000).await?;
    eprintln!("[create] step 4: wallet A funded");
    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ── Send P2ID note from wallet A to wallet B ──
    eprintln!("[create] step 5: sending P2ID note from wallet A to wallet B...");
    let send_amount = 1_000u64;
    let send_asset = FungibleAsset::new(faucet_id, send_amount)
        .map_err(|e| anyhow::anyhow!("FungibleAsset: {e:?}"))?;

    let payment = PaymentNoteDescription::new(
        vec![Asset::Fungible(send_asset)],
        wallet_a_id,
        wallet_b_id,
    );
    let p2id_req = TransactionRequestBuilder::new()
        .build_pay_to_id(payment, NoteType::Public, client.rng())
        .map_err(|e| anyhow::anyhow!("build_pay_to_id: {e:?}"))?;
    let p2id_tx = client
        .submit_new_transaction(wallet_a_id, p2id_req)
        .await?;
    eprintln!("[create] step 5: P2ID tx submitted: {p2id_tx}");

    // Wait for the note to be committed on-chain
    eprintln!("[create] step 6: waiting for P2ID note to appear on-chain...");
    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    client.sync_state().await?;

    // Get the P2ID output note from the client's output_notes store
    let output_notes = client.get_output_notes(miden_client::store::NoteFilter::All).await?;
    eprintln!("[create] step 6: found {} output notes", output_notes.len());
    let p2id_note_record = output_notes.into_iter()
        .find(|n| {
            // The P2ID note should be the most recent output note (not the mint)
            eprintln!("  output note: id={}", n.id());
            true  // take the last one
        })
        .ok_or_else(|| anyhow::anyhow!("no output notes found"))?;
    // Convert OutputNoteRecord to Note
    let p2id_note: Note = p2id_note_record.try_into()
        .map_err(|e: miden_client::store::NoteRecordError| anyhow::anyhow!("note convert: {e:?}"))?;
    let note_id = p2id_note.id();
    eprintln!("[create] step 7: P2ID note found: id={note_id}");
    eprintln!(
        "[create]   script_root: {:?}",
        p2id_note.recipient().script().root()
    );
    eprintln!(
        "[create]   storage items: {}",
        p2id_note.recipient().storage().num_items()
    );
    eprintln!("[create]   assets: {}", p2id_note.assets().num_assets());
    eprintln!(
        "[create]   tag: {:?}",
        p2id_note.metadata().tag()
    );

    // ── Save artifacts ──
    eprintln!("[create] step 8: saving artifacts to {OUTPUT_DIR}");

    // Save wallet B account bytes (base64)
    let wallet_b_bytes = wallet_b_account.to_bytes();
    let wallet_b_b64 =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &wallet_b_bytes);
    std::fs::write(output_dir.join("wallet_b.b64"), &wallet_b_b64)?;
    eprintln!(
        "[create]   wallet_b.b64: {} bytes (b64), account_id={}",
        wallet_b_b64.len(),
        wallet_b_id.to_hex()
    );

    // Save P2ID note bytes (base64)
    let note_bytes = p2id_note.to_bytes();
    let note_b64 =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &note_bytes);
    std::fs::write(output_dir.join("p2id_note.b64"), &note_b64)?;
    eprintln!(
        "[create]   p2id_note.b64: {} bytes (b64), note_id={}",
        note_b64.len(),
        note_id
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

    // Save setup.toml
    let setup_toml = format!(
        "wallet_b_id_hex = \"{}\"\nfaucet_id_hex = \"{}\"\n",
        wallet_b_id.to_hex(),
        faucet_id.to_hex(),
    );
    std::fs::write(output_dir.join("setup.toml"), &setup_toml)?;
    eprintln!("[create]   setup.toml written");

    eprintln!("[create] DONE. Artifacts saved to {OUTPUT_DIR}");
    eprintln!("[create] Note ID: {note_id}");
    eprintln!("[create] Wallet B ID: {}", wallet_b_id.to_hex());
    eprintln!("[create] Now run p2id_consume_process to consume the note from a separate process.");

    Ok(())
}
