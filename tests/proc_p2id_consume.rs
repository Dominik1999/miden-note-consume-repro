//! Process 2: Consume a P2ID note from artifacts saved by proc_p2id_create.
//!
//! Reads from /tmp/proc_p2id/, creates a FRESH client, imports wallet B + note,
//! syncs, and consumes.
//!
//! Run:
//!   RUST_LOG=miden_client::transaction=info cargo test --test proc_p2id_consume -- --ignored --nocapture

use std::sync::Arc;
use std::time::Duration;

use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::Client;
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_client::rpc::{Endpoint, GrpcClient};
use miden_protocol::account::{Account, AccountId};
use miden_protocol::note::NoteFile;
use miden_protocol::utils::serde::Deserializable;

const INPUT_DIR: &str = "/tmp/proc_p2id";

async fn build_client(
    dir: &std::path::Path,
) -> anyhow::Result<(Client<FilesystemKeyStore>, Arc<FilesystemKeyStore>)> {
    let endpoint = Endpoint::try_from("https://rpc.testnet.miden.io").unwrap();
    let rpc = Arc::new(GrpcClient::new(&endpoint, 30_000));
    let ks_dir = dir.join("keystore");
    std::fs::create_dir_all(&ks_dir)?;
    let ks = Arc::new(FilesystemKeyStore::new(ks_dir).unwrap());
    let c = ClientBuilder::new()
        .rpc(rpc)
        .sqlite_store(dir.join("store.sqlite3"))
        .authenticator(ks.clone())
        .in_debug_mode(true.into())
        .build()
        .await?;
    Ok((c, ks))
}

#[tokio::test]
#[ignore = "requires proc_p2id_create to have run first + testnet access"]
async fn proc_p2id_consume() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let input_dir = std::path::Path::new(INPUT_DIR);
    if !input_dir.exists() {
        anyhow::bail!("Input directory {INPUT_DIR} does not exist. Run proc_p2id_create first.");
    }

    // ── Read setup.toml ──
    let setup_toml: toml::Value =
        toml::from_str(&std::fs::read_to_string(input_dir.join("setup.toml"))?)?;
    let wallet_b_id_hex = setup_toml["wallet_b_id_hex"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing wallet_b_id_hex"))?;
    let note_type_str = setup_toml
        .get("note_type")
        .and_then(|v| v.as_str())
        .unwrap_or("public");
    let wallet_b_id = AccountId::from_hex(wallet_b_id_hex)?;
    eprintln!("[consume] wallet B: {}", wallet_b_id.to_hex());
    eprintln!("[consume] note_type: {note_type_str}");

    // ── Read wallet B account ──
    let wallet_b_b64 = std::fs::read_to_string(input_dir.join("wallet_b.b64"))?;
    let wallet_b_bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        wallet_b_b64.trim(),
    )?;
    let wallet_b_account = Account::read_from_bytes(&wallet_b_bytes)?;
    eprintln!(
        "[consume] wallet B loaded: id={}",
        wallet_b_account.id().to_hex()
    );

    // ── Read note from .mno (NoteFile) ──
    let mno_b64 = std::fs::read_to_string(input_dir.join("note.mno"))?;
    let mno_bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        mno_b64.trim(),
    )?;
    let note_file = NoteFile::read_from_bytes(&mno_bytes)
        .map_err(|e| anyhow::anyhow!("NoteFile decode: {e}"))?;

    // Extract the Note for later use in consume request
    let (note, note_file_for_import) = match &note_file {
        NoteFile::NoteWithProof(n, _proof) => {
            eprintln!("[consume] loaded NoteFile::NoteWithProof (includes inclusion proof)");
            (n.clone(), note_file)
        }
        NoteFile::NoteDetails { .. } => {
            eprintln!("[consume] loaded NoteFile::NoteDetails (no inclusion proof)");
            // We can't get a full Note from NoteDetails alone without metadata,
            // so this path is not expected for our test
            anyhow::bail!("Expected NoteWithProof, got NoteDetails");
        }
        NoteFile::NoteId(_) => {
            anyhow::bail!("Expected NoteWithProof, got NoteId");
        }
    };
    let note_id = note.id();
    eprintln!("[consume] note id: {note_id}");
    eprintln!(
        "[consume]   script_root: {:?}",
        note.recipient().script().root()
    );
    eprintln!(
        "[consume]   storage items: {}",
        note.recipient().storage().num_items()
    );
    eprintln!("[consume]   assets: {}", note.assets().num_assets());
    eprintln!("[consume]   tag: {:?}", note.metadata().tag());

    // ── Create a FRESH client (new tempdir, new store) ──
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

    // Import wallet B via add_account
    client.add_account(&wallet_b_account, false).await?;
    eprintln!("[consume] wallet B imported");

    // Import note via NoteFile (NoteWithProof includes inclusion proof)
    client.import_notes(&[note_file_for_import]).await?;
    eprintln!("[consume] note imported via NoteFile");

    // Sync
    eprintln!("[consume] syncing...");
    for attempt in 0..60 {
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

        if attempt == 59 {
            eprintln!("[consume] WARNING: note never authenticated after 60 attempts");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    // ── Consume via input_notes ──
    eprintln!("[consume] consuming P2ID note...");
    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(note, None)])
        .build()
        .map_err(|e| anyhow::anyhow!("input_notes build: {e:?}"))?;

    match client.submit_new_transaction(wallet_b_id, consume_req).await {
        Ok(tx_id) => {
            eprintln!("[consume] ════════════════════════════════════════════════");
            eprintln!("[consume] SUCCESS: P2ID note consumed cross-process! tx={tx_id}");
            eprintln!("[consume] note_type: {note_type_str}");
            eprintln!("[consume] ════════════════════════════════════════════════");
            Ok(())
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!("[consume] ════════════════════════════════════════════════");
            eprintln!(
                "[consume] FAILED: {}",
                &err_str[..err_str.len().min(500)]
            );
            eprintln!("[consume] ════════════════════════════════════════════════");
            anyhow::bail!("P2ID cross-process consume failed: {e}");
        }
    }
}
