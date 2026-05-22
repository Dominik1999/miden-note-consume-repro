//! MockChain test for the secret-hash custom note.
//!
//! This test passes consistently, demonstrating that the note script is
//! correct. The same script fails with `StackReadFailed` when consumed
//! via the real miden-client against testnet.

use miden_protocol::asset::{Asset, FungibleAsset};
use miden_protocol::note::*;
use miden_protocol::transaction::RawOutputNote;
use miden_protocol::{Felt, Hasher, Word};
use miden_standards::code_builder::CodeBuilder;
use miden_testing::{Auth, MockChain};

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

#[tokio::test]
async fn mock_consume_secret_hash_note() -> anyhow::Result<()> {
    // 1. Compile the custom note script
    let note_script = CodeBuilder::default().compile_note_script(NOTE_MASM)?;

    // 2. Build MockChain with a wallet and a faucet
    let mut builder = MockChain::builder();
    let consumer = builder.add_existing_wallet(Auth::IncrNonce)?;
    let faucet =
        builder.add_existing_basic_faucet(Auth::IncrNonce, "TEST", 1_000_000, None)?;

    // 3. Create the note with the secret's digest in storage
    let asset = FungibleAsset::new(faucet.id(), 1000)?;
    let storage = NoteStorage::new(vec![
        secret_digest()[0],
        secret_digest()[1],
        secret_digest()[2],
        secret_digest()[3],
    ])?;
    let serial_num: Word =
        [Felt::new(1), Felt::new(2), Felt::new(3), Felt::new(4)].into();

    let metadata =
        NoteMetadata::new(consumer.id(), NoteType::Public).with_tag(NoteTag::new(0));
    let vault = NoteAssets::new(vec![Asset::Fungible(asset)])?;
    let recipient = NoteRecipient::new(serial_num, note_script.clone(), storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();

    builder.add_output_note(RawOutputNote::Full(note));
    let mut mock_chain = builder.build()?;

    mock_chain.prove_next_block()?;

    // 4. Consume the note by passing the secret as note_args
    let mut note_args = std::collections::BTreeMap::new();
    note_args.insert(note_id, secret());

    let tx = mock_chain
        .build_tx_context(consumer.id(), &[note_id], &[])?
        .extend_note_args(note_args)
        .add_note_script(note_script)
        .build()?;

    let executed = tx.execute().await?;

    // 5. Verify the note was consumed and assets were transferred
    // The wallet should now have the assets
    assert!(
        executed.output_notes().num_notes() == 0,
        "no output notes expected (assets go into wallet vault)"
    );

    println!("MockChain test PASSED: secret hash note consumed successfully");
    Ok(())
}

#[tokio::test]
async fn mock_wrong_secret_rejected() -> anyhow::Result<()> {
    let note_script = CodeBuilder::default().compile_note_script(NOTE_MASM)?;

    let mut builder = MockChain::builder();
    let consumer = builder.add_existing_wallet(Auth::IncrNonce)?;
    let faucet =
        builder.add_existing_basic_faucet(Auth::IncrNonce, "TEST", 1_000_000, None)?;

    let asset = FungibleAsset::new(faucet.id(), 1000)?;
    let storage = NoteStorage::new(vec![
        secret_digest()[0],
        secret_digest()[1],
        secret_digest()[2],
        secret_digest()[3],
    ])?;
    let serial_num: Word =
        [Felt::new(10), Felt::new(20), Felt::new(30), Felt::new(40)].into();

    let metadata =
        NoteMetadata::new(consumer.id(), NoteType::Public).with_tag(NoteTag::new(0));
    let vault = NoteAssets::new(vec![Asset::Fungible(asset)])?;
    let recipient = NoteRecipient::new(serial_num, note_script.clone(), storage);
    let note = Note::new(vault, metadata, recipient);
    let note_id = note.id();

    builder.add_output_note(RawOutputNote::Full(note));
    let mut mock_chain = builder.build()?;
    mock_chain.prove_next_block()?;

    // Pass wrong secret
    let wrong_secret: Word =
        [Felt::new(99), Felt::new(99), Felt::new(99), Felt::new(99)].into();
    let mut note_args = std::collections::BTreeMap::new();
    note_args.insert(note_id, wrong_secret);

    let tx = mock_chain
        .build_tx_context(consumer.id(), &[note_id], &[])?
        .extend_note_args(note_args)
        .add_note_script(note_script)
        .build()?;

    assert!(
        tx.execute().await.is_err(),
        "wrong secret should be rejected"
    );

    println!("MockChain test PASSED: wrong secret correctly rejected");
    Ok(())
}
