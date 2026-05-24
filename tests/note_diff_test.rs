//! Compare the note from setup-testnet vs a freshly created one.
//! Field-by-field comparison to find what differs.

use miden_protocol::account::auth::AuthSecretKey;
use miden_protocol::asset::{Asset, FungibleAsset};
use miden_protocol::note::*;
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_protocol::{Felt, Word};
use miden_standards::code_builder::CodeBuilder;

const ADN_MASM: &str = include_str!("../masm/agent_debit_note.masm");

#[test]
fn compare_notes() {
    let setup_dir = match std::env::var("SETUP_DIR") {
        Ok(d) => d,
        Err(_) => { eprintln!("SETUP_DIR not set"); return; }
    };
    let setup_dir = std::path::PathBuf::from(setup_dir);
    let setup_toml: toml::Value = toml::from_str(
        &std::fs::read_to_string(setup_dir.join("setup.toml")).unwrap()
    ).unwrap();

    // Read the setup-testnet note
    let file_bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        std::fs::read_to_string(setup_dir.join("adn_note.b64")).unwrap().trim()
    ).unwrap();
    let file_note = Note::read_from_bytes(&file_bytes).unwrap();

    // Read agent key to get PK
    let agent_key_path = setup_dir.join(
        setup_toml["agents"].as_array().unwrap()[0].get("hot_key_path").unwrap().as_str().unwrap()
    );
    let sk_bytes = std::fs::read(&agent_key_path).unwrap();
    let falcon_sk = miden_protocol::crypto::dsa::falcon512_poseidon2::SecretKey::read_from_bytes(&sk_bytes).unwrap();
    let agent_sk = AuthSecretKey::Falcon512Poseidon2(falcon_sk);
    let agent_pk: Word = agent_sk.public_key().to_commitment().into();

    // Build a fresh note with the same parameters
    let note_script = CodeBuilder::default().compile_note_script(ADN_MASM).unwrap();

    let faucet_id_hex = setup_toml["faucet_id_hex"].as_str().unwrap();
    let faucet_id = miden_protocol::account::AccountId::from_hex(faucet_id_hex).unwrap();

    // Use the file note's exact parameters
    let file_storage = file_note.recipient().storage();
    let file_serial = file_note.recipient().serial_num();
    let file_metadata = file_note.metadata();

    eprintln!("=== FILE NOTE ===");
    eprintln!("id:            {}", file_note.id());
    eprintln!("serial_num:    {:?}", file_serial);
    eprintln!("script_root:   {:?}", file_note.recipient().script().root());
    eprintln!("storage_items: {}", file_storage.num_items());
    eprintln!("storage_commitment: {:?}", file_storage.commitment());
    eprintln!("storage_elements: {:?}", file_storage.to_elements());
    eprintln!("assets_count:  {}", file_note.assets().num_assets());
    for a in file_note.assets().iter() {
        eprintln!("  asset: {:?}", a);
    }
    eprintln!("sender:        {}", file_metadata.sender());
    eprintln!("note_type:     {:?}", file_metadata.note_type());
    eprintln!("tag:           {:?}", file_metadata.tag());
    eprintln!("attachment:    {:?}", file_metadata.attachment());
    eprintln!("recipient_digest: {:?}", file_note.recipient().digest());

    // Build the SAME note from scratch
    let fresh_storage = NoteStorage::new(file_storage.to_elements()).unwrap();
    let fresh_recipient = NoteRecipient::new(file_serial, note_script.clone(), fresh_storage.clone());
    let fresh_vault = file_note.assets().clone();
    let fresh_metadata = NoteMetadata::new(file_metadata.sender(), file_metadata.note_type())
        .with_tag(file_metadata.tag());
    let fresh_note = Note::new(fresh_vault, fresh_metadata, fresh_recipient);

    eprintln!("\n=== FRESH NOTE (same params) ===");
    eprintln!("id:            {}", fresh_note.id());
    eprintln!("serial_num:    {:?}", fresh_note.recipient().serial_num());
    eprintln!("script_root:   {:?}", fresh_note.recipient().script().root());
    eprintln!("storage_items: {}", fresh_note.recipient().storage().num_items());
    eprintln!("storage_commitment: {:?}", fresh_note.recipient().storage().commitment());
    eprintln!("recipient_digest: {:?}", fresh_note.recipient().digest());
    eprintln!("tag:           {:?}", fresh_note.metadata().tag());

    // Compare field by field
    eprintln!("\n=== COMPARISON ===");
    eprintln!("script_root match:      {}", file_note.recipient().script().root() == fresh_note.recipient().script().root());
    eprintln!("serial_num match:       {}", file_serial == fresh_note.recipient().serial_num());
    eprintln!("storage_commitment match: {}", file_storage.commitment() == fresh_note.recipient().storage().commitment());
    eprintln!("recipient_digest match: {}", file_note.recipient().digest() == fresh_note.recipient().digest());
    eprintln!("note_id match:          {}", file_note.id() == fresh_note.id());
    eprintln!("assets match:           {}", file_note.assets().commitment() == fresh_note.assets().commitment());
    eprintln!("metadata sender match:  {}", file_metadata.sender() == fresh_note.metadata().sender());
    eprintln!("metadata tag match:     {}", file_metadata.tag() == fresh_note.metadata().tag());
    eprintln!("metadata type match:    {}", file_metadata.note_type() == fresh_note.metadata().note_type());

    // Compare serialized bytes
    let file_rebytes = file_note.to_bytes();
    let fresh_bytes = fresh_note.to_bytes();
    eprintln!("\nfile_note bytes:  {} bytes", file_rebytes.len());
    eprintln!("fresh_note bytes: {} bytes", fresh_bytes.len());
    eprintln!("bytes match:      {}", file_rebytes == fresh_bytes);

    if file_rebytes != fresh_bytes {
        for (i, (a, b)) in file_rebytes.iter().zip(fresh_bytes.iter()).enumerate() {
            if a != b {
                eprintln!("FIRST DIFF at byte {}: file=0x{:02x} fresh=0x{:02x}", i, a, b);
                eprintln!("  context file:  {:02x?}", &file_rebytes[i.saturating_sub(8)..=(i+8).min(file_rebytes.len()-1)]);
                eprintln!("  context fresh: {:02x?}", &fresh_bytes[i.saturating_sub(8)..=(i+8).min(fresh_bytes.len()-1)]);
                break;
            }
        }
        if file_rebytes.len() != fresh_bytes.len() {
            eprintln!("LENGTH DIFFERS: file={} fresh={}", file_rebytes.len(), fresh_bytes.len());
        }
    }
}
