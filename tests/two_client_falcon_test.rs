//! Two-client Falcon-512 signature verification test.
//!
//! Client A creates a custom note (falcon_p2id_note.masm) that requires a Falcon-512
//! signature to consume. Client B (separate store/keystore) imports the note and
//! consumes it by providing the Falcon signature in the advice map.
//!
//! Run with:
//!   RUST_LOG=miden_client::transaction=info cargo test --test two_client_falcon_test -- --ignored --nocapture

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
use miden_protocol::account::{Account, AccountId};
use miden_protocol::asset::Asset;
use miden_protocol::note::{
    Note, NoteAssets, NoteDetails, NoteFile, NoteMetadata, NoteRecipient, NoteStorage, NoteTag,
};
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_protocol::{Hasher, Word};
use miden_standards::code_builder::CodeBuilder;
use rand::RngCore;

const FALCON_P2ID_MASM: &str = include_str!("../masm/falcon_p2id_note.masm");

async fn build_client(
    data_dir: &std::path::Path,
) -> anyhow::Result<(Client<FilesystemKeyStore>, Arc<FilesystemKeyStore>)> {
    let endpoint = Endpoint::try_from("https://rpc.testnet.miden.io").unwrap();
    let rpc = Arc::new(GrpcClient::new(&endpoint, 30_000));
    let keystore = Arc::new(FilesystemKeyStore::new(data_dir.join("keystore")).unwrap());
    let client = ClientBuilder::new()
        .rpc(rpc)
        .sqlite_store(data_dir.join("store.sqlite3"))
        .authenticator(keystore.clone())
        .in_debug_mode(true.into())
        .build()
        .await
        .unwrap();
    Ok((client, keystore))
}

fn rand_seed(client: &mut Client<FilesystemKeyStore>) -> [u8; 32] {
    let mut seed = [0u8; 32];
    client.rng().fill_bytes(&mut seed);
    seed
}

async fn deploy_faucet(
    client: &mut Client<FilesystemKeyStore>,
    keystore: &Arc<FilesystemKeyStore>,
    symbol_str: &str,
) -> anyhow::Result<AccountId> {
    let seed = rand_seed(client);
    let key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let symbol =
        TokenSymbol::new(symbol_str).map_err(|e| anyhow::anyhow!("TokenSymbol: {e:?}"))?;
    let faucet = AccountBuilder::new(seed)
        .account_type(AccountType::FungibleFaucet)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(
            BasicFungibleFaucet::new(symbol, 6, Felt::new(1_000_000_000))
                .map_err(|e| anyhow::anyhow!("BasicFungibleFaucet: {e:?}"))?,
        )
        .with_component(AuthControlled::allow_all())
        .build()
        .map_err(|e| anyhow::anyhow!("faucet build: {e:?}"))?;
    let faucet_id = faucet.id();
    client.add_account(&faucet, false).await?;
    keystore
        .add_key(&key, faucet_id)
        .await
        .map_err(|e| anyhow::anyhow!("faucet keystore: {e:?}"))?;
    Ok(faucet_id)
}

async fn deploy_wallet(
    client: &mut Client<FilesystemKeyStore>,
    keystore: &Arc<FilesystemKeyStore>,
) -> anyhow::Result<(AccountId, AuthSecretKey, Vec<u8>)> {
    let seed = rand_seed(client);
    let key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let wallet = AccountBuilder::new(seed)
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(BasicWallet)
        .build()
        .map_err(|e| anyhow::anyhow!("wallet build: {e:?}"))?;
    let wallet_id = wallet.id();
    let wallet_bytes = wallet.to_bytes();
    client.add_account(&wallet, false).await?;
    keystore
        .add_key(&key, wallet_id)
        .await
        .map_err(|e| anyhow::anyhow!("wallet keystore: {e:?}"))?;
    Ok((wallet_id, key, wallet_bytes))
}

async fn mint_and_consume(
    client: &mut Client<FilesystemKeyStore>,
    faucet_id: AccountId,
    target_id: AccountId,
    amount: u64,
) -> anyhow::Result<()> {
    let mint_asset =
        FungibleAsset::new(faucet_id, amount).map_err(|e| anyhow::anyhow!("asset: {e:?}"))?;
    let mint_req = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(mint_asset, target_id, NoteType::Public, client.rng())
        .map_err(|e| anyhow::anyhow!("mint tx build: {e:?}"))?;
    let mint_tx = client.submit_new_transaction(faucet_id, mint_req).await?;
    eprintln!("  mint tx submitted: {mint_tx}");

    for attempt in 0..60 {
        client.sync_state().await?;
        let consumable = client.get_consumable_notes(Some(target_id)).await?;
        if !consumable.is_empty() {
            eprintln!(
                "  found {} consumable notes (attempt {attempt}), consuming...",
                consumable.len()
            );
            let notes: Vec<_> = consumable
                .into_iter()
                .map(|(note, _)| note.try_into())
                .collect::<Result<_, _>>()?;
            let consume_req = TransactionRequestBuilder::new()
                .build_consume_notes(notes)
                .map_err(|e| anyhow::anyhow!("consume build: {e:?}"))?;
            let consume_tx = client
                .submit_new_transaction(target_id, consume_req)
                .await?;
            eprintln!("  mint note consumed: {consume_tx}");
            return Ok(());
        }
        if attempt == 59 {
            anyhow::bail!("timed out waiting for consumable mint note");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    unreachable!()
}

#[tokio::test]
#[ignore = "requires network access to Miden testnet"]
async fn two_client_falcon_p2id() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    // ═══════════════════════════════════════════════════════════════
    // CLIENT A: deploy accounts, mint, create Falcon P2ID note
    // ═══════════════════════════════════════════════════════════════
    let tmp_a = tempfile::tempdir()?;
    let (mut client_a, keystore_a) = build_client(tmp_a.path()).await?;
    client_a.sync_state().await?;
    eprintln!("[falcon] step 1: client A synced");

    // Deploy faucet (Public)
    let faucet_id = deploy_faucet(&mut client_a, &keystore_a, "FTST").await?;
    eprintln!("[falcon] step 2: faucet deployed: {}", faucet_id.to_hex());

    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Deploy wallet A (Public)
    let (wallet_a_id, _wallet_a_key, _wallet_a_bytes) =
        deploy_wallet(&mut client_a, &keystore_a).await?;
    eprintln!("[falcon] step 3: wallet A deployed: {}", wallet_a_id.to_hex());

    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Deploy wallet B (Public) -- deployed by client A so it's on-chain
    let (wallet_b_id, wallet_b_key, wallet_b_bytes) =
        deploy_wallet(&mut client_a, &keystore_a).await?;
    eprintln!("[falcon] step 4: wallet B deployed: {}", wallet_b_id.to_hex());

    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Mint tokens to wallet A and consume
    eprintln!("[falcon] step 5: minting tokens to wallet A...");
    mint_and_consume(&mut client_a, faucet_id, wallet_a_id, 10_000).await?;
    eprintln!("[falcon] step 5: wallet A funded");

    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ── Create the Falcon P2ID note ──
    let agent_sk = AuthSecretKey::new_falcon512_poseidon2_with_rng(client_a.rng());
    let agent_pk: Word = agent_sk.public_key().to_commitment().into();

    let note_script = CodeBuilder::default().compile_note_script(FALCON_P2ID_MASM)?;
    eprintln!("[falcon] step 6: Falcon P2ID note script compiled");

    let amount = 500u64;
    let asset =
        FungibleAsset::new(faucet_id, amount).map_err(|e| anyhow::anyhow!("asset: {e:?}"))?;

    // Storage: 6 items [agent_pk[0], agent_pk[1], agent_pk[2], agent_pk[3], target_suffix, target_prefix]
    let storage = NoteStorage::new(vec![
        agent_pk[0],
        agent_pk[1],
        agent_pk[2],
        agent_pk[3],
        wallet_b_id.suffix(),
        wallet_b_id.prefix().as_felt(),
    ])?;

    let mut serial_bytes = [0u8; 32];
    client_a.rng().fill_bytes(&mut serial_bytes);
    let serial_num: Word = [
        Felt::new(u64::from_le_bytes(serial_bytes[0..8].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[8..16].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[16..24].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[24..32].try_into().unwrap())),
    ]
    .into();

    let tag = NoteTag::with_account_target(wallet_b_id);
    let metadata = NoteMetadata::new(wallet_a_id, NoteType::Public).with_tag(tag);
    let vault = NoteAssets::new(vec![Asset::Fungible(asset)])?;
    let recipient = NoteRecipient::new(serial_num, note_script, storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();
    eprintln!("[falcon] step 6: Falcon P2ID note built, id={note_id}");

    // Serialize note for client B
    let note_bytes = note.to_bytes();
    eprintln!("[falcon] step 6: note serialized ({} bytes)", note_bytes.len());

    // Submit note on-chain via own_output_notes
    let create_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![note.clone()])
        .build()
        .map_err(|e| anyhow::anyhow!("own_output_notes build: {e:?}"))?;
    let create_tx = client_a
        .submit_new_transaction(wallet_a_id, create_req)
        .await?;
    eprintln!("[falcon] step 7: note submitted on-chain: {create_tx}");

    // Wait for on-chain confirmation
    tokio::time::sleep(Duration::from_secs(10)).await;
    client_a.sync_state().await?;
    eprintln!("[falcon] step 7: client A synced after submission");

    // ═══════════════════════════════════════════════════════════════
    // CLIENT B: separate client, import wallet B + note, consume
    // ═══════════════════════════════════════════════════════════════
    eprintln!("[falcon] ══════════════════════════════════════════════════════");
    eprintln!("[falcon] CLIENT B: Starting consumer client");
    eprintln!("[falcon] ══════════════════════════════════════════════════════");

    let tmp_b = tempfile::tempdir()?;
    let (mut client_b, keystore_b) = build_client(tmp_b.path()).await?;
    client_b.sync_state().await?;
    eprintln!("[falcon] step 8: client B created and synced");

    // Import wallet B from serialized bytes
    let wallet_b_imported = Account::read_from_bytes(&wallet_b_bytes)?;
    client_b.add_account(&wallet_b_imported, false).await?;
    keystore_b
        .add_key(&wallet_b_key, wallet_b_id)
        .await
        .map_err(|e| anyhow::anyhow!("keystore add wallet B key: {e:?}"))?;
    eprintln!("[falcon] step 8: wallet B imported into client B");

    // Import note via NoteFile::NoteDetails with tag
    let deserialized_note = Note::read_from_bytes(&note_bytes)?;
    assert_eq!(deserialized_note.id(), note_id);
    let note_details = NoteDetails::new(
        deserialized_note.assets().clone(),
        deserialized_note.recipient().clone(),
    );
    client_b
        .import_notes(&[NoteFile::NoteDetails {
            details: note_details,
            after_block_num: 0u32.into(),
            tag: Some(tag),
        }])
        .await?;
    eprintln!("[falcon] step 9: note imported into client B");

    // Sync client B to discover the note on-chain
    for attempt in 0..60 {
        client_b.sync_state().await?;
        if let Ok(Some(record)) = client_b.get_input_note(note_id).await {
            if record.is_authenticated() {
                eprintln!("[falcon] step 9: note authenticated (attempt {attempt})");
                break;
            }
            if attempt % 5 == 0 {
                eprintln!(
                    "[falcon]   attempt {attempt}: is_authenticated={}",
                    record.is_authenticated()
                );
            }
        } else if attempt % 10 == 0 {
            eprintln!("[falcon]   attempt {attempt}: note not in store yet");
        }
        if attempt == 59 {
            eprintln!("[falcon] WARNING: note never became authenticated after 60 attempts, trying anyway...");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    // ── Build note_args + Falcon signature for consumption ──
    let note_args: Word = [Felt::new(amount), Felt::ZERO, Felt::ZERO, Felt::ZERO].into();
    let message: Word = Hasher::merge(&[serial_num.into(), note_args.into()]).into();
    let sig = agent_sk.sign(message);
    let prepared: Vec<Felt> = sig.to_prepared_signature(message);
    let sig_key: Word = Hasher::merge(&[agent_pk.into(), message.into()]).into();

    eprintln!("[falcon] step 10: consuming Falcon P2ID note from client B...");
    eprintln!("[falcon]   note_args={note_args:?}");
    eprintln!("[falcon]   sig_key={sig_key:?}");
    eprintln!("[falcon]   prepared_sig len={}", prepared.len());

    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(deserialized_note, Some(note_args))])
        .extend_advice_map([(sig_key, prepared.as_slice())])
        .build()
        .map_err(|e| anyhow::anyhow!("consume build: {e:?}"))?;

    match client_b
        .submit_new_transaction(wallet_b_id, consume_req)
        .await
    {
        Ok(tx_id) => {
            eprintln!("[falcon] ══════════════════════════════════════════════════════");
            eprintln!("[falcon] SUCCESS: Falcon P2ID note consumed cross-client! tx={tx_id}");
            eprintln!("[falcon] ══════════════════════════════════════════════════════");
            Ok(())
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!("[falcon] ══════════════════════════════════════════════════════");
            eprintln!("[falcon] FAILED: {}", &err_str[..err_str.len().min(800)]);
            eprintln!("[falcon] ══════════════════════════════════════════════════════");
            anyhow::bail!("Falcon P2ID cross-client consume failed: {e}");
        }
    }
}
