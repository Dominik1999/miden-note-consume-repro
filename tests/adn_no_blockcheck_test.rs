//! Cross-client testnet test: ADN without block-height check.
//!
//! Tests whether removing the block-height branching (exec.tx::get_block_number
//! + expiry comparison + if.true/else) from the ADN MASM fixes the
//! StackReadFailed error observed in cross-client testnet consumption.
//!
//! Flow:
//!   Client A: deploys faucet + wallet A, mints, funds wallet A,
//!             creates ADN note via own_output_notes, serializes it.
//!   Client B: deploys wallet B, deserializes + imports the note,
//!             syncs, tries to consume with Falcon sig in advice map.
//!
//! Run:
//!   RUST_LOG=info cargo test --test adn_no_blockcheck_test -- --ignored --nocapture

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
use miden_protocol::asset::Asset;
use miden_protocol::note::{
    Note, NoteAssets, NoteDetails, NoteFile, NoteMetadata, NoteRecipient, NoteStorage, NoteTag,
};
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_protocol::{Hasher, Word};
use miden_standards::code_builder::CodeBuilder;
use rand::RngCore;

const ADN_NO_BLOCKCHECK_MASM: &str = include_str!("../masm/adn_no_blockcheck.masm");

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

fn rand_seed(client: &mut Client<FilesystemKeyStore>) -> [u8; 32] {
    let mut seed = [0u8; 32];
    client.rng().fill_bytes(&mut seed);
    seed
}

async fn deploy_faucet(
    client: &mut Client<FilesystemKeyStore>,
    keystore: &Arc<FilesystemKeyStore>,
    symbol_str: &str,
) -> anyhow::Result<miden_protocol::account::AccountId> {
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
) -> anyhow::Result<miden_protocol::account::AccountId> {
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
    client.add_account(&wallet, false).await?;
    keystore
        .add_key(&key, wallet_id)
        .await
        .map_err(|e| anyhow::anyhow!("wallet keystore: {e:?}"))?;
    Ok(wallet_id)
}

async fn mint_and_consume(
    client: &mut Client<FilesystemKeyStore>,
    faucet_id: miden_protocol::account::AccountId,
    target_id: miden_protocol::account::AccountId,
    amount: u64,
) -> anyhow::Result<()> {
    let mint_asset = FungibleAsset::new(faucet_id, amount)
        .map_err(|e| anyhow::anyhow!("FungibleAsset: {e:?}"))?;
    let mint_req = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(mint_asset, target_id, NoteType::Public, client.rng())
        .map_err(|e| anyhow::anyhow!("mint tx build: {e:?}"))?;
    let mint_tx = client.submit_new_transaction(faucet_id, mint_req).await?;
    eprintln!("  mint tx submitted: {mint_tx}");

    // Wait for consumable, then consume
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

async fn wait_for_note_consumable(
    client: &mut Client<FilesystemKeyStore>,
    account_id: miden_protocol::account::AccountId,
    note_id: miden_protocol::note::NoteId,
    max_attempts: usize,
) -> anyhow::Result<bool> {
    for attempt in 0..max_attempts {
        client.sync_state().await?;
        let consumable = client.get_consumable_notes(Some(account_id)).await?;
        if consumable.iter().any(|(n, _)| n.id() == note_id) {
            eprintln!("  note {note_id} is consumable (attempt {attempt})");
            return Ok(true);
        }
        if attempt == max_attempts - 1 {
            eprintln!(
                "  WARNING: note {note_id} never became consumable after {max_attempts} attempts"
            );
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    Ok(false)
}

#[tokio::test]
#[ignore = "requires network access to Miden testnet"]
async fn adn_no_blockcheck_cross_client() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    // ═══════════════════════════════════════════════════════════════
    // CLIENT A: deploys faucet + wallet A, mints, creates ADN note
    // ═══════════════════════════════════════════════════════════════
    let tmp_a = tempfile::tempdir()?;
    let (mut client_a, keystore_a) = build_client(tmp_a.path()).await?;
    client_a.sync_state().await?;
    eprintln!("[adn-noblock] step 0: client A synced");

    // Deploy faucet
    let faucet_id = deploy_faucet(&mut client_a, &keystore_a, "ATEST").await?;
    eprintln!(
        "[adn-noblock] step 1: faucet deployed: {}",
        faucet_id.to_hex()
    );

    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Deploy wallet A
    let wallet_a_id = deploy_wallet(&mut client_a, &keystore_a).await?;
    eprintln!(
        "[adn-noblock] step 2: wallet A deployed: {}",
        wallet_a_id.to_hex()
    );

    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Mint tokens to wallet A and consume the mint note
    eprintln!("[adn-noblock] step 3: minting tokens to wallet A...");
    mint_and_consume(&mut client_a, faucet_id, wallet_a_id, 10_000).await?;
    eprintln!("[adn-noblock] step 3: wallet A funded with 10000 tokens");

    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ── Generate the agent signing key (separate from wallet A's auth key) ──
    let agent_sk = AuthSecretKey::new_falcon512_poseidon2_with_rng(client_a.rng());
    let agent_pk: Word = agent_sk.public_key().to_commitment().into();

    // ── Client B setup (need wallet B's ID for the note tag) ──
    let tmp_b = tempfile::tempdir()?;
    let (mut client_b, keystore_b) = build_client(tmp_b.path()).await?;
    client_b.sync_state().await?;
    eprintln!("[adn-noblock] step 4: client B synced");

    let wallet_b_id = deploy_wallet(&mut client_b, &keystore_b).await?;
    eprintln!(
        "[adn-noblock] step 4: wallet B deployed: {}",
        wallet_b_id.to_hex()
    );

    client_b.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ═══════════════════════════════════════════════════════════════
    // CLIENT A: create the ADN note (no block check variant)
    // ═══════════════════════════════════════════════════════════════
    let note_script = CodeBuilder::default().compile_note_script(ADN_NO_BLOCKCHECK_MASM)?;
    eprintln!("[adn-noblock] step 5: note script compiled");

    let balance = 1000u64;
    let amount = 100u64;
    let asset =
        FungibleAsset::new(faucet_id, balance).map_err(|e| anyhow::anyhow!("asset: {e:?}"))?;

    // 7 storage items: agent_pk(4), user_suffix, user_prefix, expiry
    let storage = NoteStorage::new(vec![
        agent_pk[0],
        agent_pk[1],
        agent_pk[2],
        agent_pk[3],
        wallet_a_id.suffix(),
        wallet_a_id.prefix().as_felt(),
        Felt::new(1_000_000), // expiry far in the future (unused but kept for layout)
    ])?;

    // Random serial number
    let mut serial_bytes = [0u8; 32];
    client_a.rng().fill_bytes(&mut serial_bytes);
    let serial_num: Word = [
        Felt::new(u64::from_le_bytes(serial_bytes[0..8].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[8..16].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[16..24].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[24..32].try_into().unwrap())),
    ]
    .into();

    // Note args: [merchant_suffix, merchant_prefix, amount, 0]
    // Use wallet_b as the "merchant" for simplicity
    let note_args: Word = [
        wallet_b_id.suffix(),
        wallet_b_id.prefix().as_felt(),
        Felt::new(amount),
        Felt::ZERO,
    ]
    .into();

    let tag = NoteTag::with_account_target(wallet_b_id);
    let metadata = NoteMetadata::new(wallet_a_id, NoteType::Public).with_tag(tag);
    let vault = NoteAssets::new(vec![Asset::Fungible(asset)])?;
    let recipient = NoteRecipient::new(serial_num, note_script, storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();
    eprintln!("[adn-noblock] step 5: ADN note built, id={note_id}");

    // Serialize the note to bytes (simulates cross-client transfer)
    let note_bytes = note.to_bytes();
    eprintln!(
        "[adn-noblock] step 5: note serialized ({} bytes)",
        note_bytes.len()
    );

    // Submit on-chain from wallet A via own_output_notes
    let create_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![note.clone()])
        .build()
        .map_err(|e| anyhow::anyhow!("create note tx: {e:?}"))?;
    let create_tx = client_a
        .submit_new_transaction(wallet_a_id, create_req)
        .await?;
    eprintln!("[adn-noblock] step 5: ADN note submitted on-chain: {create_tx}");

    // ═══════════════════════════════════════════════════════════════
    // CLIENT B: deserialize, import, sync, consume
    // ═══════════════════════════════════════════════════════════════
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Deserialize the note from bytes
    let deserialized_note = Note::read_from_bytes(&note_bytes)?;
    assert_eq!(deserialized_note.id(), note_id);
    eprintln!("[adn-noblock] step 6: note deserialized on client B");

    // Import via NoteFile::NoteDetails
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
    eprintln!("[adn-noblock] step 6: note imported into client B");

    // Wait for note to become consumable
    eprintln!("[adn-noblock] step 6: waiting for note to become consumable...");
    let found = wait_for_note_consumable(&mut client_b, wallet_b_id, note_id, 60).await?;
    if !found {
        eprintln!("[adn-noblock] WARNING: note never became consumable, trying anyway...");
    }

    // ── Compute Falcon signature and advice map entry ──
    // message = merge(serial_num, note_args)
    let message: Word = Hasher::merge(&[serial_num.into(), note_args.into()]).into();
    let sig = agent_sk.sign(message);
    let prepared: Vec<Felt> = sig.to_prepared_signature(message);
    // sig_key = merge(agent_pk, message)
    let sig_key: Word = Hasher::merge(&[agent_pk.into(), message.into()]).into();

    eprintln!("[adn-noblock] step 7: Falcon signature computed");
    eprintln!(
        "[adn-noblock]   sig_key={sig_key:?}, prepared_len={}",
        prepared.len()
    );

    // Build the consume request with the note + args + advice map
    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(deserialized_note, Some(note_args))])
        .extend_advice_map([(sig_key, prepared.as_slice())])
        .build()
        .map_err(|e| anyhow::anyhow!("consume build: {e:?}"))?;

    eprintln!("[adn-noblock] step 7: client B attempting to consume ADN note...");

    match client_b
        .submit_new_transaction(wallet_b_id, consume_req)
        .await
    {
        Ok(tx_id) => {
            eprintln!("[adn-noblock] SUCCESS: tx_id={tx_id}");
            eprintln!("[adn-noblock]   ADN (no block check) consumed cross-client on testnet!");
            eprintln!("[adn-noblock]   => Block height check WAS the cause of StackReadFailed.");
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!(
                "[adn-noblock] FAILED: {}",
                &err_str[..err_str.len().min(500)]
            );
            if err_str.contains("StackReadFailed") {
                eprintln!("[adn-noblock]   StackReadFailed still present WITHOUT block check!");
                eprintln!("[adn-noblock]   => Block height check is NOT the cause.");
                panic!("StackReadFailed persists even without block height check");
            } else {
                eprintln!("[adn-noblock]   Different error (not StackReadFailed).");
                panic!(
                    "Unexpected error: {}",
                    &err_str[..err_str.len().min(300)]
                );
            }
        }
    }

    Ok(())
}
