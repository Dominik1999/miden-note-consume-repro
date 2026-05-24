//! Process 2: Consume a P2ID note created by a separate process.
//!
//! Reads artifacts from `/tmp/p2id_cross_process/` (created by p2id_create_process),
//! creates a fresh miden-client, imports wallet B + the P2ID note, syncs, and
//! attempts to consume the note.
//!
//! This reproduces the exact cross-process pattern that fails in ADN tests:
//! one process creates the note, another process consumes it.
//!
//! Run with:
//!   RUST_LOG=info cargo test --test p2id_consume_process -- --ignored --nocapture

use std::sync::Arc;
use std::time::Duration;

use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::Client;
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_client::rpc::{Endpoint, GrpcClient};
use miden_protocol::account::{Account, AccountId};
use miden_protocol::note::{Note, NoteDetails, NoteFile, NoteTag};
use miden_protocol::utils::serde::Deserializable;

const INPUT_DIR: &str = "/tmp/p2id_cross_process";

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

#[tokio::test]
#[ignore = "requires p2id_create_process to have run first + testnet access"]
async fn p2id_consume_from_file() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let input_dir = std::path::Path::new(INPUT_DIR);
    if !input_dir.exists() {
        anyhow::bail!(
            "Input directory {INPUT_DIR} does not exist. Run p2id_create_process first."
        );
    }

    // ── Read setup.toml ──
    let setup_toml: toml::Value =
        toml::from_str(&std::fs::read_to_string(input_dir.join("setup.toml"))?)?;
    let wallet_b_id_hex = setup_toml["wallet_b_id_hex"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing wallet_b_id_hex in setup.toml"))?;
    let faucet_id_hex = setup_toml["faucet_id_hex"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing faucet_id_hex in setup.toml"))?;
    let wallet_b_id = AccountId::from_hex(wallet_b_id_hex)?;
    let _faucet_id = AccountId::from_hex(faucet_id_hex)?;
    eprintln!("[consume] wallet B id: {}", wallet_b_id.to_hex());
    eprintln!("[consume] faucet id:   {faucet_id_hex}");

    // ── Read wallet B account ──
    let wallet_b_b64 = std::fs::read_to_string(input_dir.join("wallet_b.b64"))?;
    let wallet_b_bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        wallet_b_b64.trim(),
    )?;
    let wallet_b_account = Account::read_from_bytes(&wallet_b_bytes)?;
    eprintln!(
        "[consume] wallet B account loaded: id={}",
        wallet_b_account.id().to_hex()
    );
    assert_eq!(
        wallet_b_account.id(),
        wallet_b_id,
        "wallet B id mismatch between setup.toml and account bytes"
    );

    // ── Read P2ID note ──
    let note_b64 = std::fs::read_to_string(input_dir.join("p2id_note.b64"))?;
    let note_bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        note_b64.trim(),
    )?;
    let p2id_note = Note::read_from_bytes(&note_bytes)?;
    let note_id = p2id_note.id();
    eprintln!("[consume] P2ID note loaded: id={note_id}");
    eprintln!(
        "[consume]   script_root: {:?}",
        p2id_note.recipient().script().root()
    );
    eprintln!(
        "[consume]   storage items: {}",
        p2id_note.recipient().storage().num_items()
    );
    eprintln!(
        "[consume]   assets: {}",
        p2id_note.assets().num_assets()
    );
    eprintln!(
        "[consume]   tag: {:?}",
        p2id_note.metadata().tag()
    );

    // ── Create a fresh client ──
    let tmp = tempfile::tempdir()?;
    let data_dir = tmp.path();

    // Copy keystore files from the create process
    let dst_keystore = data_dir.join("keystore");
    std::fs::create_dir_all(&dst_keystore)?;
    let src_keystore = input_dir.join("keystore");
    for entry in std::fs::read_dir(&src_keystore)? {
        let entry = entry?;
        std::fs::copy(entry.path(), dst_keystore.join(entry.file_name()))?;
        eprintln!("[consume] keystore: copied {:?}", entry.file_name());
    }

    let (mut client, _keystore) = build_client(data_dir).await?;

    // Import wallet B account
    client.add_account(&wallet_b_account, false).await?;
    eprintln!("[consume] wallet B imported into client");

    // Initial sync
    client.sync_state().await?;
    eprintln!("[consume] client synced");

    // Import the P2ID note
    let tag = NoteTag::with_account_target(wallet_b_id);
    let note_details = NoteDetails::new(
        p2id_note.assets().clone(),
        p2id_note.recipient().clone(),
    );
    client
        .import_notes(&[NoteFile::NoteDetails {
            details: note_details,
            after_block_num: 0u32.into(),
            tag: Some(tag),
        }])
        .await?;
    eprintln!("[consume] P2ID note imported into client");

    // Wait for the note to become consumable
    eprintln!("[consume] waiting for P2ID note to become consumable...");
    let mut found = false;
    for attempt in 0..60 {
        client.sync_state().await?;
        let consumable = client.get_consumable_notes(Some(wallet_b_id)).await?;
        if consumable.iter().any(|(n, _)| n.id() == note_id) {
            eprintln!("[consume] note is consumable (attempt {attempt})");
            found = true;
            break;
        }
        if attempt == 59 {
            eprintln!("[consume] WARNING: note never became consumable after 60 attempts");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    if !found {
        // Check note state
        match client.get_input_note(note_id).await {
            Ok(Some(record)) => {
                eprintln!("[consume] note state: {:?}", record.state());
                eprintln!(
                    "[consume] note is_authenticated: {}",
                    record.is_authenticated()
                );
            }
            Ok(None) => eprintln!("[consume] note NOT FOUND in store"),
            Err(e) => eprintln!("[consume] get_input_note error: {e}"),
        }
        eprintln!("[consume] trying to consume anyway...");
    }

    // ── Attempt to consume the P2ID note ──
    eprintln!("[consume] consuming P2ID note...");

    // P2ID notes don't need note_args — they just verify the consuming account
    // matches the target account ID stored in the note's storage.
    let consume_req = TransactionRequestBuilder::new()
        .build_consume_notes(vec![p2id_note])
        .map_err(|e| anyhow::anyhow!("consume build: {e:?}"))?;

    match client
        .submit_new_transaction(wallet_b_id, consume_req)
        .await
    {
        Ok(tx_id) => {
            eprintln!("[consume] SUCCESS: tx_id={tx_id}");
            eprintln!("[consume] P2ID note consumed successfully from a separate process!");
            eprintln!("[consume] This proves standard P2ID works cross-process.");
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!("[consume] FAILED: {}", &err_str[..err_str.len().min(500)]);
            if err_str.contains("StackReadFailed") {
                eprintln!("[consume] StackReadFailed! P2ID fails cross-process too.");
                eprintln!("[consume] This is the same failure mode as the ADN test.");
                anyhow::bail!("P2ID cross-process consume failed with StackReadFailed");
            } else {
                eprintln!("[consume] Different error (not StackReadFailed).");
                anyhow::bail!("P2ID cross-process consume failed: {e}");
            }
        }
    }

    eprintln!("[consume] DONE.");
    Ok(())
}
