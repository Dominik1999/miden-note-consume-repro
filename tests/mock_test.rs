//! MockChain tests for custom notes.
//!
//! Test 1-2: Simple secret hash note (passes on both MockChain and real client)
//! Test 3-4: Falcon signature + P2ID output note (passes MockChain, FAILS real client)

use std::collections::BTreeMap;

use miden_protocol::account::auth::AuthSecretKey;
use miden_protocol::asset::{Asset, FungibleAsset};
use miden_protocol::note::*;
use miden_protocol::transaction::RawOutputNote;
use miden_protocol::vm::AdviceInputs;
use miden_protocol::{Felt, Hasher, Word};
use miden_standards::code_builder::CodeBuilder;
use miden_testing::{Auth, MockChain};

const SIMPLE_MASM: &str = include_str!("../masm/secret_hash_note.masm");
const FALCON_P2ID_MASM: &str = include_str!("../masm/falcon_p2id_note.masm");
const FALCON_P2ID_HASMAPKEY_MASM: &str = include_str!("../masm/falcon_p2id_hasmapkey_note.masm");
const ADN_MASM: &str = include_str!("../masm/agent_debit_note.masm");

// ── Helpers ──

fn secret() -> Word {
    [Felt::new(42), Felt::new(43), Felt::new(44), Felt::new(45)].into()
}

fn secret_digest() -> Word {
    let s: [Felt; 4] = secret().into();
    Hasher::hash_elements(&s)
}

fn make_falcon_keypair(seed: u64) -> AuthSecretKey {
    use miden_protocol::crypto::dsa::falcon512_poseidon2::SecretKey;
    use rand_chacha::ChaCha20Rng;
    use rand::SeedableRng;
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    let sk = SecretKey::with_rng(&mut rng);
    AuthSecretKey::Falcon512Poseidon2(sk)
}

// ═══════════════════════════════════════════════════════════════════
// Test 1-2: Simple secret hash note (PASSES on real client too)
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn mock_simple_consume() -> anyhow::Result<()> {
    let note_script = CodeBuilder::default().compile_note_script(SIMPLE_MASM)?;

    let mut builder = MockChain::builder();
    let consumer = builder.add_existing_wallet(Auth::IncrNonce)?;
    let faucet = builder.add_existing_basic_faucet(Auth::IncrNonce, "TEST", 1_000_000, None)?;

    let asset = FungibleAsset::new(faucet.id(), 1000)?;
    let storage = NoteStorage::new(secret_digest().to_vec())?;
    let serial_num: Word = [Felt::new(1), Felt::new(2), Felt::new(3), Felt::new(4)].into();
    let metadata = NoteMetadata::new(consumer.id(), NoteType::Public).with_tag(NoteTag::new(0));
    let vault = NoteAssets::new(vec![Asset::Fungible(asset)])?;
    let recipient = NoteRecipient::new(serial_num, note_script.clone(), storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();

    builder.add_output_note(RawOutputNote::Full(note));
    let mut mock_chain = builder.build()?;
    mock_chain.prove_next_block()?;

    let mut note_args = BTreeMap::new();
    note_args.insert(note_id, secret());

    let tx = mock_chain
        .build_tx_context(consumer.id(), &[note_id], &[])?
        .extend_note_args(note_args)
        .add_note_script(note_script)
        .build()?;

    let executed = tx.execute().await?;
    assert_eq!(executed.output_notes().num_notes(), 0);
    println!("PASSED: simple secret hash note consumed");
    Ok(())
}

#[tokio::test]
async fn mock_simple_wrong_secret_rejected() -> anyhow::Result<()> {
    let note_script = CodeBuilder::default().compile_note_script(SIMPLE_MASM)?;

    let mut builder = MockChain::builder();
    let consumer = builder.add_existing_wallet(Auth::IncrNonce)?;
    let faucet = builder.add_existing_basic_faucet(Auth::IncrNonce, "TEST", 1_000_000, None)?;

    let asset = FungibleAsset::new(faucet.id(), 1000)?;
    let storage = NoteStorage::new(secret_digest().to_vec())?;
    let serial_num: Word = [Felt::new(10), Felt::new(20), Felt::new(30), Felt::new(40)].into();
    let metadata = NoteMetadata::new(consumer.id(), NoteType::Public).with_tag(NoteTag::new(0));
    let vault = NoteAssets::new(vec![Asset::Fungible(asset)])?;
    let recipient = NoteRecipient::new(serial_num, note_script.clone(), storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();

    builder.add_output_note(RawOutputNote::Full(note));
    let mut mock_chain = builder.build()?;
    mock_chain.prove_next_block()?;

    let wrong_secret: Word = [Felt::new(99), Felt::new(99), Felt::new(99), Felt::new(99)].into();
    let mut note_args = BTreeMap::new();
    note_args.insert(note_id, wrong_secret);

    let tx = mock_chain
        .build_tx_context(consumer.id(), &[note_id], &[])?
        .extend_note_args(note_args)
        .add_note_script(note_script)
        .build()?;

    assert!(tx.execute().await.is_err(), "wrong secret should be rejected");
    println!("PASSED: wrong secret correctly rejected");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════
// Test 3-4: Falcon signature + P2ID output note (FAILS on real client)
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn mock_falcon_p2id_consume() -> anyhow::Result<()> {
    let note_script = CodeBuilder::default().compile_note_script(FALCON_P2ID_MASM)?;
    let agent_sk = make_falcon_keypair(42);
    let agent_pk: Word = agent_sk.public_key().to_commitment().into();

    let mut builder = MockChain::builder();
    let consumer = builder.add_existing_wallet(Auth::IncrNonce)?;
    let faucet = builder.add_existing_basic_faucet(Auth::IncrNonce, "TEST", 1_000_000, None)?;
    let target = builder.add_existing_wallet(Auth::Noop)?;

    let amount = 500u64;
    let asset = FungibleAsset::new(faucet.id(), 1000)?;

    // Storage: [agent_pk(4), target_suffix, target_prefix]
    let storage = NoteStorage::new(vec![
        agent_pk[0], agent_pk[1], agent_pk[2], agent_pk[3],
        target.id().suffix(),
        target.id().prefix().as_felt(),
    ])?;

    let serial_num: Word = [Felt::new(100), Felt::new(200), Felt::new(300), Felt::new(400)].into();
    let metadata = NoteMetadata::new(consumer.id(), NoteType::Public).with_tag(NoteTag::new(0));
    let vault = NoteAssets::new(vec![Asset::Fungible(asset)])?;
    let recipient = NoteRecipient::new(serial_num, note_script.clone(), storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();

    builder.add_output_note(RawOutputNote::Full(note));
    let mut mock_chain = builder.build()?;
    mock_chain.prove_next_block()?;

    // Compute message = merge(serial_num, [amount, 0, 0, 0])
    let note_args: Word = [Felt::new(amount), Felt::ZERO, Felt::ZERO, Felt::ZERO].into();
    let message: Word = Hasher::merge(&[serial_num.into(), note_args.into()]).into();

    // Sign with Falcon and put prepared sig in advice map
    let sig = agent_sk.sign(message);
    let prepared = sig.to_prepared_signature(message);
    let sig_key: Word = Hasher::merge(&[agent_pk.into(), message.into()]).into();

    let advice = AdviceInputs::default().with_map([(sig_key, prepared)]);

    let mut args = BTreeMap::new();
    args.insert(note_id, note_args);

    let tx = mock_chain
        .build_tx_context(consumer.id(), &[note_id], &[])?
        .extend_note_args(args)
        .add_note_script(note_script)
        .extend_advice_inputs(advice)
        .build()?;

    let executed = tx.execute().await?;

    // Should produce 1 P2ID output note
    assert_eq!(executed.output_notes().num_notes(), 1, "expected P2ID output note");
    println!("PASSED: Falcon sig verified + P2ID output note created");
    Ok(())
}

#[tokio::test]
async fn mock_falcon_p2id_wrong_sig_rejected() -> anyhow::Result<()> {
    let note_script = CodeBuilder::default().compile_note_script(FALCON_P2ID_MASM)?;
    let agent_sk = make_falcon_keypair(42);
    let agent_pk: Word = agent_sk.public_key().to_commitment().into();
    let wrong_sk = make_falcon_keypair(999);

    let mut builder = MockChain::builder();
    let consumer = builder.add_existing_wallet(Auth::IncrNonce)?;
    let faucet = builder.add_existing_basic_faucet(Auth::IncrNonce, "TEST", 1_000_000, None)?;
    let target = builder.add_existing_wallet(Auth::Noop)?;

    let asset = FungibleAsset::new(faucet.id(), 1000)?;
    let storage = NoteStorage::new(vec![
        agent_pk[0], agent_pk[1], agent_pk[2], agent_pk[3],
        target.id().suffix(),
        target.id().prefix().as_felt(),
    ])?;

    let serial_num: Word = [Felt::new(101), Felt::new(201), Felt::new(301), Felt::new(401)].into();
    let metadata = NoteMetadata::new(consumer.id(), NoteType::Public).with_tag(NoteTag::new(0));
    let vault = NoteAssets::new(vec![Asset::Fungible(asset)])?;
    let recipient = NoteRecipient::new(serial_num, note_script.clone(), storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();

    builder.add_output_note(RawOutputNote::Full(note));
    let mut mock_chain = builder.build()?;
    mock_chain.prove_next_block()?;

    let note_args: Word = [Felt::new(500), Felt::ZERO, Felt::ZERO, Felt::ZERO].into();
    let message: Word = Hasher::merge(&[serial_num.into(), note_args.into()]).into();

    // Sign with WRONG key but put at the right map key
    let wrong_sig = wrong_sk.sign(message);
    let prepared = wrong_sig.to_prepared_signature(message);
    let sig_key: Word = Hasher::merge(&[agent_pk.into(), message.into()]).into();

    let advice = AdviceInputs::default().with_map([(sig_key, prepared)]);

    let mut args = BTreeMap::new();
    args.insert(note_id, note_args);

    let tx = mock_chain
        .build_tx_context(consumer.id(), &[note_id], &[])?
        .extend_note_args(args)
        .add_note_script(note_script)
        .extend_advice_inputs(advice)
        .build()?;

    assert!(tx.execute().await.is_err(), "wrong Falcon sig should be rejected");
    println!("PASSED: wrong Falcon signature correctly rejected");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════
// Test 5: Falcon + P2ID + adv.has_mapkey (the ADN pattern)
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn mock_falcon_p2id_hasmapkey() -> anyhow::Result<()> {
    let note_script = CodeBuilder::default().compile_note_script(FALCON_P2ID_HASMAPKEY_MASM)?;
    let agent_sk = make_falcon_keypair(55);
    let agent_pk: Word = agent_sk.public_key().to_commitment().into();

    let mut builder = MockChain::builder();
    let consumer = builder.add_existing_wallet(Auth::IncrNonce)?;
    let faucet = builder.add_existing_basic_faucet(Auth::IncrNonce, "TEST", 1_000_000, None)?;
    let target = builder.add_existing_wallet(Auth::Noop)?;

    let amount = 500u64;
    let asset = FungibleAsset::new(faucet.id(), 1000)?;
    let storage = NoteStorage::new(vec![
        agent_pk[0], agent_pk[1], agent_pk[2], agent_pk[3],
        target.id().suffix(), target.id().prefix().as_felt(),
    ])?;

    let serial_num: Word = [Felt::new(555), Felt::new(666), Felt::new(777), Felt::new(888)].into();
    let metadata = NoteMetadata::new(consumer.id(), NoteType::Public).with_tag(NoteTag::new(0));
    let vault = NoteAssets::new(vec![Asset::Fungible(asset)])?;
    let recipient = NoteRecipient::new(serial_num, note_script.clone(), storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();

    builder.add_output_note(RawOutputNote::Full(note));
    let mut mock_chain = builder.build()?;
    mock_chain.prove_next_block()?;

    let note_args: Word = [Felt::new(amount), Felt::ZERO, Felt::ZERO, Felt::ZERO].into();
    let message: Word = Hasher::merge(&[serial_num.into(), note_args.into()]).into();
    let sig = agent_sk.sign(message);
    let prepared = sig.to_prepared_signature(message);
    let sig_key: Word = Hasher::merge(&[agent_pk.into(), message.into()]).into();

    let advice = AdviceInputs::default().with_map([(sig_key, prepared)]);
    let mut args = BTreeMap::new();
    args.insert(note_id, note_args);

    let tx = mock_chain
        .build_tx_context(consumer.id(), &[note_id], &[])?
        .extend_note_args(args)
        .add_note_script(note_script)
        .extend_advice_inputs(advice)
        .build()?;

    let executed = tx.execute().await?;
    assert_eq!(executed.output_notes().num_notes(), 1, "expected P2ID output note");
    println!("PASSED: Falcon + P2ID + has_mapkey pattern works in MockChain");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════
// Test 6: Actual AgentDebitNote MASM (7-item storage, block check,
//         2 output notes: P2ID + remainder)
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn mock_adn_consume() -> anyhow::Result<()> {
    use miden_protocol::account::AccountId;

    let note_script = CodeBuilder::default().compile_note_script(ADN_MASM)?;
    let agent_sk = make_falcon_keypair(77);
    let agent_pk: Word = agent_sk.public_key().to_commitment().into();

    let mut builder = MockChain::builder();
    let consumer = builder.add_existing_wallet(Auth::IncrNonce)?;
    let faucet = builder.add_existing_basic_faucet(Auth::IncrNonce, "USDC", 1_000_000, None)?;
    let merchant = builder.add_existing_wallet(Auth::Noop)?;
    let user = builder.add_existing_wallet(Auth::Noop)?;

    let balance = 1000u64;
    let amount = 100u64;
    let expiry = 1_000_000u32;

    let asset = FungibleAsset::new(faucet.id(), balance)?;
    // 7-item storage: [agent_pk(4), user_suffix, user_prefix, expiry]
    let storage = NoteStorage::new(vec![
        agent_pk[0], agent_pk[1], agent_pk[2], agent_pk[3],
        user.id().suffix(), user.id().prefix().as_felt(),
        Felt::new(expiry as u64),
    ])?;

    let serial_num: Word = [Felt::new(77), Felt::new(88), Felt::new(99), Felt::new(111)].into();
    let metadata = NoteMetadata::new(consumer.id(), NoteType::Public).with_tag(NoteTag::new(0));
    let vault = NoteAssets::new(vec![Asset::Fungible(asset)])?;
    let recipient = NoteRecipient::new(serial_num, note_script.clone(), storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();

    builder.add_output_note(RawOutputNote::Full(note));
    let mut mock_chain = builder.build()?;
    mock_chain.prove_next_block()?;

    // note_args: [merchant_suffix, merchant_prefix, amount, 0]
    let note_args: Word = [
        merchant.id().suffix(), merchant.id().prefix().as_felt(),
        Felt::new(amount), Felt::ZERO,
    ].into();

    // message = merge(serial_num, note_args)
    let message: Word = Hasher::merge(&[serial_num.into(), note_args.into()]).into();

    // Sign and put in advice map
    let sig = agent_sk.sign(message);
    let prepared = sig.to_prepared_signature(message);
    let sig_key: Word = Hasher::merge(&[agent_pk.into(), message.into()]).into();

    let advice = AdviceInputs::default().with_map([(sig_key, prepared)]);

    let mut args = BTreeMap::new();
    args.insert(note_id, note_args);

    let tx = mock_chain
        .build_tx_context(consumer.id(), &[note_id], &[])?
        .extend_note_args(args)
        .add_note_script(note_script)
        .extend_advice_inputs(advice)
        .build()?;

    let executed = tx.execute().await?;
    // ADN produces 2 output notes: P2ID to merchant + remainder ADN
    assert_eq!(executed.output_notes().num_notes(), 2, "expected P2ID + remainder");
    println!("PASSED: actual ADN MASM works in MockChain (2 output notes)");
    Ok(())
}
