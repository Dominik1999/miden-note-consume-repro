//! Process 2: Consume a Falcon-verified note created by proc_falcon_create.
//!
//! Reads artifacts from /tmp/proc_falcon/, creates a fresh client, imports wallet B
//! and the NoteFile, syncs, then builds a Falcon signature and consumes the note.
//!
//! Run with:
//!   RUST_LOG=info cargo test --test proc_falcon_consume -- --ignored --nocapture

use std::sync::Arc;
use std::time::Duration;

use miden_client::auth::AuthSecretKey;
use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::{Client, Felt};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_client::rpc::{Endpoint, GrpcClient};
use miden_protocol::account::{Account, AccountId};
use miden_protocol::note::{Note, NoteFile};
use miden_protocol::utils::serde::Deserializable;
use miden_protocol::{Hasher, Word};

const INPUT_DIR: &str = "/tmp/proc_falcon";

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
#[ignore = "requires proc_falcon_create to have run first + testnet access"]
async fn proc_falcon_consume() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let input_dir = std::path::Path::new(INPUT_DIR);
    if !input_dir.exists() {
        anyhow::bail!(
            "Input directory {INPUT_DIR} does not exist. Run proc_falcon_create first."
        );
    }

    // ── Read setup.toml ──
    let setup_toml: toml::Value =
        toml::from_str(&std::fs::read_to_string(input_dir.join("setup.toml"))?)?;
    let wallet_b_id_hex = setup_toml["wallet_b_id_hex"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing wallet_b_id_hex"))?;
    let _faucet_id_hex = setup_toml["faucet_id_hex"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing faucet_id_hex"))?;
    let note_type_str = setup_toml
        .get("note_type")
        .and_then(|v| v.as_str())
        .unwrap_or("public");
    let wallet_b_id = AccountId::from_hex(wallet_b_id_hex)?;
    eprintln!("[consume] wallet B id: {}", wallet_b_id.to_hex());
    eprintln!("[consume] note type:   {note_type_str}");

    // ── Read wallet B account ──
    let wallet_b_b64 = std::fs::read_to_string(input_dir.join("wallet_b.b64"))?;
    let wallet_b_bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        wallet_b_b64.trim(),
    )?;
    let wallet_b_account = Account::read_from_bytes(&wallet_b_bytes)?;
    assert_eq!(wallet_b_account.id(), wallet_b_id);
    eprintln!("[consume] wallet B loaded: {}", wallet_b_account.id().to_hex());

    // ── Read NoteFile (.mno) ──
    let note_file_bytes = std::fs::read(input_dir.join("note.mno"))?;
    let note_file = NoteFile::read_from_bytes(&note_file_bytes)?;
    let note: Note = match &note_file {
        NoteFile::NoteWithProof(n, _) => {
            eprintln!("[consume] loaded NoteFile::NoteWithProof (includes inclusion proof)");
            n.clone()
        }
        NoteFile::NoteDetails { details: _, .. } => {
            eprintln!("[consume] loaded NoteFile::NoteDetails (no inclusion proof)");
            // We need the full Note for input_notes; reconstruct is not possible
            // from NoteDetails alone (no metadata). We'll import and consume via
            // get_consumable_notes instead.
            anyhow::bail!(
                "NoteDetails import path: the consume process needs to use \
                 import_notes + get_consumable_notes (see below). \
                 For now, NoteWithProof is the expected path."
            );
        }
        NoteFile::NoteId(_) => {
            anyhow::bail!("NoteFile::NoteId cannot be consumed directly");
        }
    };
    let note_id = note.id();
    eprintln!("[consume] note id: {note_id}");
    eprintln!("[consume]   script_root: {:?}", note.recipient().script().root());
    eprintln!("[consume]   storage items: {}", note.recipient().storage().num_items());
    eprintln!("[consume]   assets: {}", note.assets().num_assets());

    // ── Read agent_sk ──
    let agent_sk_bytes = std::fs::read(input_dir.join("agent_sk.bin"))?;
    let agent_sk = AuthSecretKey::read_from_bytes(&agent_sk_bytes)?;
    let agent_pk: Word = agent_sk.public_key().to_commitment().into();
    eprintln!("[consume] agent_sk loaded, pk commitment: {agent_pk:?}");

    // ── Create fresh client ──
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

    // Import the NoteFile
    client.import_notes(&[note_file]).await?;
    eprintln!("[consume] note imported into client");

    // Sync to authenticate the note on-chain
    for attempt in 0..60 {
        client.sync_state().await?;
        if let Ok(Some(record)) = client.get_input_note(note_id).await {
            if record.is_authenticated() {
                eprintln!("[consume] note authenticated (attempt {attempt})");
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
            eprintln!("[consume] WARNING: note never authenticated after 60 attempts, trying anyway...");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    // ── Build Falcon signature for consumption ──
    let amount = 500u64;
    let note_args: Word = [Felt::new(amount), Felt::ZERO, Felt::ZERO, Felt::ZERO].into();
    let serial_num: Word = note.recipient().serial_num();
    let message: Word = Hasher::merge(&[serial_num.into(), note_args.into()]).into();
    let sig = agent_sk.sign(message);
    let prepared: Vec<Felt> = sig.to_prepared_signature(message);
    let sig_key: Word = Hasher::merge(&[agent_pk.into(), message.into()]).into();

    eprintln!("[consume] consuming Falcon P2ID note...");
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
            eprintln!("[consume] SUCCESS: Falcon P2ID note consumed cross-process! tx={tx_id}");
            eprintln!("[consume] ══════════════════════════════════════════════════════");
            Ok(())
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!("[consume] ══════════════════════════════════════════════════════");
            eprintln!("[consume] FAILED: {}", &err_str[..err_str.len().min(800)]);
            eprintln!("[consume] ══════════════════════════════════════════════════════");
            anyhow::bail!("Falcon P2ID cross-process consume failed: {e}");
        }
    }
}
