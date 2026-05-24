//! Process 2: Consume an ADN note created by proc_adn_create from a separate
//! process.
//!
//! Reads artifacts from /tmp/proc_adn/ (created by proc_adn_create), creates a
//! fresh miden-client, imports wallet B + NoteFile, syncs, builds the Falcon
//! signature over (serial_num, note_args), and consumes the ADN note.
//!
//! Run with:
//!   RUST_LOG=info cargo test --test proc_adn_consume -- --ignored --nocapture

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
use miden_protocol::{Hasher, Word};

const INPUT_DIR: &str = "/tmp/proc_adn";

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
#[ignore = "requires proc_adn_create to have run first + testnet access"]
async fn proc_adn_consume() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let input_dir = std::path::Path::new(INPUT_DIR);
    if !input_dir.exists() {
        anyhow::bail!(
            "Input directory {INPUT_DIR} does not exist. Run proc_adn_create first."
        );
    }

    // ── Read setup.toml ──
    let setup_toml: toml::Value =
        toml::from_str(&std::fs::read_to_string(input_dir.join("setup.toml"))?)?;

    let wallet_b_id_hex = setup_toml["wallet_b_id_hex"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing wallet_b_id_hex in setup.toml"))?;
    let wallet_b_id = AccountId::from_hex(wallet_b_id_hex)?;
    eprintln!("[consume] wallet B id: {}", wallet_b_id.to_hex());

    // Parse serial_num from setup.toml: ["0x...", "0x...", "0x...", "0x..."]
    let serial_arr = setup_toml["serial_num_hex"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing serial_num_hex array in setup.toml"))?;
    if serial_arr.len() != 4 {
        anyhow::bail!(
            "serial_num_hex must have 4 elements, got {}",
            serial_arr.len()
        );
    }
    let serial_num: Word = [
        Felt::new(parse_hex_felt(serial_arr[0].as_str().unwrap())?),
        Felt::new(parse_hex_felt(serial_arr[1].as_str().unwrap())?),
        Felt::new(parse_hex_felt(serial_arr[2].as_str().unwrap())?),
        Felt::new(parse_hex_felt(serial_arr[3].as_str().unwrap())?),
    ]
    .into();
    eprintln!("[consume] serial_num: {:?}", serial_num);

    // ── Read wallet B account ──
    let wallet_b_b64 = std::fs::read_to_string(input_dir.join("wallet_b.b64"))?;
    let wallet_b_bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        wallet_b_b64.trim(),
    )?;
    let wallet_b_account = Account::read_from_bytes(&wallet_b_bytes)?;
    assert_eq!(
        wallet_b_account.id(),
        wallet_b_id,
        "wallet B id mismatch"
    );
    eprintln!(
        "[consume] wallet B account loaded: {}",
        wallet_b_account.id().to_hex()
    );

    // ── Read agent secret key ──
    let agent_sk_bytes = std::fs::read(input_dir.join("agent_sk.bin"))?;
    let agent_sk =
        miden_client::auth::AuthSecretKey::read_from_bytes(&agent_sk_bytes)
            .map_err(|e| anyhow::anyhow!("agent_sk decode: {e}"))?;
    let agent_pk: Word = agent_sk.public_key().to_commitment().into();
    eprintln!("[consume] agent_pk: {:?}", agent_pk);

    // ── Read NoteFile (.mno) ──
    let note_mno_b64 = std::fs::read_to_string(input_dir.join("note.mno"))?;
    let note_mno_bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        note_mno_b64.trim(),
    )?;
    let note_file = NoteFile::read_from_bytes(&note_mno_bytes)
        .map_err(|e| anyhow::anyhow!("NoteFile decode: {e}"))?;

    // Extract the Note from the NoteFile
    let note: Note = match &note_file {
        NoteFile::NoteWithProof(n, _) => {
            eprintln!("[consume] loaded NoteFile::NoteWithProof (has inclusion proof)");
            n.clone()
        }
        NoteFile::NoteDetails { .. } => {
            eprintln!("[consume] loaded NoteFile::NoteDetails (no inclusion proof)");
            // We still need the Note for consumption; reconstruct minimal info
            // but NoteDetails doesn't have metadata, so we have to import differently
            // For now just import the NoteFile directly and get note from store later
            // We'll handle this below
            eprintln!("[consume] WARNING: NoteDetails path -- will import directly");
            // Can't easily get a Note from NoteDetails alone (no metadata).
            // We'll import the NoteFile and then get the note from the store.
            // Use a placeholder; the actual consumption will use store notes.
            anyhow::bail!(
                "NoteDetails import not fully supported in this test; \
                 re-run proc_adn_create and wait for inclusion proof"
            );
        }
        NoteFile::NoteId(_) => anyhow::bail!("NoteFile::NoteId is not usable for consumption"),
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
    eprintln!("[consume] NoteFile imported into client");

    // Wait for note to become authenticated / consumable
    eprintln!("[consume] waiting for note to become consumable...");
    let mut found = false;
    for attempt in 0..60 {
        client.sync_state().await?;

        // Check authentication state
        if let Ok(Some(record)) = client.get_input_note(note_id).await {
            if record.is_authenticated() && !found {
                eprintln!("[consume] note authenticated (attempt {attempt})");
            }
        }

        let consumable = client.get_consumable_notes(Some(wallet_b_id)).await?;
        if consumable.iter().any(|(n, _)| n.id() == note_id) {
            eprintln!("[consume] note is consumable (attempt {attempt})");
            found = true;
            break;
        }
        if attempt == 59 {
            eprintln!(
                "[consume] WARNING: note never became consumable after 60 attempts"
            );
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    if !found {
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

    // ── Build note_args + Falcon signature ──
    let amount = 100u64;
    let note_args: Word = [
        wallet_b_id.suffix(),
        wallet_b_id.prefix().as_felt(),
        Felt::new(amount),
        Felt::ZERO,
    ]
    .into();

    let message: Word = Hasher::merge(&[serial_num.into(), note_args.into()]).into();
    let sig = agent_sk.sign(message);
    let prepared: Vec<Felt> = sig.to_prepared_signature(message);
    let sig_key: Word = Hasher::merge(&[agent_pk.into(), message.into()]).into();

    eprintln!("[consume] consuming ADN note...");
    eprintln!("[consume]   note_args: {:?}", note_args);
    eprintln!("[consume]   sig_key: {:?}", sig_key);
    eprintln!("[consume]   prepared_sig len: {}", prepared.len());

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
            eprintln!("[consume] ========================================");
            eprintln!("[consume] SUCCESS: ADN note consumed! tx={tx_id}");
            eprintln!("[consume] ========================================");
            Ok(())
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!("[consume] ========================================");
            eprintln!(
                "[consume] FAILED: {}",
                &err_str[..err_str.len().min(800)]
            );
            eprintln!("[consume] ========================================");
            anyhow::bail!("ADN cross-process consume failed: {e}");
        }
    }
}

/// Parse a hex string like "0x1a2b" or "1a2b" into a u64.
fn parse_hex_felt(s: &str) -> anyhow::Result<u64> {
    let s = s.trim();
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    u64::from_str_radix(s, 16).map_err(|e| anyhow::anyhow!("parse hex felt '{s}': {e}"))
}
