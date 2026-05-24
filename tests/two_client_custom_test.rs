//! Two-client custom note test: Client A creates a secret_hash_note MANUALLY via `own_output_notes`,
//! then Client B (separate store/keystore) imports and consumes it with the secret as note_args.
//!
//! Run with:
//!   RUST_LOG=miden_client::transaction=info cargo test --test two_client_custom_test -- --ignored --nocapture

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
use miden_protocol::{Hasher, Word};
use miden_standards::code_builder::CodeBuilder;
use rand::RngCore;

const NOTE_MASM: &str = include_str!("../masm/secret_hash_note.masm");

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
async fn two_client_custom_note() -> anyhow::Result<()> {
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
    eprintln!("[custom] client A synced");

    // 1. Deploy faucet (Public, symbol "CTST")
    let faucet_seed = {
        let mut s = [0u8; 32];
        client_a.rng().fill_bytes(&mut s);
        s
    };
    let faucet_key = AuthSecretKey::new_falcon512_poseidon2_with_rng(client_a.rng());
    let symbol = TokenSymbol::new("CTST").map_err(|e| anyhow::anyhow!("symbol: {e:?}"))?;
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
    eprintln!("[custom] step 1: faucet deployed: {}", faucet_id.to_hex());

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
    eprintln!("[custom] step 2: wallet A deployed: {}", wallet_a_id.to_hex());

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
        "[custom] step 3: wallet B created (NOT in client A): {}",
        wallet_b_id.to_hex()
    );

    // 4. Mint to wallet A, consume the mint note
    eprintln!("[custom] step 4: minting tokens to wallet A...");
    let mint_asset = FungibleAsset::new(faucet_id, 10_000)
        .map_err(|e| anyhow::anyhow!("mint asset: {e:?}"))?;
    let mint_req = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(mint_asset, wallet_a_id, NoteType::Public, client_a.rng())
        .map_err(|e| anyhow::anyhow!("mint req: {e:?}"))?;
    let mint_tx = client_a
        .submit_new_transaction(faucet_id, mint_req)
        .await?;
    eprintln!("[custom] step 4: mint tx submitted: {mint_tx}");

    // Wait for mint note to become consumable, then consume it
    for attempt in 0..60 {
        client_a.sync_state().await?;
        let consumable = client_a.get_consumable_notes(Some(wallet_a_id)).await?;
        if !consumable.is_empty() {
            eprintln!("[custom] step 4: mint note consumable (attempt {attempt}), consuming...");
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
            eprintln!("[custom] step 4: mint note consumed: {consume_tx}");
            break;
        }
        if attempt == 59 {
            anyhow::bail!("timed out waiting for consumable mint note");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    client_a.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    eprintln!("[custom] step 4: wallet A funded");

    // 5. Build a custom secret_hash_note MANUALLY
    let note_script = CodeBuilder::default().compile_note_script(NOTE_MASM)?;

    // Storage: 4 items = the digest of the secret
    let digest = secret_digest();
    let digest_felts: [Felt; 4] = digest.into();
    let storage = NoteStorage::new(digest_felts.to_vec())?;

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
    let metadata = NoteMetadata::new(wallet_a_id, NoteType::Public).with_tag(tag);
    let vault = NoteAssets::new(vec![Asset::Fungible(send_asset)])?;
    let recipient = NoteRecipient::new(serial, note_script, storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();
    eprintln!("[custom] step 5: custom note built manually, id={note_id}");

    // 6. Submit via own_output_notes
    let create_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![note.clone()])
        .build()
        .map_err(|e| anyhow::anyhow!("own_output_notes tx build: {e:?}"))?;
    let create_tx = client_a
        .submit_new_transaction(wallet_a_id, create_req)
        .await?;
    eprintln!("[custom] step 6: custom note submitted on-chain: {create_tx}");

    // 7. Serialize note and wallet B
    let note_bytes = note.to_bytes();
    let wallet_b_bytes = wallet_b.to_bytes();
    eprintln!(
        "[custom] step 7: serialized note ({} bytes) and wallet B ({} bytes)",
        note_bytes.len(),
        wallet_b_bytes.len()
    );

    // 8. Wait 10 seconds for on-chain commit
    eprintln!("[custom] step 8: waiting 10s for on-chain commit...");
    tokio::time::sleep(Duration::from_secs(10)).await;

    // ═══════════════════════════════════════════════════════════════════════
    // CLIENT B: Consumer (completely separate client instance)
    // ═══════════════════════════════════════════════════════════════════════

    eprintln!("[custom] ══════════════════════════════════════════════════════");
    eprintln!("[custom] CLIENT B: Starting consumer client");
    eprintln!("[custom] ══════════════════════════════════════════════════════");

    let tmp_b = tempfile::tempdir()?;
    let (mut client_b, keystore_b) = build_client(tmp_b.path()).await?;

    // 1. Deserialize wallet B + custom note from bytes
    let wallet_b_deserialized = Account::read_from_bytes(&wallet_b_bytes)?;
    let custom_note_b = Note::read_from_bytes(&note_bytes)?;
    eprintln!(
        "[custom] B step 1: deserialized wallet B={} and note={}",
        wallet_b_deserialized.id().to_hex(),
        custom_note_b.id()
    );

    // 2. Import wallet B via add_account
    client_b.add_account(&wallet_b_deserialized, false).await?;
    eprintln!("[custom] B step 2: wallet B imported");

    // 3. Add wallet B key to keystore
    keystore_b
        .add_key(&wallet_b_key, wallet_b_id)
        .await
        .map_err(|e| anyhow::anyhow!("wallet B key in client B: {e:?}"))?;
    eprintln!("[custom] B step 3: wallet B key added to keystore");

    // 4. Import custom note via NoteFile::NoteDetails with tag
    let note_details = NoteDetails::new(custom_note_b.assets().clone(), custom_note_b.recipient().clone());
    client_b
        .import_notes(&[NoteFile::NoteDetails {
            details: note_details,
            after_block_num: 0u32.into(),
            tag: Some(tag),
        }])
        .await?;
    eprintln!("[custom] B step 4: custom note imported with tag");

    // 5. Sync, check note state
    eprintln!("[custom] B step 5: syncing to authenticate note...");
    for attempt in 0..60 {
        client_b.sync_state().await?;

        if let Ok(Some(record)) = client_b.get_input_note(note_id).await {
            if attempt % 5 == 0 || record.is_authenticated() {
                eprintln!(
                    "[custom]   attempt {attempt}: is_authenticated={}",
                    record.is_authenticated()
                );
            }
            if record.is_authenticated() {
                eprintln!("[custom] B step 5: note authenticated!");
                break;
            }
        } else if attempt % 10 == 0 {
            eprintln!("[custom]   attempt {attempt}: note not in store yet");
        }

        if attempt == 59 {
            eprintln!("[custom] WARNING: note never became authenticated after 60 attempts");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    // 6. Consume via input_notes([(note, Some(secret))])
    eprintln!("[custom] B step 6: consuming custom note with secret...");
    let note_for_consume = Note::read_from_bytes(&note_bytes)?;
    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(note_for_consume, Some(secret()))])
        .build()
        .map_err(|e| anyhow::anyhow!("input_notes build: {e:?}"))?;

    match client_b.submit_new_transaction(wallet_b_id, consume_req).await {
        Ok(tx_id) => {
            eprintln!("[custom] ══════════════════════════════════════════════════════");
            eprintln!("[custom] SUCCESS: Custom note consumed cross-client! tx={tx_id}");
            eprintln!("[custom] ══════════════════════════════════════════════════════");
            Ok(())
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            eprintln!("[custom] ══════════════════════════════════════════════════════");
            eprintln!("[custom] FAILED: {}", &err_str[..err_str.len().min(500)]);
            eprintln!("[custom] ══════════════════════════════════════════════════════");
            anyhow::bail!("Custom note cross-client consume failed: {e}");
        }
    }
}
