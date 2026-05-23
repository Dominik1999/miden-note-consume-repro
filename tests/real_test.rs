//! Real miden-client test for the secret-hash custom note.
//!
//! This test connects to the Miden testnet and attempts to consume a custom
//! note with the same script that works perfectly in MockChain. It reproduces
//! the `StackReadFailed` error.
//!
//! Run with: `cargo test real_test -- --ignored --nocapture`
//!
//! Prerequisites:
//! - Network access to https://rpc.testnet.miden.io
//! - Sufficient time (deploys accounts, waits for block inclusion)

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
use miden_protocol::note::{
    Note, NoteAssets, NoteDetails, NoteFile, NoteMetadata, NoteRecipient, NoteStorage, NoteTag,
};
use miden_protocol::{Hasher, Word};
use miden_protocol::asset::Asset;
use miden_standards::code_builder::CodeBuilder;
use rand::RngCore;

const NOTE_MASM: &str = include_str!("../masm/secret_hash_note.masm");
const FALCON_P2ID_MASM: &str = include_str!("../masm/falcon_p2id_note.masm");
const FALCON_P2ID_HASMAPKEY_MASM: &str = include_str!("../masm/falcon_p2id_hasmapkey_note.masm");

/// The secret that the note consumer must know.
fn secret() -> Word {
    [Felt::new(42), Felt::new(43), Felt::new(44), Felt::new(45)].into()
}

/// The digest of the secret, stored in the note's storage.
fn secret_digest() -> Word {
    let s: [Felt; 4] = secret().into();
    Hasher::hash_elements(&s)
}

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

/// This test reproduces the StackReadFailed bug.
///
/// Steps:
/// 1. Deploy a faucet account
/// 2. Deploy a consumer wallet account
/// 3. Mint tokens via the faucet
/// 4. Consume the minted note so the consumer has tokens
/// 5. Create a custom note with the secret hash script
/// 6. Submit the custom note on-chain (via own_output_notes)
/// 7. Sync and wait for the note to appear
/// 8. Try to consume the custom note — this fails with StackReadFailed
#[tokio::test]
#[ignore = "requires network access to Miden testnet"]
async fn real_consume_secret_hash_note() -> anyhow::Result<()> {
    // Use a temp directory for client state
    let tmp = tempfile::tempdir()?;
    let data_dir = tmp.path();
    let (mut client, keystore) = build_client(data_dir).await?;

    client.sync_state().await?;
    eprintln!("step 0: client synced");

    // ── 1. Deploy faucet ──
    let faucet_seed = rand_seed(&mut client);
    let faucet_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let symbol = TokenSymbol::new("RTEST")
        .map_err(|e| anyhow::anyhow!("TokenSymbol: {e:?}"))?;
    let max_supply = Felt::new(1_000_000_000);
    let faucet = AccountBuilder::new(faucet_seed)
        .account_type(AccountType::FungibleFaucet)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            faucet_key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(
            BasicFungibleFaucet::new(symbol, 6, max_supply)
                .map_err(|e| anyhow::anyhow!("BasicFungibleFaucet: {e:?}"))?,
        )
        .with_component(AuthControlled::allow_all())
        .build()
        .map_err(|e| anyhow::anyhow!("faucet build: {e:?}"))?;
    client.add_account(&faucet, false).await?;
    keystore
        .add_key(&faucet_key, faucet.id())
        .await
        .map_err(|e| anyhow::anyhow!("faucet keystore: {e:?}"))?;
    let faucet_id = faucet.id();
    eprintln!("step 1: faucet deployed: {}", faucet_id.to_hex());

    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ── 2. Deploy consumer wallet ──
    let consumer_seed = rand_seed(&mut client);
    let consumer_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let consumer = AccountBuilder::new(consumer_seed)
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            consumer_key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(BasicWallet)
        .build()
        .map_err(|e| anyhow::anyhow!("consumer build: {e:?}"))?;
    client.add_account(&consumer, false).await?;
    keystore
        .add_key(&consumer_key, consumer.id())
        .await
        .map_err(|e| anyhow::anyhow!("consumer keystore: {e:?}"))?;
    let consumer_id = consumer.id();
    eprintln!("step 2: consumer wallet deployed: {}", consumer_id.to_hex());

    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ── 3. Mint tokens from faucet to consumer ──
    let mint_amount = 10_000u64;
    let mint_asset = FungibleAsset::new(faucet_id, mint_amount)
        .map_err(|e| anyhow::anyhow!("FungibleAsset: {e:?}"))?;
    let mint_req = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(mint_asset, consumer_id, NoteType::Public, client.rng())
        .map_err(|e| anyhow::anyhow!("mint tx build: {e:?}"))?;
    let mint_tx = client.submit_new_transaction(faucet_id, mint_req).await?;
    eprintln!("step 3: mint tx submitted: {mint_tx}");

    // ── 4. Wait for consumable, then consume the mint note ──
    eprintln!("step 4: waiting for mint note to become consumable...");
    for attempt in 0..40 {
        client.sync_state().await?;
        let consumable = client.get_consumable_notes(Some(consumer_id)).await?;
        if !consumable.is_empty() {
            eprintln!("step 4: found {} consumable notes, consuming...", consumable.len());
            let notes: Vec<_> = consumable
                .into_iter()
                .map(|(note, _)| note.try_into())
                .collect::<Result<_, _>>()?;
            let consume_req = TransactionRequestBuilder::new()
                .build_consume_notes(notes)
                .map_err(|e| anyhow::anyhow!("consume build: {e:?}"))?;
            let consume_tx = client
                .submit_new_transaction(consumer_id, consume_req)
                .await?;
            eprintln!("step 4: mint note consumed: {consume_tx}");
            break;
        }
        if attempt == 39 {
            anyhow::bail!("timed out waiting for consumable mint note");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ── 5. Compile the custom note script ──
    let note_script = CodeBuilder::default().compile_note_script(NOTE_MASM)?;
    eprintln!("step 5: custom note script compiled");

    // ── 6. Create and submit the custom note ──
    let asset = FungibleAsset::new(faucet_id, 1000)
        .map_err(|e| anyhow::anyhow!("asset: {e:?}"))?;
    let storage = NoteStorage::new(vec![
        secret_digest()[0],
        secret_digest()[1],
        secret_digest()[2],
        secret_digest()[3],
    ])?;

    let mut serial_bytes = [0u8; 32];
    client.rng().fill_bytes(&mut serial_bytes);
    let serial_num: Word = [
        Felt::new(u64::from_le_bytes(serial_bytes[0..8].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[8..16].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[16..24].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[24..32].try_into().unwrap())),
    ]
    .into();

    let metadata =
        NoteMetadata::new(consumer_id, NoteType::Public).with_tag(NoteTag::with_account_target(consumer_id));
    let vault = NoteAssets::new(vec![Asset::Fungible(asset)])?;
    let recipient = NoteRecipient::new(serial_num, note_script, storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();
    eprintln!("step 6: custom note built, id={note_id}");

    // Submit the note on-chain by creating a tx that outputs it
    let create_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![note.clone()])
        .build()
        .map_err(|e| anyhow::anyhow!("create note tx: {e:?}"))?;
    let create_tx = client
        .submit_new_transaction(consumer_id, create_req)
        .await?;
    eprintln!("step 6: custom note submitted on-chain: {create_tx}");

    // ── 7. Wait for the note to become consumable ──
    eprintln!("step 7: waiting for custom note to become consumable...");
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Import the note into the store so the client knows about it
    let note_details = NoteDetails::new(note.assets().clone(), note.recipient().clone());
    let note_file = NoteFile::NoteDetails {
        details: note_details,
        after_block_num: 0u32.into(),
        tag: Some(note.metadata().tag()),
    };
    client.import_notes(&[note_file]).await?;
    eprintln!("step 7: note imported into client store");

    for attempt in 0..40 {
        client.sync_state().await?;
        let consumable = client.get_consumable_notes(Some(consumer_id)).await?;
        let found = consumable.iter().any(|(n, _)| n.id() == note_id);
        if found {
            eprintln!("step 7: custom note is consumable (attempt {attempt})");
            break;
        }
        if attempt == 39 {
            eprintln!("WARNING: custom note never became consumable, trying anyway...");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    // ── 8. Try to consume the custom note — THIS SHOULD FAIL ──
    eprintln!("step 8: syncing before consume attempt...");
    client.sync_state().await?;

    eprintln!("step 8: attempting to consume custom note with secret as note_args...");
    eprintln!("        (this is where StackReadFailed occurs with real client)");

    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(note, Some(secret()))])
        .build()
        .map_err(|e| anyhow::anyhow!("consume custom note tx: {e:?}"))?;

    match client
        .submit_new_transaction(consumer_id, consume_req)
        .await
    {
        Ok(tx_id) => {
            eprintln!("step 8: UNEXPECTED SUCCESS: tx_id={tx_id}");
            eprintln!("        If this passes, the bug may have been fixed!");
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!("step 8: EXPECTED FAILURE:");
            eprintln!("        {err_str}");
            if err_str.contains("StackReadFailed") {
                eprintln!("        Confirmed: StackReadFailed error reproduced!");
                eprintln!("        This is the bug: MockChain works, real client fails.");
            } else {
                eprintln!("        Different error than StackReadFailed, but still fails.");
                eprintln!("        The note consumption with custom scripts is broken.");
            }
            // We expect this to fail — that's the point of this repro
            // Uncomment the next line if you want the test to actually fail:
            // return Err(anyhow::anyhow!("consume failed as expected: {e}"));
        }
    }

    eprintln!("\nRepro complete.");
    eprintln!("Bug: custom note script works in MockChain but fails with real miden-client.");
    eprintln!("Error: TransactionExecutorError(TransactionProgramExecutionFailed(AdviceError {{ err: StackReadFailed }}))");

    Ok(())
}

/// Verify that a noop transaction (no input notes) succeeds with the same
/// client setup. This proves the client connection and account setup are fine.
#[tokio::test]
#[ignore = "requires network access to Miden testnet"]
async fn real_noop_transaction_succeeds() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let data_dir = tmp.path();
    let (mut client, keystore) = build_client(data_dir).await?;

    client.sync_state().await?;

    let seed = rand_seed(&mut client);
    let key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let account = AccountBuilder::new(seed)
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(BasicWallet)
        .build()
        .map_err(|e| anyhow::anyhow!("build: {e:?}"))?;
    client.add_account(&account, false).await?;
    keystore
        .add_key(&key, account.id())
        .await
        .map_err(|e| anyhow::anyhow!("keystore: {e:?}"))?;

    client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Noop transaction: no input notes, no outputs
    let req = TransactionRequestBuilder::new()
        .build()
        .map_err(|e| anyhow::anyhow!("noop tx: {e:?}"))?;
    let tx_id = client
        .submit_new_transaction(account.id(), req)
        .await?;
    eprintln!("Noop transaction succeeded: {tx_id}");
    eprintln!("This proves the client setup is correct — only custom note consumption fails.");

    Ok(())
}

/// This test reproduces StackReadFailed with a Falcon-signed note that creates
/// a P2ID output note — the full pattern used in AgentDebitNote.
///
/// The simple secret_hash note (test above) PASSES on main. This one FAILS.
#[tokio::test]
#[ignore = "requires network access to Miden testnet"]
async fn real_falcon_p2id_note() -> anyhow::Result<()> {
    use miden_protocol::vm::AdviceInputs;

    let tmp = tempfile::tempdir()?;
    let data_dir = tmp.path();
    let (mut client, keystore) = build_client(data_dir).await?;

    client.sync_state().await?;
    eprintln!("step 0: client synced");

    // ── 1. Deploy faucet ──
    let faucet_seed = rand_seed(&mut client);
    let faucet_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let symbol = TokenSymbol::new("FTEST")
        .map_err(|e| anyhow::anyhow!("TokenSymbol: {e:?}"))?;
    let max_supply = Felt::new(1_000_000_000);
    let faucet = AccountBuilder::new(faucet_seed)
        .account_type(AccountType::FungibleFaucet)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            faucet_key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(
            BasicFungibleFaucet::new(symbol, 6, max_supply)
                .map_err(|e| anyhow::anyhow!("BasicFungibleFaucet: {e:?}"))?,
        )
        .with_component(AuthControlled::allow_all())
        .build()
        .map_err(|e| anyhow::anyhow!("faucet build: {e:?}"))?;
    client.add_account(&faucet, false).await?;
    keystore.add_key(&faucet_key, faucet.id()).await
        .map_err(|e| anyhow::anyhow!("faucet keystore: {e:?}"))?;
    let faucet_id = faucet.id();
    eprintln!("step 1: faucet deployed: {}", faucet_id.to_hex());

    client.sync_state().await?;

    // ── 2. Deploy consumer wallet ──
    let consumer_seed = rand_seed(&mut client);
    let consumer_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let consumer = AccountBuilder::new(consumer_seed)
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            consumer_key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(BasicWallet)
        .build()
        .map_err(|e| anyhow::anyhow!("consumer build: {e:?}"))?;
    client.add_account(&consumer, false).await?;
    keystore.add_key(&consumer_key, consumer.id()).await
        .map_err(|e| anyhow::anyhow!("consumer keystore: {e:?}"))?;
    let consumer_id = consumer.id();
    eprintln!("step 2: consumer wallet deployed: {}", consumer_id.to_hex());

    // ── 3. Deploy target wallet (P2ID recipient) ──
    let target_seed = rand_seed(&mut client);
    let target_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let target = AccountBuilder::new(target_seed)
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            target_key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(BasicWallet)
        .build()
        .map_err(|e| anyhow::anyhow!("target build: {e:?}"))?;
    client.add_account(&target, false).await?;
    keystore.add_key(&target_key, target.id()).await
        .map_err(|e| anyhow::anyhow!("target keystore: {e:?}"))?;
    let target_id = target.id();
    eprintln!("step 3: target wallet deployed: {}", target_id.to_hex());

    client.sync_state().await?;

    // ── 4. Mint tokens to consumer ──
    let mint_asset = FungibleAsset::new(faucet_id, 10_000)
        .map_err(|e| anyhow::anyhow!("FungibleAsset: {e:?}"))?;
    let mint_req = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(mint_asset, consumer_id, NoteType::Public, client.rng())
        .map_err(|e| anyhow::anyhow!("mint tx: {e:?}"))?;
    let mint_tx = client.submit_new_transaction(faucet_id, mint_req).await?;
    eprintln!("step 4: mint submitted: {mint_tx}");

    // ── 5. Wait + consume mint note ──
    eprintln!("step 5: waiting for mint note...");
    for attempt in 0..40 {
        client.sync_state().await?;
        let consumable = client.get_consumable_notes(Some(consumer_id)).await?;
        if !consumable.is_empty() {
            let notes: Vec<_> = consumable.into_iter()
                .map(|(note, _)| note.try_into())
                .collect::<Result<_, _>>()?;
            let consume_req = TransactionRequestBuilder::new()
                .build_consume_notes(notes)
                .map_err(|e| anyhow::anyhow!("consume: {e:?}"))?;
            let consume_tx = client.submit_new_transaction(consumer_id, consume_req).await?;
            eprintln!("step 5: mint consumed: {consume_tx}");
            break;
        }
        if attempt == 39 { anyhow::bail!("timeout waiting for mint note"); }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    client.sync_state().await?;

    // ── 6. Create the Falcon + P2ID note ──
    let agent_sk = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let agent_pk: Word = agent_sk.public_key().to_commitment().into();

    let note_script = CodeBuilder::default().compile_note_script(FALCON_P2ID_MASM)?;
    eprintln!("step 6: falcon+p2id note script compiled");

    let amount = 500u64;
    let asset = FungibleAsset::new(faucet_id, 1000)
        .map_err(|e| anyhow::anyhow!("asset: {e:?}"))?;

    // Storage: [agent_pk(4), target_suffix, target_prefix]
    let storage = NoteStorage::new(vec![
        agent_pk[0], agent_pk[1], agent_pk[2], agent_pk[3],
        target_id.suffix(), target_id.prefix().as_felt(),
    ])?;

    let mut serial_bytes = [0u8; 32];
    client.rng().fill_bytes(&mut serial_bytes);
    let serial_num: Word = [
        Felt::new(u64::from_le_bytes(serial_bytes[0..8].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[8..16].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[16..24].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[24..32].try_into().unwrap())),
    ].into();

    let tag = NoteTag::with_account_target(consumer_id);
    let metadata = NoteMetadata::new(consumer_id, NoteType::Public).with_tag(tag);
    let vault = NoteAssets::new(vec![Asset::Fungible(asset)])?;
    let recipient = NoteRecipient::new(serial_num, note_script, storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();
    eprintln!("step 6: note built, id={note_id}");

    // Submit on-chain
    let create_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![note.clone()])
        .build()
        .map_err(|e| anyhow::anyhow!("create note tx: {e:?}"))?;
    let create_tx = client.submit_new_transaction(consumer_id, create_req).await?;
    eprintln!("step 6: note submitted: {create_tx}");

    // ── 7. Wait for note to be on-chain ──
    tokio::time::sleep(Duration::from_secs(5)).await;
    let note_details = NoteDetails::new(note.assets().clone(), note.recipient().clone());
    let note_file = NoteFile::NoteDetails {
        details: note_details,
        after_block_num: 0u32.into(),
        tag: Some(tag),
    };
    client.import_notes(&[note_file]).await?;

    eprintln!("step 7: waiting for note to sync...");
    for attempt in 0..40 {
        client.sync_state().await?;
        let consumable = client.get_consumable_notes(Some(consumer_id)).await?;
        if consumable.iter().any(|(n, _)| n.id() == note_id) {
            eprintln!("step 7: note is consumable (attempt {attempt})");
            break;
        }
        if attempt == 39 { eprintln!("WARNING: note never became consumable, trying anyway..."); }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    // ── 8. Consume with Falcon signature + advice map ──
    eprintln!("step 8: syncing before consume...");
    client.sync_state().await?;

    // Compute message = merge(serial_num, [amount, 0, 0, 0])
    let note_args: Word = [Felt::new(amount), Felt::ZERO, Felt::ZERO, Felt::ZERO].into();
    let message: Word = Hasher::merge(&[serial_num.into(), note_args.into()]).into();

    // Sign + put in advice map at key = merge(AGENT_PK, MESSAGE)
    let sig = agent_sk.sign(message);
    let prepared: Vec<Felt> = sig.to_prepared_signature(message);
    let sig_key: Word = Hasher::merge(&[agent_pk.into(), message.into()]).into();

    eprintln!("step 8: attempting consume with Falcon sig in advice map...");
    eprintln!("        (this is where StackReadFailed occurs)");

    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(note, Some(note_args))])
        .extend_advice_map([(sig_key, prepared.as_slice())])
        .build()
        .map_err(|e| anyhow::anyhow!("consume tx: {e:?}"))?;

    match client.submit_new_transaction(consumer_id, consume_req).await {
        Ok(tx_id) => {
            eprintln!("step 8: SUCCESS! tx_id={tx_id}");
            eprintln!("        The bug is fixed!");
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!("step 8: FAILED:");
            eprintln!("        {err_str}");
            if err_str.contains("StackReadFailed") {
                eprintln!("        Confirmed: StackReadFailed reproduced!");
                eprintln!("        Falcon sig + P2ID output note pattern fails on real client.");
            }
        }
    }

    Ok(())
}

/// Same as real_falcon_p2id_note but uses adv.has_mapkey to check for the
/// signature before reading it. This matches the AgentDebitNote pattern.
/// If real_falcon_p2id_note passes but this fails, adv.has_mapkey is the culprit.
#[tokio::test]
#[ignore = "requires network access to Miden testnet"]
async fn real_falcon_p2id_hasmapkey_note() -> anyhow::Result<()> {
    use miden_protocol::vm::AdviceInputs;

    let tmp = tempfile::tempdir()?;
    let data_dir = tmp.path();
    let (mut client, keystore) = build_client(data_dir).await?;
    client.sync_state().await?;
    eprintln!("[hasmapkey] step 0: synced");

    // Deploy faucet
    let faucet_seed = rand_seed(&mut client);
    let faucet_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let symbol = TokenSymbol::new("HMTEST").map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let faucet = AccountBuilder::new(faucet_seed)
        .account_type(AccountType::FungibleFaucet)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            faucet_key.public_key().to_commitment().into(), AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(BasicFungibleFaucet::new(symbol, 6, Felt::new(1_000_000_000)).map_err(|e| anyhow::anyhow!("{e:?}"))?)
        .with_component(AuthControlled::allow_all())
        .build().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    client.add_account(&faucet, false).await?;
    keystore.add_key(&faucet_key, faucet.id()).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let faucet_id = faucet.id();
    eprintln!("[hasmapkey] step 1: faucet {}", faucet_id.to_hex());

    // Deploy consumer + target
    let consumer_seed = rand_seed(&mut client);
    let consumer_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let consumer = AccountBuilder::new(consumer_seed)
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            consumer_key.public_key().to_commitment().into(), AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(BasicWallet)
        .build().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    client.add_account(&consumer, false).await?;
    keystore.add_key(&consumer_key, consumer.id()).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let consumer_id = consumer.id();

    let target_seed = rand_seed(&mut client);
    let target_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let target = AccountBuilder::new(target_seed)
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            target_key.public_key().to_commitment().into(), AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(BasicWallet)
        .build().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    client.add_account(&target, false).await?;
    keystore.add_key(&target_key, target.id()).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let target_id = target.id();
    client.sync_state().await?;
    eprintln!("[hasmapkey] step 2: consumer={} target={}", consumer_id.to_hex(), target_id.to_hex());

    // Mint + consume
    let mint_asset = FungibleAsset::new(faucet_id, 10_000).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let mint_req = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(mint_asset, consumer_id, NoteType::Public, client.rng())
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    client.submit_new_transaction(faucet_id, mint_req).await?;
    eprintln!("[hasmapkey] step 3: mint submitted");

    for attempt in 0..40 {
        client.sync_state().await?;
        let consumable = client.get_consumable_notes(Some(consumer_id)).await?;
        if !consumable.is_empty() {
            let notes: Vec<_> = consumable.into_iter().map(|(n, _)| n.try_into()).collect::<Result<_, _>>()?;
            let req = TransactionRequestBuilder::new().build_consume_notes(notes).map_err(|e| anyhow::anyhow!("{e:?}"))?;
            client.submit_new_transaction(consumer_id, req).await?;
            eprintln!("[hasmapkey] step 4: mint consumed");
            break;
        }
        if attempt == 39 { anyhow::bail!("timeout"); }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    client.sync_state().await?;

    // Create the hasmapkey note
    let agent_sk = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let agent_pk: Word = agent_sk.public_key().to_commitment().into();
    let note_script = CodeBuilder::default().compile_note_script(FALCON_P2ID_HASMAPKEY_MASM)?;

    let amount = 500u64;
    let asset = FungibleAsset::new(faucet_id, 1000).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let storage = NoteStorage::new(vec![
        agent_pk[0], agent_pk[1], agent_pk[2], agent_pk[3],
        target_id.suffix(), target_id.prefix().as_felt(),
    ])?;
    let mut serial_bytes = [0u8; 32];
    client.rng().fill_bytes(&mut serial_bytes);
    let serial_num: Word = [
        Felt::new(u64::from_le_bytes(serial_bytes[0..8].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[8..16].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[16..24].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[24..32].try_into().unwrap())),
    ].into();

    let tag = NoteTag::with_account_target(consumer_id);
    let metadata = NoteMetadata::new(consumer_id, NoteType::Public).with_tag(tag);
    let vault = NoteAssets::new(vec![Asset::Fungible(asset)])?;
    let recipient = NoteRecipient::new(serial_num, note_script, storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();

    let create_req = TransactionRequestBuilder::new().own_output_notes(vec![note.clone()]).build().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    client.submit_new_transaction(consumer_id, create_req).await?;
    eprintln!("[hasmapkey] step 5: note submitted on-chain, id={note_id}");

    // Import + wait
    tokio::time::sleep(Duration::from_secs(5)).await;
    let note_details = NoteDetails::new(note.assets().clone(), note.recipient().clone());
    client.import_notes(&[NoteFile::NoteDetails { details: note_details, after_block_num: 0u32.into(), tag: Some(tag) }]).await?;
    for attempt in 0..40 {
        client.sync_state().await?;
        let consumable = client.get_consumable_notes(Some(consumer_id)).await?;
        if consumable.iter().any(|(n, _)| n.id() == note_id) {
            eprintln!("[hasmapkey] step 6: note consumable (attempt {attempt})");
            break;
        }
        if attempt == 39 { eprintln!("WARNING: note never became consumable"); }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    // Consume with has_mapkey pattern
    client.sync_state().await?;
    let note_args: Word = [Felt::new(amount), Felt::ZERO, Felt::ZERO, Felt::ZERO].into();
    let message: Word = Hasher::merge(&[serial_num.into(), note_args.into()]).into();
    let sig = agent_sk.sign(message);
    let prepared: Vec<Felt> = sig.to_prepared_signature(message);
    let sig_key: Word = Hasher::merge(&[agent_pk.into(), message.into()]).into();

    eprintln!("[hasmapkey] step 7: consuming with adv.has_mapkey pattern...");

    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(note, Some(note_args))])
        .extend_advice_map([(sig_key, prepared.as_slice())])
        .build().map_err(|e| anyhow::anyhow!("{e:?}"))?;

    match client.submit_new_transaction(consumer_id, consume_req).await {
        Ok(tx_id) => eprintln!("[hasmapkey] step 7: SUCCESS! tx_id={tx_id}"),
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!("[hasmapkey] step 7: FAILED: {err_str}");
            if err_str.contains("StackReadFailed") {
                eprintln!("[hasmapkey] Confirmed: adv.has_mapkey pattern causes StackReadFailed!");
            }
        }
    }

    Ok(())
}
