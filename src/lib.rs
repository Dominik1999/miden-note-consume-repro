/// Simple custom note: hash preimage check + add_assets_to_account.
/// This PASSES on real testnet with miden-client main.
pub const SIMPLE_NOTE_MASM: &str = include_str!("../masm/secret_hash_note.masm");

/// Complex custom note: Falcon signature verification + P2ID output note.
/// This FAILS with StackReadFailed on real testnet.
pub const FALCON_P2ID_NOTE_MASM: &str = include_str!("../masm/falcon_p2id_note.masm");
