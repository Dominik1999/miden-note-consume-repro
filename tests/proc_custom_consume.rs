//! Process 2: Consume a custom secret_hash_note created by proc_custom_create.
//!
//! Reads artifacts from /tmp/proc_custom/ (created by proc_custom_create),
//! creates a fresh miden-client, imports wallet B + the NoteFile, syncs,
//! and consumes the note with the secret [42, 43, 44, 45] as note_args.
//!
//! Run with:
//!   RUST_LOG=info cargo test --test proc_custom_consume -- --ignored --nocapture

use std::sync::Arc;
use std::time::Duration;

use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::{Client, Felt};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_client::rpc::{Endpoint, GrpcClient};
use miden_protocol::account::{Account, AccountId};
use miden_protocol::note::{Note, NoteFile};
use miden_protocol::utils::serde::Deserializable;
use miden_protocol::Word;

const INPUT_DIR: &str = "/tmp/proc_custom";

fn secret() -> Word {
    [Felt::new(42), Felt::new(43), Felt::new(44), Felt::new(45)].into()
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

#[tokio::test]
#[ignore = "requires proc_custom_create to have run first + testnet access"]
async fn proc_custom_consume_from_file() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let input_dir = std::path::Path::new(INPUT_DIR);
    if !input_dir.exists() {
        anyhow::bail!(
            "Input directory {INPUT_DIR} does not exist. Run proc_custom_create first."
        );
    }

    // Read setup.toml
    let setup_toml: toml::Value =
        toml::from_str(&std::fs::read_to_string(input_dir.join("setup.toml"))?)?;
    let wallet_b_id_hex = setup_toml["wallet_b_id_hex"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing wallet_b_id_hex in setup.toml"))?;
    let faucet_id_hex = setup_toml["faucet_id_hex"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing faucet_id_hex in setup.toml"))?;
    let note_type_str = setup_toml
        .get("note_type")
        .and_then(|v| v.as_str())
        .unwrap_or("Public");
    let wallet_b_id = AccountId::from_hex(wallet_b_id_hex)?;
    let _faucet_id = AccountId::from_hex(faucet_id_hex)?;
    eprintln!("[consume] wallet B id: {}", wallet_b_id.to_hex());
    eprintln!("[consume] faucet id:   {faucet_id_hex}");
    eprintln!("[consume] note_type:   {note_type_str}");

    // Read wallet B account
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
        "wallet B id mismatch"
    );

    // Read the raw Note (for consumption via input_notes)
    let note_b64 = std::fs::read_to_string(input_dir.join("note.b64"))?;
    let note_bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        note_b64.trim(),
    )?;
    let custom_note = Note::read_from_bytes(&note_bytes)?;
    let note_id = custom_note.id();
    eprintln!("[consume] custom note loaded: id={note_id}");
    eprintln!(
        "[consume]   script_root: {:?}",
        custom_note.recipient().script().root()
    );
    eprintln!(
        "[consume]   storage items: {}",
        custom_note.recipient().storage().num_items()
    );
    eprintln!(
        "[consume]   assets: {}",
        custom_note.assets().num_assets()
    );

    // Read NoteFile (.mno) for import
    let mno_bytes = std::fs::read(input_dir.join("note.mno"))?;
    let note_file = NoteFile::read_from_bytes(&mno_bytes)
        .map_err(|e| anyhow::anyhow!("NoteFile deserialize: {e:?}"))?;
    let note_file_variant = match &note_file {
        NoteFile::NoteWithProof(_, _) => "NoteWithProof",
        NoteFile::NoteDetails { .. } => "NoteDetails",
        NoteFile::NoteId(_) => "NoteId",
    };
    eprintln!("[consume] NoteFile loaded: variant={note_file_variant}");

    // Create a fresh client
    let tmp = tempfile::tempdir()?;
    let data_dir = tmp.path();

    // Copy keystore files
    let dst_keystore = data_dir.join("keystore");
    std::fs::create_dir_all(&dst_keystore)?;
    let src_keystore = input_dir.join("keystore");
    for entry in std::fs::read_dir(&src_keystore)? {
        let entry = entry?;
        std::fs::copy(entry.path(), dst_keystore.join(entry.file_name()))?;
        eprintln!("[consume] keystore: copied {:?}", entry.file_name());
    }

    let (mut client, _keystore) = build_client(data_dir).await?;

    // Import wallet B
    client.add_account(&wallet_b_account, false).await?;
    eprintln!("[consume] wallet B imported into client");

    // Initial sync
    client.sync_state().await?;
    eprintln!("[consume] client synced");

    // Import NoteFile
    client.import_notes(&[note_file]).await?;
    eprintln!("[consume] NoteFile imported");

    // Sync to authenticate the note (for public notes)
    eprintln!("[consume] syncing to authenticate note...");
    let max_attempts = if note_type_str == "Private" { 20 } else { 60 };
    for attempt in 0..max_attempts {
        client.sync_state().await?;

        if let Ok(Some(record)) = client.get_input_note(note_id).await {
            if attempt % 5 == 0 || record.is_authenticated() {
                eprintln!(
                    "[consume]   attempt {attempt}: is_authenticated={}",
                    record.is_authenticated()
                );
            }
            if record.is_authenticated() {
                eprintln!("[consume] note authenticated!");
                break;
            }
        } else if attempt % 10 == 0 {
            eprintln!("[consume]   attempt {attempt}: note not in store yet");
        }

        if attempt == max_attempts - 1 {
            if note_type_str == "Private" {
                eprintln!("[consume] private note stays unauthenticated (expected), proceeding...");
            } else {
                eprintln!("[consume] WARNING: public note never authenticated after {max_attempts} attempts");
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    // Consume via input_notes with secret
    eprintln!("[consume] consuming custom note with secret [42,43,44,45]...");
    let note_for_consume = Note::read_from_bytes(&note_bytes)?;
    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(note_for_consume, Some(secret()))])
        .build()
        .map_err(|e| anyhow::anyhow!("input_notes build: {e:?}"))?;

    match client
        .submit_new_transaction(wallet_b_id, consume_req)
        .await
    {
        Ok(tx_id) => {
            eprintln!("============================================================");
            eprintln!("SUCCESS: Custom {note_type_str} note consumed cross-process!");
            eprintln!("  tx_id = {tx_id}");
            eprintln!("============================================================");
            Ok(())
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!("============================================================");
            eprintln!("FAILED: Custom {note_type_str} note cross-process consume");
            eprintln!("  error: {}", &err_str[..err_str.len().min(800)]);
            eprintln!("============================================================");
            anyhow::bail!("Custom note cross-process consume failed: {e}");
        }
    }
}
