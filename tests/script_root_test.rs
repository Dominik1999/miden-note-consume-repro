//! Compare the script root from the setup-testnet note file
//! vs what CodeBuilder produces from the same MASM source.

use miden_protocol::note::Note;
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_standards::code_builder::CodeBuilder;

const ADN_MASM: &str = include_str!("../masm/agent_debit_note.masm");

#[test]
fn compare_script_roots() {
    let setup_dir = match std::env::var("SETUP_DIR") {
        Ok(d) => d,
        Err(_) => {
            eprintln!("SETUP_DIR not set, skipping");
            return;
        }
    };

    let note_bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        std::fs::read_to_string(format!("{setup_dir}/adn_note.b64")).unwrap().trim()
    ).unwrap();
    let note = Note::read_from_bytes(&note_bytes).unwrap();
    let file_root = note.recipient().script().root();

    let script = CodeBuilder::default().compile_note_script(ADN_MASM).unwrap();
    let compiled_root = script.root();

    eprintln!("File script_root:     {file_root:?}");
    eprintln!("Compiled script_root: {compiled_root:?}");
    eprintln!("Match: {}", file_root == compiled_root);

    // Also compare MAST forest sizes
    eprintln!("File MAST size:     {} bytes", note.recipient().script().mast().to_bytes().len());
    eprintln!("Compiled MAST size: {} bytes", script.mast().to_bytes().len());

    assert_eq!(file_root, compiled_root, "Script roots must match!");
}
