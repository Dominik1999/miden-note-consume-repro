//! Process 2: Consume a hasmapkey Falcon note created by a separate process.
//!
//! Reads artifacts from `/tmp/proc_hasmapkey/` (created by proc_hasmapkey_create),
//! creates a fresh miden-client, imports wallet B + the hasmapkey note, syncs, and
//! attempts to consume the note with the Falcon signature in the advice map.
//!
//! This reproduces the exact cross-process pattern: one process creates the note,
//! another process consumes it (with adv.has_mapkey signature verification).
//!
//! Run with:
//!   RUST_LOG=info cargo test --test proc_hasmapkey_consume -- --ignored --nocapture

use std::sync::Arc;
use std::time::Duration;

use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::{Client, Felt};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_client::rpc::{Endpoint, GrpcClient};
use miden_protocol::account::{Account, AccountId};
use miden_protocol::note::{Note, NoteFile, NoteTag};
use miden_protocol::utils::serde::Deserializable;
use miden_protocol::{Hasher, Word};
use miden_client::auth::AuthSecretKey;

const INPUT_DIR: &str = "/tmp/proc_hasmapkey";

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
#[ignore = "requires proc_hasmapkey_create to have run first + testnet access"]
async fn hasmapkey_consume_from_file() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let input_dir = std::path::Path::new(INPUT_DIR);
    if !input_dir.exists() {
        anyhow::bail!(
            "Input directory {INPUT_DIR} does not exist. Run proc_hasmapkey_create first."
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
    let note_private = setup_toml
        .get("note_private")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let wallet_b_id = AccountId::from_hex(wallet_b_id_hex)?;
    let _faucet_id = AccountId::from_hex(faucet_id_hex)?;
    eprintln!("[consume] wallet B id: {}", wallet_b_id.to_hex());
    eprintln!("[consume] faucet id:   {faucet_id_hex}");
    eprintln!(
        "[consume] note type:   {}",
        if note_private { "Private" } else { "Public" }
    );

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

    // ── Read agent secret key ──
    let agent_sk_bytes = std::fs::read(input_dir.join("agent_sk.bin"))?;
    let agent_sk = AuthSecretKey::read_from_bytes(&agent_sk_bytes)
        .map_err(|e| anyhow::anyhow!("agent_sk deserialize: {e}"))?;
    let agent_pk: Word = agent_sk.public_key().to_commitment().into();
    eprintln!("[consume] agent_sk loaded, pk={agent_pk:?}");

    // ── Read serial_num ──
    let serial_data = std::fs::read(input_dir.join("serial_num.bin"))?;
    let serial_num = Word::read_from_bytes(&serial_data)
        .map_err(|e| anyhow::anyhow!("serial_num deserialize: {e}"))?;
    eprintln!("[consume] serial_num loaded: {serial_num:?}");

    // ── Read hasmapkey note ──
    // Try .mno (NoteFile) first, fall back to .b64 (raw Note)
    let (note, note_file_for_import) = if input_dir.join("hasmapkey_note.mno").exists() {
        let mno_b64 = std::fs::read_to_string(input_dir.join("hasmapkey_note.mno"))?;
        let mno_bytes = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            mno_b64.trim(),
        )?;
        let note_file = NoteFile::read_from_bytes(&mno_bytes)
            .map_err(|e| anyhow::anyhow!("NoteFile decode: {e}"))?;
        let note_from_file: Option<Note> = match &note_file {
            NoteFile::NoteWithProof(n, _) => {
                eprintln!("[consume] loaded .mno as NoteFile::NoteWithProof (includes inclusion proof)");
                Some(n.clone())
            }
            NoteFile::NoteDetails { .. } => {
                eprintln!("[consume] loaded .mno as NoteFile::NoteDetails (private, no proof)");
                None
            }
            NoteFile::NoteId(_) => {
                eprintln!("[consume] loaded .mno as NoteFile::NoteId (unexpected)");
                None
            }
        };
        // If NoteWithProof, we have the note directly; otherwise load from .b64
        if let Some(n) = note_from_file {
            (n, Some(note_file))
        } else {
            // Load raw note from .b64 for input_notes
            let note_b64 = std::fs::read_to_string(input_dir.join("hasmapkey_note.b64"))?;
            let note_bytes = base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                note_b64.trim(),
            )?;
            let note = Note::read_from_bytes(&note_bytes)?;
            (note, Some(note_file))
        }
    } else {
        // Fallback: raw Note from .b64
        let note_b64 = std::fs::read_to_string(input_dir.join("hasmapkey_note.b64"))?;
        let note_bytes = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            note_b64.trim(),
        )?;
        let note = Note::read_from_bytes(&note_bytes)?;
        eprintln!("[consume] loaded .b64 (raw Note) -- no inclusion proof (legacy)");
        (note, None)
    };
    let note_id = note.id();
    eprintln!("[consume] hasmapkey note loaded: id={note_id}");
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

    // Import the hasmapkey note
    let tag = NoteTag::with_account_target(wallet_b_id);
    if let Some(nf) = note_file_for_import {
        eprintln!("[consume] importing via NoteFile (includes proof if public)");
        client.import_notes(&[nf]).await?;
    } else {
        use miden_protocol::note::NoteDetails;
        let note_details = NoteDetails::new(
            note.assets().clone(),
            note.recipient().clone(),
        );
        eprintln!("[consume] importing via NoteFile::NoteDetails (no inclusion proof)");
        client
            .import_notes(&[NoteFile::NoteDetails {
                details: note_details,
                after_block_num: 0u32.into(),
                tag: Some(tag),
            }])
            .await?;
    }
    eprintln!("[consume] hasmapkey note imported into client");

    // Wait for the note to become consumable (or at least authenticated for public)
    eprintln!("[consume] waiting for note to become ready...");
    if note_private {
        // Private notes stay unauthenticated -- just sync a few times
        for attempt in 0..10 {
            client.sync_state().await?;
            if attempt % 3 == 0 {
                if let Ok(Some(record)) = client.get_input_note(note_id).await {
                    eprintln!(
                        "[consume]   attempt {attempt}: state={:?}, is_authenticated={}",
                        record.state(),
                        record.is_authenticated()
                    );
                }
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        eprintln!("[consume] proceeding with unauthenticated note (private)");
    } else {
        // Public notes: wait for authentication
        let mut found = false;
        for attempt in 0..60 {
            client.sync_state().await?;
            if let Ok(Some(record)) = client.get_input_note(note_id).await {
                if record.is_authenticated() {
                    eprintln!("[consume] note authenticated (attempt {attempt})");
                    found = true;
                    break;
                }
                if attempt % 5 == 0 {
                    eprintln!(
                        "[consume]   attempt {attempt}: is_authenticated={}",
                        record.is_authenticated()
                    );
                }
            } else if attempt % 10 == 0 {
                eprintln!("[consume]   attempt {attempt}: note not in store yet");
            }
            if attempt == 59 {
                eprintln!("[consume] WARNING: note never became authenticated, trying anyway...");
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        if !found {
            if let Ok(Some(record)) = client.get_input_note(note_id).await {
                eprintln!("[consume] note state: {:?}", record.state());
                eprintln!(
                    "[consume] note is_authenticated: {}",
                    record.is_authenticated()
                );
            }
        }
    }

    // ── Build note_args + Falcon signature for consumption ──
    // Note args: [500, 0, 0, 0]
    let note_args: Word = [Felt::new(500), Felt::ZERO, Felt::ZERO, Felt::ZERO].into();

    // MESSAGE = merge(serial_num, note_args)
    let message: Word = Hasher::merge(&[serial_num.into(), note_args.into()]).into();
    let sig = agent_sk.sign(message);
    let prepared: Vec<Felt> = sig.to_prepared_signature(message);
    let sig_key: Word = Hasher::merge(&[agent_pk.into(), message.into()]).into();

    eprintln!("[consume] consuming hasmapkey note...");
    eprintln!("[consume]   note_args={note_args:?}");
    eprintln!("[consume]   sig_key={sig_key:?}");
    eprintln!("[consume]   prepared_sig len={}", prepared.len());

    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(note, Some(note_args))])
        .extend_advice_map([(sig_key, prepared.as_slice())])
        .build()
        .map_err(|e| anyhow::anyhow!("consume build: {e:?}"))?;

    match client
        .submit_new_transaction(wallet_b_id, consume_req)
        .await
    {
        Ok(tx_id) => {
            eprintln!("[consume] ══════════════════════════════════════════════════════");
            eprintln!("[consume] SUCCESS: hasmapkey note consumed cross-process! tx={tx_id}");
            eprintln!("[consume] ══════════════════════════════════════════════════════");
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!("[consume] ══════════════════════════════════════════════════════");
            eprintln!("[consume] FAILED: {}", &err_str[..err_str.len().min(800)]);
            eprintln!("[consume] ══════════════════════════════════════════════════════");
            anyhow::bail!("hasmapkey cross-process consume failed: {e}");
        }
    }

    eprintln!("[consume] DONE.");
    Ok(())
}
