//! Exact reproduction of the facilitator's consumption pattern.
//!
//! Reads the same adn_note.b64 and facilitator_account.b64 files that
//! the facilitator binary uses, creates a fresh miden-client, imports
//! the account + note, syncs, and tries to consume.
//!
//! Run: SETUP_DIR=/tmp/chain-finality-test2 cargo test --test facilitator_repro_test -- --ignored --nocapture

use std::sync::Arc;

use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::{Client, Felt};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::account::{Account, AccountId};
use miden_protocol::note::{Note, NoteDetails, NoteFile, NoteTag};
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_protocol::{Hasher, Word};
use miden_client::rpc::{Endpoint, GrpcClient};

#[tokio::test]
#[ignore = "requires SETUP_DIR env var pointing to setup-testnet output"]
async fn facilitator_consume_from_files() -> anyhow::Result<()> {
    let setup_dir = std::env::var("SETUP_DIR")
        .map_err(|_| anyhow::anyhow!("set SETUP_DIR to the setup-testnet output dir"))?;
    let setup_dir = std::path::PathBuf::from(setup_dir);

    // Read setup.toml
    let setup_toml: toml::Value = toml::from_str(
        &std::fs::read_to_string(setup_dir.join("setup.toml"))?
    )?;
    let facilitator_id_hex = setup_toml["facilitator_account_id_hex"].as_str().unwrap();
    let facilitator_id = AccountId::from_hex(facilitator_id_hex)?;
    eprintln!("facilitator: {facilitator_id_hex}");

    let merchant_id_hex = setup_toml["merchant_id_hex"].as_str().unwrap();
    let merchant_id = AccountId::from_hex(merchant_id_hex)?;

    // Read facilitator account from snapshot
    let fac_b64 = std::fs::read_to_string(setup_dir.join("facilitator_account.b64"))?;
    let fac_bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD, fac_b64.trim())?;
    let fac_account = Account::read_from_bytes(&fac_bytes)?;
    eprintln!("facilitator account: {} bytes, id={}", fac_bytes.len(), fac_account.id().to_hex());

    // Read ADN note from snapshot
    let note_b64 = std::fs::read_to_string(setup_dir.join("adn_note.b64"))?;
    let note_bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD, note_b64.trim())?;
    let note = Note::read_from_bytes(&note_bytes)?;
    eprintln!("ADN note: {} bytes, id={}, storage={}, assets={}",
        note_bytes.len(), note.id(), note.recipient().storage().num_items(), note.assets().num_assets());

    // Create a fresh client
    let tmp = tempfile::tempdir()?;
    let data_dir = tmp.path();

    // Copy keystore from setup
    let keystore_dir = data_dir.join("keystore");
    std::fs::create_dir_all(&keystore_dir)?;
    for entry in std::fs::read_dir(setup_dir.join("keystore"))? {
        let entry = entry?;
        std::fs::copy(entry.path(), keystore_dir.join(entry.file_name()))?;
    }

    let endpoint = Endpoint::try_from("https://rpc.testnet.miden.io")
        .map_err(|e| anyhow::anyhow!("endpoint: {e:?}"))?;
    let rpc = Arc::new(GrpcClient::new(&endpoint, 30_000));
    let keystore = Arc::new(FilesystemKeyStore::new(keystore_dir)
        .map_err(|e| anyhow::anyhow!("keystore: {e:?}"))?);
    let store_path = data_dir.join("store.sqlite3");

    let mut client = ClientBuilder::new()
        .rpc(rpc)
        .sqlite_store(store_path)
        .authenticator(keystore)
        .in_debug_mode(true.into())
        .build()
        .await
        .map_err(|e| anyhow::anyhow!("client build: {e}"))?;

    // Import facilitator account
    client.add_account(&fac_account, false).await?;
    eprintln!("facilitator account imported");

    // Import ADN note (with tag)
    let tag = note.metadata().tag();
    let note_details = NoteDetails::new(note.assets().clone(), note.recipient().clone());
    client.import_notes(&[NoteFile::NoteDetails {
        details: note_details,
        after_block_num: 0u32.into(),
        tag: Some(tag),
    }]).await?;
    eprintln!("ADN note imported with tag={tag:?}");

    // Sync
    let sync = client.sync_state().await?;
    eprintln!("synced to block {}", sync.block_num);

    // Check consumable
    let consumable = client.get_consumable_notes(Some(facilitator_id)).await?;
    eprintln!("consumable notes for facilitator: {}", consumable.len());
    for (n, relevance) in &consumable {
        eprintln!("  note {} relevance={relevance:?}", n.id());
    }

    // Build note_args: [merchant_suffix, merchant_prefix, amount=100, 0]
    let amount = 100u64;
    let note_args: Word = [
        merchant_id.suffix(), merchant_id.prefix().as_felt(),
        Felt::new(amount), Felt::ZERO,
    ].into();

    let serial_num = note.recipient().serial_num();
    let message: Word = Hasher::merge(&[serial_num.into(), note_args.into()]).into();

    // Get agent PK from note storage
    let storage = note.recipient().storage().to_elements();
    let agent_pk: Word = [storage[0], storage[1], storage[2], storage[3]].into();

    // We don't have the agent's secret key here, so we can't sign.
    // But we can test if the note is at least executable WITHOUT the sig
    // (should fail at falcon verify, NOT at StackReadFailed)

    eprintln!("attempting consume WITHOUT signature (should fail at verify, not StackReadFailed)...");

    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(note, Some(note_args))])
        .build()
        .map_err(|e| anyhow::anyhow!("build: {e:?}"))?;

    match client.submit_new_transaction(facilitator_id, consume_req).await {
        Ok(tx_id) => {
            eprintln!("UNEXPECTED SUCCESS: {tx_id}");
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!("FAILED: {}", &err_str[..err_str.len().min(300)]);
            if err_str.contains("StackReadFailed") {
                eprintln!("BUG REPRODUCED: StackReadFailed from facilitator pattern");
            } else {
                eprintln!("Different error (expected — no signature provided)");
            }
        }
    }

    Ok(())
}
