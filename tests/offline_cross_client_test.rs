//! Offline cross-client reproduction of StackReadFailed.
//!
//! No testnet needed — uses MockRpcApi backed by MockChain.
//!
//! Client A: creates accounts, mints, creates the ADN note, submits it.
//! Client B: separate client with its own store, imports the note from
//!           serialized bytes, and tries to consume it.
//!
//! Run: cargo test --test offline_cross_client_test -- --nocapture

use std::collections::BTreeMap;
use std::sync::Arc;

use miden_client::account::component::{AuthControlled, BasicFungibleFaucet, BasicWallet};
use miden_client::account::{AccountBuilder, AccountStorageMode, AccountType};
use miden_client::asset::{FungibleAsset, TokenSymbol};
use miden_client::auth::{AuthSchemeId, AuthSecretKey, AuthSingleSig};
use miden_client::builder::ClientBuilder;
use miden_client::keystore::{FilesystemKeyStore, Keystore};
use miden_client::note::NoteType;
use miden_client::testing::mock::MockRpcApi;
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::Felt;
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::account::Account;
use miden_protocol::asset::Asset;
use miden_protocol::note::*;
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_protocol::vm::AdviceInputs;
use miden_protocol::{Hasher, Word};
use miden_standards::code_builder::CodeBuilder;
use miden_testing::MockChain;
use rand::RngCore;

const ADN_MASM: &str = include_str!("../masm/agent_debit_note.masm");

fn temp_store_path() -> std::path::PathBuf {
    let dir = tempfile::tempdir().unwrap().into_path();
    dir.join("store.sqlite3")
}

fn temp_keystore() -> (FilesystemKeyStore, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap().into_path();
    let ks = FilesystemKeyStore::new(dir.clone()).unwrap();
    (ks, dir)
}

/// Create a Client backed by the given MockRpcApi (offline, no testnet).
async fn create_mock_client(
    rpc: MockRpcApi,
) -> anyhow::Result<(miden_client::Client<FilesystemKeyStore>, MockRpcApi, FilesystemKeyStore)> {
    let (keystore, _dir) = temp_keystore();
    let rpc_clone = rpc.clone();
    let mut client = ClientBuilder::new()
        .rpc(Arc::new(rpc))
        .sqlite_store(temp_store_path())
        .authenticator(Arc::new(keystore.clone()))
        .in_debug_mode(true.into())
        .build()
        .await
        .map_err(|e| anyhow::anyhow!("client build: {e}"))?;

    // Ensure genesis block is in the store
    client.ensure_genesis_in_place().await?;

    Ok((client, rpc_clone, keystore))
}

/// Offline cross-client test:
/// Client A creates ADN note → serialize → Client B deserializes + consumes.
/// Both clients use the SAME MockRpcApi (shared MockChain) but separate stores.
#[tokio::test]
async fn offline_cross_client_adn() -> anyhow::Result<()> {
    // Build a shared MockChain
    let mock_chain = MockChain::new();
    let rpc = MockRpcApi::new(mock_chain);

    // ═══════════════════════════════════════════════════════════════
    // CLIENT A: creates accounts + note
    // ═══════════════════════════════════════════════════════════════
    let (mut client_a, rpc_a, keystore_a): (miden_client::Client<FilesystemKeyStore>, MockRpcApi, FilesystemKeyStore) = create_mock_client(rpc.clone()).await?;
    eprintln!("[offline] client A created");

    // Deploy faucet
    let faucet_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client_a.rng());
    let symbol = TokenSymbol::new("OTEST").map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let faucet = AccountBuilder::new({
        let mut s = [0u8; 32]; client_a.rng().fill_bytes(&mut s); s
    })
        .account_type(AccountType::FungibleFaucet)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            faucet_key.public_key().to_commitment().into(), AuthSchemeId::Falcon512Poseidon2))
        .with_component(BasicFungibleFaucet::new(symbol, 6, Felt::new(1_000_000_000))
            .map_err(|e| anyhow::anyhow!("{e:?}"))?)
        .with_component(AuthControlled::allow_all())
        .build().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    client_a.add_account(&faucet, false).await?;
    keystore_a.add_key(&faucet_key, faucet.id()).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let faucet_id = faucet.id();

    // Deploy agent (note creator)
    let agent_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client_a.rng());
    let agent = AccountBuilder::new({
        let mut s = [0u8; 32]; client_a.rng().fill_bytes(&mut s); s
    })
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            agent_key.public_key().to_commitment().into(), AuthSchemeId::Falcon512Poseidon2))
        .with_component(BasicWallet)
        .build().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    client_a.add_account(&agent, false).await?;
    keystore_a.add_key(&agent_key, agent.id()).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let agent_id = agent.id();

    // Deploy facilitator (note consumer — different account)
    let facilitator_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client_a.rng());
    let facilitator = AccountBuilder::new({
        let mut s = [0u8; 32]; client_a.rng().fill_bytes(&mut s); s
    })
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            facilitator_key.public_key().to_commitment().into(), AuthSchemeId::Falcon512Poseidon2))
        .with_component(BasicWallet)
        .build().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    client_a.add_account(&facilitator, false).await?;
    keystore_a.add_key(&facilitator_key, facilitator.id()).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let facilitator_id = facilitator.id();
    let facilitator_bytes = facilitator.to_bytes();

    // Deploy merchant
    let merchant_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client_a.rng());
    let merchant = AccountBuilder::new({
        let mut s = [0u8; 32]; client_a.rng().fill_bytes(&mut s); s
    })
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            merchant_key.public_key().to_commitment().into(), AuthSchemeId::Falcon512Poseidon2))
        .with_component(BasicWallet)
        .build().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    client_a.add_account(&merchant, false).await?;
    let merchant_id = merchant.id();

    // Sync + prove block to commit accounts
    rpc_a.prove_block();
    client_a.sync_state().await?;
    eprintln!("[offline] accounts deployed: agent={} facilitator={} merchant={}",
        agent_id.to_hex(), facilitator_id.to_hex(), merchant_id.to_hex());

    // Mint to agent
    let mint_asset = FungibleAsset::new(faucet_id, 10_000).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let mint_req = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(mint_asset, agent_id, NoteType::Public, client_a.rng())
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    client_a.submit_new_transaction(faucet_id, mint_req).await?;
    rpc_a.prove_block();
    client_a.sync_state().await?;

    // Consume mint note
    let consumable: Vec<_> = client_a.get_consumable_notes(Some(agent_id)).await?;
    assert!(!consumable.is_empty(), "agent should have consumable mint note");
    let notes: Vec<Note> = consumable.into_iter().map(|(n, _): (miden_client::store::InputNoteRecord, _)| -> anyhow::Result<Note> {
        let note: Note = n.try_into().map_err(|e: miden_client::store::NoteRecordError| anyhow::anyhow!("{e:?}"))?;
        Ok(note)
    }).collect::<anyhow::Result<_>>()?;
    client_a.submit_new_transaction(agent_id,
        TransactionRequestBuilder::new().build_consume_notes(notes).map_err(|e| anyhow::anyhow!("{e:?}"))?
    ).await?;
    rpc_a.prove_block();
    client_a.sync_state().await?;
    eprintln!("[offline] agent funded");

    // Create ADN note
    let adn_agent_sk = AuthSecretKey::new_falcon512_poseidon2_with_rng(client_a.rng());
    let adn_agent_pk: Word = adn_agent_sk.public_key().to_commitment().into();
    let note_script = CodeBuilder::default().compile_note_script(ADN_MASM)?;

    let balance = 1000u64;
    let amount = 100u64;
    let asset = FungibleAsset::new(faucet_id, balance).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let storage = NoteStorage::new(vec![
        adn_agent_pk[0], adn_agent_pk[1], adn_agent_pk[2], adn_agent_pk[3],
        agent_id.suffix(), agent_id.prefix().as_felt(),
        Felt::new(1_000_000),
    ])?;

    let mut sb = [0u8; 32];
    client_a.rng().fill_bytes(&mut sb);
    let serial_num: Word = [
        Felt::new(u64::from_le_bytes(sb[0..8].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(sb[8..16].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(sb[16..24].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(sb[24..32].try_into().unwrap())),
    ].into();

    let tag = NoteTag::with_account_target(facilitator_id);
    let metadata = NoteMetadata::new(agent_id, NoteType::Public).with_tag(tag);
    let vault = NoteAssets::new(vec![Asset::Fungible(asset)])?;
    let recipient = NoteRecipient::new(serial_num, note_script, storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();

    // Serialize the note (simulates saving to adn_note.b64)
    let note_bytes = note.to_bytes();

    // Submit on-chain from agent
    client_a.submit_new_transaction(agent_id,
        TransactionRequestBuilder::new().own_output_notes(vec![note.clone()]).build()
            .map_err(|e| anyhow::anyhow!("{e:?}"))?
    ).await?;
    rpc_a.prove_block();
    client_a.sync_state().await?;
    eprintln!("[offline] ADN note on-chain: {} ({} bytes serialized)", note_id, note_bytes.len());

    // Prove many extra blocks to simulate a long chain (like testnet at block ~978K)
    // This tests whether having many blocks in the partial blockchain affects execution
    let extra_blocks = std::env::var("EXTRA_BLOCKS").unwrap_or("0".into()).parse::<u32>().unwrap_or(0);
    if extra_blocks > 0 {
        eprintln!("[offline] proving {extra_blocks} extra blocks to simulate long chain...");
        for _ in 0..extra_blocks {
            rpc_a.prove_block();
        }
        client_a.sync_state().await?;
        eprintln!("[offline] chain now at block {}", rpc_a.get_chain_tip_block_num());
    }

    // ═══════════════════════════════════════════════════════════════
    // CLIENT B: separate client, same MockChain
    // ═══════════════════════════════════════════════════════════════
    let (mut client_b, _rpc_b, keystore_b): (miden_client::Client<FilesystemKeyStore>, MockRpcApi, FilesystemKeyStore) = create_mock_client(rpc.clone()).await?;

    // Import facilitator account from serialized bytes
    let fac_account = Account::read_from_bytes(&facilitator_bytes)?;
    client_b.add_account(&fac_account, false).await?;
    Keystore::add_key(&keystore_b, &facilitator_key, facilitator_id).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;

    // Deserialize the note from bytes (like the facilitator does)
    let deserialized_note = Note::read_from_bytes(&note_bytes)?;
    assert_eq!(deserialized_note.id(), note_id);

    // Import note via NoteFile::NoteDetails (like the facilitator does)
    let note_details = NoteDetails::new(
        deserialized_note.assets().clone(),
        deserialized_note.recipient().clone(),
    );
    client_b.import_notes(&[NoteFile::NoteDetails {
        details: note_details,
        after_block_num: 0u32.into(),
        tag: Some(deserialized_note.metadata().tag()),
    }]).await?;

    // Sync client B
    client_b.sync_state().await?;

    // Check note state
    match client_b.get_input_note(note_id).await {
        Ok(Some(record)) => {
            eprintln!("[offline] note state: {:?}", record.state());
            eprintln!("[offline] note is_authenticated: {}", record.is_authenticated());
        }
        Ok(None) => eprintln!("[offline] NOTE NOT FOUND"),
        Err(e) => eprintln!("[offline] get_input_note error: {e}"),
    }

    // Check consumable
    let consumable: Vec<_> = client_b.get_consumable_notes(Some(facilitator_id)).await?;
    eprintln!("[offline] consumable notes for facilitator: {}", consumable.len());

    // Build note_args + sign
    let note_args: Word = [
        merchant_id.suffix(), merchant_id.prefix().as_felt(),
        Felt::new(amount), Felt::ZERO,
    ].into();
    let message: Word = Hasher::merge(&[serial_num.into(), note_args.into()]).into();
    let sig = adn_agent_sk.sign(message);
    let prepared: Vec<Felt> = sig.to_prepared_signature(message);
    let sig_key: Word = Hasher::merge(&[adn_agent_pk.into(), message.into()]).into();

    eprintln!("[offline] consuming from client B (facilitator pattern)...");

    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(deserialized_note, Some(note_args))])
        .extend_advice_map([(sig_key, prepared.as_slice())])
        .build()
        .map_err(|e| anyhow::anyhow!("consume build: {e:?}"))?;

    match client_b.submit_new_transaction(facilitator_id, consume_req).await {
        Ok(tx_id) => {
            eprintln!("[offline] SUCCESS: tx_id={tx_id}");
            eprintln!("         Cross-client ADN consumption works offline!");
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!("[offline] FAILED: {}", &err_str[..err_str.len().min(300)]);
            if err_str.contains("StackReadFailed") {
                eprintln!("[offline] BUG REPRODUCED: StackReadFailed in offline cross-client test!");
                // Fail the test so CI catches it
                panic!("StackReadFailed reproduced offline");
            } else {
                eprintln!("[offline] Different error — not StackReadFailed");
                panic!("Unexpected error: {}", &err_str[..err_str.len().min(200)]);
            }
        }
    }

    Ok(())
}
