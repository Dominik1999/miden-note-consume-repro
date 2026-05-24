//! Two-client P2ID test (PRIVATE): Client A creates a P2ID note with NoteType::Private,
//! then Client B (separate store/keystore) imports and consumes it.
//! Private notes are NOT stored on-chain, so the note stays unauthenticated (Expected).
//! Client B consumes it as unauthenticated via input_notes([(note, ...)]).
//!
//! Run with:
//!   RUST_LOG=miden_client::transaction=info cargo test --test two_client_p2id_private_test -- --ignored --nocapture

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
use miden_protocol::note::*;
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_protocol::Word;
use miden_standards::code_builder::CodeBuilder;
use rand::RngCore;

async fn build_client(
    dir: &std::path::Path,
) -> anyhow::Result<(Client<FilesystemKeyStore>, Arc<FilesystemKeyStore>)> {
    let endpoint = Endpoint::try_from("https://rpc.testnet.miden.io").unwrap();
    let rpc = Arc::new(GrpcClient::new(&endpoint, 30_000));
    let ks = Arc::new(FilesystemKeyStore::new(dir.join("keystore")).unwrap());
    let c = ClientBuilder::new()
        .rpc(rpc)
        .sqlite_store(dir.join("store.sqlite3"))
        .authenticator(ks.clone())
        .in_debug_mode(true.into())
        .build()
        .await
        .unwrap();
    Ok((c, ks))
}

#[tokio::test]
#[ignore]
async fn two_client_p2id_private() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    // ═══════════════════════════════════════════════════════════════════════
    // CLIENT A: Creator / Sender
    // ═══════════════════════════════════════════════════════════════════════

    let tmp_a = tempfile::tempdir()?;
    let (mut client_a, keystore_a) = build_client(tmp_a.path()).await?;
    client_a.sync_state().await?;
    eprintln!("[p2id-priv] client A synced");

    // 1. Deploy faucet (Public, symbol "PTST")
    let faucet_seed = {
        let mut s = [0u8; 32];
        client_a.rng().fill_bytes(&mut s);
        s
    };
    let faucet_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client_a.rng());
    let symbol = TokenSymbol::new("PTST").map_err(|e| anyhow::anyhow!("symbol: {e:?}"))?;
    let faucet = AccountBuilder::new(faucet_seed)
        .account_type(AccountType::FungibleFaucet)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            faucet_key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(
            BasicFungibleFaucet::new(symbol, 6, Felt::new(1_000_000_000))
                .map_err(|e| anyhow::anyhow!("faucet component: {e:?}"))?,
        )
        .with_component(AuthControlled::allow_all())
        .build()
        .map_err(|e| anyhow::anyhow!("faucet build: {e:?}"))?;
    let faucet_id = faucet.id();
    client_a.add_account(&faucet, false).await?;
    keystore_a
        .add_key(&faucet_key, faucet_id)
        .await
        .map_err(|e| anyhow::anyhow!("faucet key: {e:?}"))?;
    eprintln!("[p2id-priv] step 1: faucet deployed: {}", faucet_id.to_hex());

    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 2. Deploy wallet A (Public, BasicWallet)
    let wallet_a_seed = {
        let mut s = [0u8; 32];
        client_a.rng().fill_bytes(&mut s);
        s
    };
    let wallet_a_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client_a.rng());
    let wallet_a = AccountBuilder::new(wallet_a_seed)
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            wallet_a_key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(BasicWallet)
        .build()
        .map_err(|e| anyhow::anyhow!("wallet A build: {e:?}"))?;
    let wallet_a_id = wallet_a.id();
    client_a.add_account(&wallet_a, false).await?;
    keystore_a
        .add_key(&wallet_a_key, wallet_a_id)
        .await
        .map_err(|e| anyhow::anyhow!("wallet A key: {e:?}"))?;
    eprintln!("[p2id-priv] step 2: wallet A deployed: {}", wallet_a_id.to_hex());

    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 3. Create wallet B's Account (Public, BasicWallet) — do NOT add to client A
    let wallet_b_seed = {
        let mut s = [0u8; 32];
        client_a.rng().fill_bytes(&mut s);
        s
    };
    let wallet_b_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client_a.rng());
    let wallet_b = AccountBuilder::new(wallet_b_seed)
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            wallet_b_key.public_key().to_commitment().into(),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .with_component(BasicWallet)
        .build()
        .map_err(|e| anyhow::anyhow!("wallet B build: {e:?}"))?;
    let wallet_b_id = wallet_b.id();
    eprintln!(
        "[p2id-priv] step 3: wallet B created (NOT in client A): {}",
        wallet_b_id.to_hex()
    );

    // 4. Mint to wallet A, consume the mint note
    eprintln!("[p2id-priv] step 4: minting tokens to wallet A...");
    let mint_asset = FungibleAsset::new(faucet_id, 10_000)
        .map_err(|e| anyhow::anyhow!("mint asset: {e:?}"))?;
    let mint_req = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(mint_asset, wallet_a_id, NoteType::Public, client_a.rng())
        .map_err(|e| anyhow::anyhow!("mint req: {e:?}"))?;
    let mint_tx = client_a
        .submit_new_transaction(faucet_id, mint_req)
        .await?;
    eprintln!("[p2id-priv] step 4: mint tx submitted: {mint_tx}");

    // Wait for mint note to become consumable, then consume it
    for attempt in 0..60 {
        client_a.sync_state().await?;
        let consumable = client_a.get_consumable_notes(Some(wallet_a_id)).await?;
        if !consumable.is_empty() {
            eprintln!("[p2id-priv] step 4: mint note consumable (attempt {attempt}), consuming...");
            let notes: Vec<_> = consumable
                .into_iter()
                .map(|(note, _)| note.try_into())
                .collect::<Result<_, _>>()?;
            let consume_req = TransactionRequestBuilder::new()
                .build_consume_notes(notes)
                .map_err(|e| anyhow::anyhow!("consume mint: {e:?}"))?;
            let consume_tx = client_a
                .submit_new_transaction(wallet_a_id, consume_req)
                .await?;
            eprintln!("[p2id-priv] step 4: mint note consumed: {consume_tx}");
            break;
        }
        if attempt == 59 {
            anyhow::bail!("timed out waiting for consumable mint note");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    eprintln!("[p2id-priv] step 4: wallet A funded");

    // 5. Build a P2ID note MANUALLY with NoteType::Private
    let p2id_script_src = "use miden::standards::notes::p2id\n@note_script\npub proc main\n    exec.p2id::main\nend";
    let p2id_script = CodeBuilder::default().compile_note_script(p2id_script_src)?;

    let storage = NoteStorage::new(vec![wallet_b_id.suffix(), wallet_b_id.prefix().as_felt()])?;

    let mut serial_bytes = [0u8; 32];
    client_a.rng().fill_bytes(&mut serial_bytes);
    let serial: Word = [
        Felt::new(u64::from_le_bytes(serial_bytes[0..8].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[8..16].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[16..24].try_into().unwrap())),
        Felt::new(u64::from_le_bytes(serial_bytes[24..32].try_into().unwrap())),
    ]
    .into();

    let send_asset = FungibleAsset::new(faucet_id, 1_000)
        .map_err(|e| anyhow::anyhow!("send asset: {e:?}"))?;
    let tag = NoteTag::with_account_target(wallet_b_id);
    let metadata = NoteMetadata::new(wallet_a_id, NoteType::Private).with_tag(tag);
    let vault = NoteAssets::new(vec![Asset::Fungible(send_asset)])?;
    let recipient = NoteRecipient::new(serial, p2id_script, storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();
    eprintln!("[p2id-priv] step 5: P2ID note built manually (PRIVATE), id={note_id}");

    // 6. Submit via own_output_notes
    let create_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![note.clone()])
        .build()
        .map_err(|e| anyhow::anyhow!("own_output_notes tx build: {e:?}"))?;
    let create_tx = client_a
        .submit_new_transaction(wallet_a_id, create_req)
        .await?;
    eprintln!("[p2id-priv] step 6: P2ID note submitted on-chain: {create_tx}");

    // 7. Serialize note and wallet B
    let note_bytes = note.to_bytes();
    let wallet_b_bytes = wallet_b.to_bytes();
    eprintln!(
        "[p2id-priv] step 7: serialized note ({} bytes) and wallet B ({} bytes)",
        note_bytes.len(),
        wallet_b_bytes.len()
    );

    // 8. Wait 10 seconds for on-chain commit
    eprintln!("[p2id-priv] step 8: waiting 10s for on-chain commit...");
    tokio::time::sleep(Duration::from_secs(10)).await;

    // ═══════════════════════════════════════════════════════════════════════
    // CLIENT B: Consumer (completely separate client instance)
    // ═══════════════════════════════════════════════════════════════════════

    eprintln!("[p2id-priv] ══════════════════════════════════════════════════════");
    eprintln!("[p2id-priv] CLIENT B: Starting consumer client");
    eprintln!("[p2id-priv] ══════════════════════════════════════════════════════");

    let tmp_b = tempfile::tempdir()?;
    let (mut client_b, keystore_b) = build_client(tmp_b.path()).await?;

    // 1. Deserialize wallet B + P2ID note from bytes
    let wallet_b_deserialized = Account::read_from_bytes(&wallet_b_bytes)?;
    let p2id_note_b = Note::read_from_bytes(&note_bytes)?;
    eprintln!(
        "[p2id-priv] B step 1: deserialized wallet B={} and note={}",
        wallet_b_deserialized.id().to_hex(),
        p2id_note_b.id()
    );

    // 2. Import wallet B via add_account
    client_b.add_account(&wallet_b_deserialized, false).await?;
    eprintln!("[p2id-priv] B step 2: wallet B imported");

    // 3. Add wallet B key to keystore
    keystore_b
        .add_key(&wallet_b_key, wallet_b_id)
        .await
        .map_err(|e| anyhow::anyhow!("wallet B key in client B: {e:?}"))?;
    eprintln!("[p2id-priv] B step 3: wallet B key added to keystore");

    // 4. Import P2ID note via NoteFile::NoteDetails with tag
    let note_details = NoteDetails::new(p2id_note_b.assets().clone(), p2id_note_b.recipient().clone());
    client_b
        .import_notes(&[NoteFile::NoteDetails {
            details: note_details,
            after_block_num: 0u32.into(),
            tag: Some(tag),
        }])
        .await?;
    eprintln!("[p2id-priv] B step 4: P2ID note imported with tag");

    // 5. Sync - for private notes, the note will NOT become authenticated
    // (the node only stores the commitment, not the note data).
    // We just need to sync so client B is up-to-date with the chain.
    eprintln!("[p2id-priv] B step 5: syncing (note will stay unauthenticated for private notes)...");
    for attempt in 0..20 {
        client_b.sync_state().await?;

        if let Ok(Some(record)) = client_b.get_input_note(note_id).await {
            if attempt % 5 == 0 {
                eprintln!(
                    "[p2id-priv]   attempt {attempt}: is_authenticated={}",
                    record.is_authenticated()
                );
            }
            if record.is_authenticated() {
                eprintln!("[p2id-priv] B step 5: note authenticated (unexpected for private, but OK)!");
                break;
            }
        } else if attempt % 10 == 0 {
            eprintln!("[p2id-priv]   attempt {attempt}: note not in store yet");
        }

        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    eprintln!("[p2id-priv] B step 5: proceeding with unauthenticated note consumption");

    // 6. Consume via input_notes([(note, None)]) - works even if unauthenticated
    eprintln!("[p2id-priv] B step 6: consuming P2ID note (unauthenticated)...");
    let note_for_consume = Note::read_from_bytes(&note_bytes)?;
    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(note_for_consume, None)])
        .build()
        .map_err(|e| anyhow::anyhow!("input_notes build: {e:?}"))?;

    match client_b.submit_new_transaction(wallet_b_id, consume_req).await {
        Ok(tx_id) => {
            eprintln!("[p2id-priv] ══════════════════════════════════════════════════════");
            eprintln!("[p2id-priv] SUCCESS: P2ID PRIVATE note consumed cross-client! tx={tx_id}");
            eprintln!("[p2id-priv] ══════════════════════════════════════════════════════");
            Ok(())
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!("[p2id-priv] ══════════════════════════════════════════════════════");
            eprintln!("[p2id-priv] FAILED: {}", &err_str[..err_str.len().min(500)]);
            eprintln!("[p2id-priv] ══════════════════════════════════════════════════════");
            anyhow::bail!("P2ID PRIVATE cross-client consume failed: {e}");
        }
    }
}
