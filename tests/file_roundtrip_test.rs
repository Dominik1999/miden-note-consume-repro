//! File-roundtrip reproduction test for StackReadFailed.
//!
//! Tests whether writing a Note/Account to a temp file via to_bytes() and
//! reading them back via read_from_bytes() (simulating what setup-testnet does
//! with adn_note.b64) causes the StackReadFailed error on consumption.
//!
//! Client A: deploy faucet + wallet, mint, fund, create ADN note, submit via
//!           own_output_notes, WRITE note + facilitator account to temp files.
//! Client B: READ note + account from temp files, import, sync, consume with
//!           Falcon signature in advice map.
//!
//! Run: cargo test --test file_roundtrip_test -- --ignored --nocapture

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
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::{Client, Felt};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::account::Account;
use miden_protocol::asset::Asset;
use miden_protocol::note::{
    Note, NoteAssets, NoteDetails, NoteFile, NoteMetadata, NoteRecipient, NoteStorage, NoteTag,
};
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_protocol::{Hasher, Word};
use miden_standards::code_builder::CodeBuilder;
use rand::RngCore;

const ADN_MASM: &str = include_str!("../masm/agent_debit_note.masm");

async fn build_client(
    data_dir: &std::path::Path,
) -> anyhow::Result<(Client<FilesystemKeyStore>, Arc<FilesystemKeyStore>)> {
    let endpoint = Endpoint::try_from("https://rpc.testnet.miden.io")
        .map_err(|e| anyhow::anyhow!("endpoint: {e:?}"))?;
    let rpc = Arc::new(GrpcClient::new(&endpoint, 30_000));
    let keystore = Arc::new(
        FilesystemKeyStore::new(data_dir.join("keystore"))
            .map_err(|e| anyhow::anyhow!("keystore: {e:?}"))?,
    );
    let store = data_dir.join("store.sqlite3");
    let client = ClientBuilder::new()
        .rpc(rpc)
        .sqlite_store(store)
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

/// File-roundtrip test: write Note + Account to temp files, read back, consume.
/// This isolates whether the file serialization itself causes StackReadFailed.
#[tokio::test]
#[ignore = "requires network access to Miden testnet"]
async fn file_roundtrip_adn() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    // ═══════════════════════════════════════════════════════════════
    // CLIENT A: creates accounts + note, writes to files
    // ═══════════════════════════════════════════════════════════════
    let tmp_a = tempfile::tempdir()?;
    let (mut client_a, keystore_a) = build_client(tmp_a.path()).await?;
    client_a.sync_state().await?;
    eprintln!("[file-rt] step 0: client A synced");

    // Deploy faucet
    let faucet_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client_a.rng());
    let symbol = TokenSymbol::new("FTEST").map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let faucet = AccountBuilder::new(rand_seed(&mut client_a))
        .account_type(AccountType::FungibleFaucet)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            faucet_key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(
            BasicFungibleFaucet::new(symbol, 6, Felt::new(1_000_000_000))
                .map_err(|e| anyhow::anyhow!("{e:?}"))?,
        )
        .with_component(AuthControlled::allow_all())
        .build()
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    client_a.add_account(&faucet, false).await?;
    keystore_a
        .add_key(&faucet_key, faucet.id())
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let faucet_id = faucet.id();

    // Deploy the "agent" account (note creator)
    let agent_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client_a.rng());
    let agent = AccountBuilder::new(rand_seed(&mut client_a))
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            agent_key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(BasicWallet)
        .build()
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    client_a.add_account(&agent, false).await?;
    keystore_a
        .add_key(&agent_key, agent.id())
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let agent_id = agent.id();

    // Deploy facilitator account (will be exported to file for client B)
    let facilitator_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client_a.rng());
    let facilitator = AccountBuilder::new(rand_seed(&mut client_a))
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            facilitator_key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(BasicWallet)
        .build()
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    client_a.add_account(&facilitator, false).await?;
    keystore_a
        .add_key(&facilitator_key, facilitator.id())
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let facilitator_id = facilitator.id();

    // Deploy merchant
    let merchant_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client_a.rng());
    let merchant = AccountBuilder::new(rand_seed(&mut client_a))
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            merchant_key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(BasicWallet)
        .build()
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    client_a.add_account(&merchant, false).await?;
    keystore_a
        .add_key(&merchant_key, merchant.id())
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let merchant_id = merchant.id();

    client_a.sync_state().await?;
    eprintln!(
        "[file-rt] step 1: accounts deployed: agent={} facilitator={} merchant={}",
        agent_id.to_hex(),
        facilitator_id.to_hex(),
        merchant_id.to_hex()
    );

    // Mint to agent + consume
    let mint_req = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet_id, 10_000).map_err(|e| anyhow::anyhow!("{e:?}"))?,
            agent_id,
            NoteType::Public,
            client_a.rng(),
        )
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    client_a
        .submit_new_transaction(faucet_id, mint_req)
        .await?;

    for attempt in 0..60 {
        client_a.sync_state().await?;
        let consumable = client_a.get_consumable_notes(Some(agent_id)).await?;
        if !consumable.is_empty() {
            eprintln!(
                "[file-rt] step 2: found {} consumable mint notes (attempt {attempt})",
                consumable.len()
            );
            let notes: Vec<_> = consumable
                .into_iter()
                .map(|(n, _)| n.try_into())
                .collect::<Result<_, _>>()?;
            client_a
                .submit_new_transaction(
                    agent_id,
                    TransactionRequestBuilder::new()
                        .build_consume_notes(notes)
                        .map_err(|e| anyhow::anyhow!("{e:?}"))?,
                )
                .await?;
            break;
        }
        if attempt == 59 {
            anyhow::bail!("timed out waiting for consumable mint note");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    client_a.sync_state().await?;
    eprintln!("[file-rt] step 2: agent funded");

    // Create ADN note with 7 storage items
    let adn_agent_sk = AuthSecretKey::new_falcon512_poseidon2_with_rng(client_a.rng());
    let adn_agent_pk: Word = adn_agent_sk.public_key().to_commitment().into();
    let note_script = CodeBuilder::default().compile_note_script(ADN_MASM)?;

    let balance = 1000u64;
    let amount = 100u64;
    let asset =
        FungibleAsset::new(faucet_id, balance).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let storage = NoteStorage::new(vec![
        adn_agent_pk[0],
        adn_agent_pk[1],
        adn_agent_pk[2],
        adn_agent_pk[3],
        agent_id.suffix(),
        agent_id.prefix().as_felt(),
        Felt::new(1_000_000),
    ])?;

    let mut sb = [0u8; 32];
    client_a.rng().fill_bytes(&mut sb);
    let serial_num: Word = [
        Felt::new(u64::from_le_bytes(sb[0..8].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(sb[8..16].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(sb[16..24].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(sb[24..32].try_into().unwrap())),
    ]
    .into();

    let tag = NoteTag::with_account_target(facilitator_id);
    let metadata = NoteMetadata::new(agent_id, NoteType::Public).with_tag(tag);
    let vault = NoteAssets::new(vec![Asset::Fungible(asset)])?;
    let recipient = NoteRecipient::new(serial_num, note_script, storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();

    // Submit on-chain from agent
    client_a
        .submit_new_transaction(
            agent_id,
            TransactionRequestBuilder::new()
                .own_output_notes(vec![note.clone()])
                .build()
                .map_err(|e| anyhow::anyhow!("{e:?}"))?,
        )
        .await?;
    eprintln!(
        "[file-rt] step 3: ADN note submitted on-chain, id={note_id}"
    );

    tokio::time::sleep(Duration::from_secs(6)).await;
    client_a.sync_state().await?;

    // ═══════════════════════════════════════════════════════════════
    // WRITE to temp files (simulating setup-testnet's adn_note.b64)
    // ═══════════════════════════════════════════════════════════════
    let file_dir = tempfile::tempdir()?;

    let note_file_path = file_dir.path().join("adn_note.bin");
    let note_bytes = note.to_bytes();
    std::fs::write(&note_file_path, &note_bytes)?;
    eprintln!(
        "[file-rt] step 4: note written to {} ({} bytes)",
        note_file_path.display(),
        note_bytes.len()
    );

    let account_file_path = file_dir.path().join("facilitator_account.bin");
    let account_bytes = facilitator.to_bytes();
    std::fs::write(&account_file_path, &account_bytes)?;
    eprintln!(
        "[file-rt] step 4: account written to {} ({} bytes)",
        account_file_path.display(),
        account_bytes.len()
    );

    // ═══════════════════════════════════════════════════════════════
    // READ back from files
    // ═══════════════════════════════════════════════════════════════
    let note_bytes_from_file = std::fs::read(&note_file_path)?;
    let account_bytes_from_file = std::fs::read(&account_file_path)?;

    let deserialized_note = Note::read_from_bytes(&note_bytes_from_file)
        .map_err(|e| anyhow::anyhow!("note decode from file: {e}"))?;
    let deserialized_account = Account::read_from_bytes(&account_bytes_from_file)
        .map_err(|e| anyhow::anyhow!("account decode from file: {e}"))?;

    assert_eq!(
        deserialized_note.id(),
        note_id,
        "note ID must survive file roundtrip"
    );
    assert_eq!(
        deserialized_account.id(),
        facilitator_id,
        "account ID must survive file roundtrip"
    );
    eprintln!(
        "[file-rt] step 5: note and account deserialized from files OK"
    );

    // ═══════════════════════════════════════════════════════════════
    // CLIENT B: separate client, imports from file-read bytes
    // ═══════════════════════════════════════════════════════════════
    let tmp_b = tempfile::tempdir()?;
    let (mut client_b, keystore_b) = build_client(tmp_b.path()).await?;

    // Import facilitator account from bytes read from file
    client_b
        .add_account(&deserialized_account, false)
        .await?;
    keystore_b
        .add_key(&facilitator_key, facilitator_id)
        .await
        .map_err(|e| anyhow::anyhow!("keystore: {e:?}"))?;
    eprintln!(
        "[file-rt] step 6: client B set up with facilitator account from file"
    );

    // Import note from bytes read from file via NoteFile::NoteDetails
    let note_details = NoteDetails::new(
        deserialized_note.assets().clone(),
        deserialized_note.recipient().clone(),
    );
    client_b
        .import_notes(&[NoteFile::NoteDetails {
            details: note_details,
            after_block_num: 0u32.into(),
            tag: Some(deserialized_note.metadata().tag()),
        }])
        .await?;
    eprintln!("[file-rt] step 6: note imported into client B from file");

    // Sync client B
    client_b.sync_state().await?;
    eprintln!("[file-rt] step 7: client B synced");

    // Check note state
    match client_b.get_input_note(note_id).await {
        Ok(Some(record)) => {
            eprintln!("[file-rt]   note state: {:?}", record.state());
            eprintln!(
                "[file-rt]   note is_authenticated: {}",
                record.is_authenticated()
            );
        }
        Ok(None) => eprintln!("[file-rt]   NOTE NOT FOUND in client B store"),
        Err(e) => eprintln!("[file-rt]   get_input_note error: {e}"),
    }

    // Wait for note to become consumable
    eprintln!("[file-rt] step 7: waiting for note to become consumable...");
    for attempt in 0..60 {
        client_b.sync_state().await?;
        let consumable = client_b
            .get_consumable_notes(Some(facilitator_id))
            .await?;
        if consumable.iter().any(|(n, _)| n.id() == note_id) {
            eprintln!(
                "[file-rt]   note is consumable (attempt {attempt})"
            );
            break;
        }
        if attempt == 59 {
            eprintln!("[file-rt]   WARNING: note never became consumable, trying anyway...");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    // ═══════════════════════════════════════════════════════════════
    // CONSUME: build note_args + Falcon signature in advice map
    // ═══════════════════════════════════════════════════════════════
    let note_args: Word = [
        merchant_id.suffix(),
        merchant_id.prefix().as_felt(),
        Felt::new(amount),
        Felt::ZERO,
    ]
    .into();
    let message: Word =
        Hasher::merge(&[serial_num.into(), note_args.into()]).into();
    let sig = adn_agent_sk.sign(message);
    let prepared: Vec<Felt> = sig.to_prepared_signature(message);
    let sig_key: Word =
        Hasher::merge(&[adn_agent_pk.into(), message.into()]).into();

    eprintln!(
        "[file-rt] step 8: consuming from client B (facilitator, file-roundtripped data)..."
    );

    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(deserialized_note, Some(note_args))])
        .extend_advice_map([(sig_key, prepared.as_slice())])
        .build()
        .map_err(|e| anyhow::anyhow!("consume build: {e:?}"))?;

    match client_b
        .submit_new_transaction(facilitator_id, consume_req)
        .await
    {
        Ok(tx_id) => {
            eprintln!("[file-rt] SUCCESS: tx_id={tx_id}");
            eprintln!(
                "          File-roundtripped ADN note consumed successfully!"
            );
            eprintln!(
                "          File serialization does NOT cause the issue."
            );
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!(
                "[file-rt] FAILED: {}",
                &err_str[..err_str.len().min(500)]
            );
            if err_str.contains("StackReadFailed") {
                eprintln!("[file-rt] BUG REPRODUCED: StackReadFailed after file roundtrip!");
                eprintln!("          File serialization IS the cause (or part of it).");
                panic!("StackReadFailed reproduced via file roundtrip");
            } else {
                eprintln!("[file-rt] Different error (not StackReadFailed)");
                panic!(
                    "Unexpected error: {}",
                    &err_str[..err_str.len().min(300)]
                );
            }
        }
    }

    Ok(())
}
