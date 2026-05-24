//! Exact reproduction of the facilitator's consumption pattern.
//!
//! Reads the same adn_note.b64 and facilitator_account.b64 files that
//! the facilitator binary uses, creates a fresh miden-client, imports
//! the account + note, syncs, and tries to consume.
//!
//! Run: SETUP_DIR=/tmp/chain-finality-test3 cargo test --test facilitator_repro_test -- --ignored --nocapture

use std::sync::Arc;
use std::time::Duration;

use miden_client::asset::FungibleAsset;
use miden_client::builder::ClientBuilder;
use miden_client::keystore::{FilesystemKeyStore, Keystore};
use miden_client::note::NoteType;
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::{Client, Felt};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::account::{Account, AccountId};
use miden_protocol::note::{Note, NoteDetails, NoteFile};
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
    let merchant_id_hex = setup_toml["merchant_id_hex"].as_str().unwrap();
    let merchant_id = AccountId::from_hex(merchant_id_hex)?;
    let faucet_id_hex = setup_toml["faucet_id_hex"].as_str().unwrap();
    let faucet_id = AccountId::from_hex(faucet_id_hex)?;
    eprintln!("facilitator={facilitator_id_hex} merchant={merchant_id_hex} faucet={faucet_id_hex}");

    // Read facilitator account + ADN note from snapshots
    let fac_bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        std::fs::read_to_string(setup_dir.join("facilitator_account.b64"))?.trim())?;
    let fac_account = Account::read_from_bytes(&fac_bytes)?;

    let note_bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        std::fs::read_to_string(setup_dir.join("adn_note.b64"))?.trim())?;
    let note = Note::read_from_bytes(&note_bytes)?;
    let note_id = note.id();
    eprintln!("note id={note_id}, storage={}, assets={}",
        note.recipient().storage().num_items(), note.assets().num_assets());

    // Create client with keystore from setup
    let tmp = tempfile::tempdir()?;
    let data_dir = tmp.path();
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

    let mut client = ClientBuilder::new()
        .rpc(rpc)
        .sqlite_store(data_dir.join("store.sqlite3"))
        .authenticator(keystore.clone())
        .in_debug_mode(true.into())
        .build()
        .await
        .map_err(|e| anyhow::anyhow!("client build: {e}"))?;

    // Import facilitator account
    client.add_account(&fac_account, false).await?;
    eprintln!("facilitator account imported");

    // Import ADN note with tag
    let tag = note.metadata().tag();
    let note_details = NoteDetails::new(note.assets().clone(), note.recipient().clone());
    client.import_notes(&[NoteFile::NoteDetails {
        details: note_details,
        after_block_num: 0u32.into(),
        tag: Some(tag),
    }]).await?;

    // Initial sync
    let sync = client.sync_state().await?;
    eprintln!("synced to block {}", sync.block_num);

    // Check note state
    match client.get_input_note(note_id).await {
        Ok(Some(record)) => eprintln!("note is_authenticated={}", record.is_authenticated()),
        Ok(None) => eprintln!("note NOT FOUND"),
        Err(e) => eprintln!("get_input_note error: {e}"),
    }

    // ── Deploy facilitator by funding it (mint + consume) ──
    let should_fund = std::env::var("FUND_FACILITATOR").is_ok();
    if should_fund {
        eprintln!("FUND_FACILITATOR set: minting to facilitator to deploy it on-chain...");

        // We need the faucet account too — import it from setup
        let faucet_bytes = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            // The faucet was created by setup-testnet's client, not exported as b64.
            // We can't mint from here without the faucet account.
            // Instead, use a noop tx to deploy the facilitator.
            "".as_bytes()
        );

        // Alternative: just do a noop transaction to deploy the facilitator on-chain
        eprintln!("submitting noop tx to deploy facilitator on-chain...");
        let noop_req = TransactionRequestBuilder::new()
            .build()
            .map_err(|e| anyhow::anyhow!("noop build: {e:?}"))?;
        match client.submit_new_transaction(facilitator_id, noop_req).await {
            Ok(tx_id) => eprintln!("noop tx succeeded: {tx_id}"),
            Err(e) => eprintln!("noop tx failed (expected if account not funded): {e}"),
        }

        // Wait 3 blocks
        eprintln!("waiting for 3 blocks...");
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_secs(3)).await;
            client.sync_state().await?;
        }
        eprintln!("waited, now re-synced");
    } else {
        eprintln!("FUND_FACILITATOR not set — skipping on-chain deployment");
    }

    // ── Try to consume the ADN note ──
    let amount = 100u64;
    let note_args: Word = [
        merchant_id.suffix(), merchant_id.prefix().as_felt(),
        Felt::new(amount), Felt::ZERO,
    ].into();

    eprintln!("attempting consume...");

    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(note, Some(note_args))])
        .build()
        .map_err(|e| anyhow::anyhow!("build: {e:?}"))?;

    match client.execute_transaction(facilitator_id, consume_req).await {
        Ok(result) => {
            eprintln!("SUCCESS: {} output notes", result.executed_transaction().output_notes().num_notes());
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!("FAILED: {}", &err_str[..err_str.len().min(300)]);
            if err_str.contains("StackReadFailed") {
                eprintln!("BUG: StackReadFailed");
            }
        }
    }

    Ok(())
}
