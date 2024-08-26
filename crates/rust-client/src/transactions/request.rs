use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::{String, ToString},
    vec::Vec,
};
use core::fmt;

use miden_lib::notes::{create_p2id_note, create_p2idr_note, create_swap_note};
use miden_objects::{
    accounts::AccountId,
    assembly::AssemblyError,
    assets::{Asset, FungibleAsset},
    crypto::{
        merkle::{InnerNodeInfo, MerkleStore},
        rand::FeltRng,
    },
    notes::{Note, NoteDetails, NoteId, NoteType, PartialNote},
    transaction::{OutputNote, TransactionArgs, TransactionScript},
    vm::AdviceMap,
    Digest, Felt, FieldElement, NoteError, Word,
};
use miden_tx::utils::{ByteReader, ByteWriter, Deserializable, DeserializationError, Serializable};

// TRANSACTION REQUEST
// ================================================================================================

pub type NoteArgs = Word;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransactionScriptTemplate {
    CustomScript(TransactionScript),
    SendNotes(Vec<PartialNote>),
}

/// A request for executing a transaction against a specific account.
///
/// A request contains information about input notes to be consumed by the transaction (if any),
/// description of the transaction script to be executed (if any), and a set of notes expected
/// to be generated by the transaction or by consuming notes generated by the transaction.
#[derive(Clone, Debug)]
pub struct TransactionRequest {
    // Notes to be consumed by the transaction that are not authenticated.
    unauthenticated_input_notes: Vec<Note>,
    /// Notes to be consumed by the transaction together with their (optional) arguments. This
    /// has to include both authenticated and unauthenticated notes.
    input_notes: BTreeMap<NoteId, Option<NoteArgs>>,
    /// Template for the creation of the transaction script.
    script_template: Option<TransactionScriptTemplate>,
    /// A map of notes expected to be generated by the transactions.
    expected_output_notes: BTreeMap<NoteId, Note>,
    /// A map of note details of notes we expect to be created as part of future transactions.
    expected_future_notes: BTreeMap<NoteId, NoteDetails>,
    /// Initial state of the `AdviceMap` that provides data during runtime.
    advice_map: AdviceMap,
    /// Initial state of the `MerkleStore` that provides data during runtime.
    merkle_store: MerkleStore,
}

impl TransactionRequest {
    // CONSTRUCTORS
    // --------------------------------------------------------------------------------------------

    pub fn new() -> Self {
        Self {
            unauthenticated_input_notes: vec![],
            input_notes: BTreeMap::new(),
            script_template: None,
            expected_output_notes: BTreeMap::new(),
            expected_future_notes: BTreeMap::new(),
            advice_map: AdviceMap::default(),
            merkle_store: MerkleStore::default(),
        }
    }

    /// Adds the specified notes as unauthenticated input notes to the transaction request.
    pub fn with_unauthenticated_input_notes(
        mut self,
        notes: impl IntoIterator<Item = (Note, Option<NoteArgs>)>,
    ) -> Self {
        for (note, argument) in notes {
            self.input_notes.insert(note.id(), argument);
            self.unauthenticated_input_notes.push(note);
        }
        self
    }

    /// Adds the specified notes as authenticated input notes to the transaction request.
    pub fn with_authenticated_input_notes(
        mut self,
        notes: impl IntoIterator<Item = (NoteId, Option<NoteArgs>)>,
    ) -> Self {
        for (note_id, argument) in notes {
            self.input_notes.insert(note_id, argument);
        }
        self
    }

    /// Specifies the output notes that should be created in the transaction script and will
    /// be used as a transaction script template. These notes will also be added the the expected
    /// output notes of the transaction.
    ///
    /// If a transaction script template is already set (e.g. by calling `with_custom_script`), this
    /// method will return an error.
    pub fn with_own_output_notes(
        mut self,
        notes: impl IntoIterator<Item = OutputNote>,
    ) -> Result<Self, TransactionRequestError> {
        if self.script_template.is_some() {
            return Err(TransactionRequestError::ScriptTemplateError(
                "Cannot set own notes when a script template is already set".to_string(),
            ));
        }

        let mut own_notes = Vec::new();

        for note in notes {
            match note {
                OutputNote::Full(note) => {
                    self.expected_output_notes.insert(note.id(), note.clone());
                    own_notes.push(note.into());
                },
                OutputNote::Partial(note) => own_notes.push(note),
                OutputNote::Header(_) => return Err(TransactionRequestError::InvalidNoteVariant),
            }
        }

        self.script_template = Some(TransactionScriptTemplate::SendNotes(own_notes));
        Ok(self)
    }

    /// Specifies a custom transaction script to be used.
    ///
    /// If a script template is already set (e.g. by calling `with_own_output_notes`), this method
    /// will return an error.
    pub fn with_custom_script(
        mut self,
        script: TransactionScript,
    ) -> Result<Self, TransactionRequestError> {
        if self.script_template.is_some() {
            return Err(TransactionRequestError::ScriptTemplateError(
                "Cannot set custom script when a script template is already set".to_string(),
            ));
        }
        self.script_template = Some(TransactionScriptTemplate::CustomScript(script));
        Ok(self)
    }

    pub fn with_expected_output_notes(mut self, notes: Vec<Note>) -> Self {
        self.expected_output_notes =
            BTreeMap::from_iter(notes.into_iter().map(|note| (note.id(), note)));
        self
    }

    pub fn with_expected_future_notes(mut self, notes: Vec<NoteDetails>) -> Self {
        self.expected_future_notes =
            BTreeMap::from_iter(notes.into_iter().map(|note| (note.id(), note)));
        self
    }

    pub fn extend_advice_map<T: IntoIterator<Item = (Digest, Vec<Felt>)>>(
        mut self,
        iter: T,
    ) -> Self {
        self.advice_map.extend(iter);
        self
    }

    pub fn extend_merkle_store<T: IntoIterator<Item = InnerNodeInfo>>(mut self, iter: T) -> Self {
        self.merkle_store.extend(iter);
        self
    }

    // STANDARDIZED REQUESTS
    // --------------------------------------------------------------------------------------------

    /// Returns a new [TransactionRequest] for a transaction to consume the specified notes.
    ///
    /// - `note_ids` is a list of note IDs to be consumed.
    pub fn consume_notes(note_ids: Vec<NoteId>) -> Self {
        let input_notes = note_ids.into_iter().map(|id| (id, None));
        Self::new().with_authenticated_input_notes(input_notes)
    }

    /// Returns a new [TransactionRequest] for a transaction to mint fungible assets. This request
    /// must be executed against a fungible faucet account.
    ///
    /// - `asset` is the fungible asset to be minted.
    /// - `target_id` is the account ID of the account to receive the minted asset.
    /// - `note_type` determines the visibility of the note to be created.
    /// - `rng` is the random number generator used to generate the serial number for the created
    ///   note.
    pub fn mint_fungible_asset(
        asset: FungibleAsset,
        target_id: AccountId,
        note_type: NoteType,
        rng: &mut impl FeltRng,
    ) -> Result<Self, TransactionRequestError> {
        let created_note = create_p2id_note(
            asset.faucet_id(),
            target_id,
            vec![asset.into()],
            note_type,
            Felt::ZERO,
            rng,
        )?;

        TransactionRequest::new().with_own_output_notes(vec![OutputNote::Full(created_note)])
    }

    /// Returns a new [TransactionRequest] for a transaction to send a P2ID or P2IDR note. This
    /// request must be executed against the wallet sender account.
    ///
    /// - `payment_data` is the data for the payment transaction that contains the asset to be
    ///   transferred, the sender account ID, and the target account ID.
    /// - `recall_height` is the block height after which the sender can recall the assets. If None,
    ///   a P2ID note is created. If Some(), a P2IDR note is created.
    /// - `note_type` determines the visibility of the note to be created.
    /// - `rng` is the random number generator used to generate the serial number for the created
    ///   note.
    pub fn pay_to_id(
        payment_data: PaymentTransactionData,
        recall_height: Option<u32>,
        note_type: NoteType,
        rng: &mut impl FeltRng,
    ) -> Result<Self, TransactionRequestError> {
        let created_note = if let Some(recall_height) = recall_height {
            create_p2idr_note(
                payment_data.account_id(),
                payment_data.target_account_id(),
                vec![payment_data.asset()],
                note_type,
                Felt::ZERO,
                recall_height,
                rng,
            )?
        } else {
            create_p2id_note(
                payment_data.account_id(),
                payment_data.target_account_id(),
                vec![payment_data.asset()],
                note_type,
                Felt::ZERO,
                rng,
            )?
        };

        TransactionRequest::new().with_own_output_notes(vec![OutputNote::Full(created_note)])
    }

    /// Returns a new [TransactionRequest] for a transaction to send a SWAP note. This request must
    /// be executed against the wallet sender account.
    ///
    /// - `swap_data` is the data for the swap transaction that contains the sender account ID, the
    ///   offered asset, and the requested asset.
    /// - `note_type` determines the visibility of the note to be created.
    /// - `rng` is the random number generator used to generate the serial number for the created
    ///   note.
    pub fn swap(
        swap_data: SwapTransactionData,
        note_type: NoteType,
        rng: &mut impl FeltRng,
    ) -> Result<Self, TransactionRequestError> {
        // The created note is the one that we need as the output of the tx, the other one is the
        // one that we expect to receive and consume eventually
        let (created_note, payback_note_details) = create_swap_note(
            swap_data.account_id(),
            swap_data.offered_asset(),
            swap_data.requested_asset(),
            note_type,
            Felt::ZERO,
            rng,
        )?;

        TransactionRequest::new()
            .with_expected_future_notes(vec![payback_note_details])
            .with_own_output_notes(vec![OutputNote::Full(created_note)])
    }

    // PUBLIC ACCESSORS
    // --------------------------------------------------------------------------------------------

    pub fn unauthenticated_input_notes(&self) -> &[Note] {
        &self.unauthenticated_input_notes
    }

    pub fn unauthenticated_input_note_ids(&self) -> impl Iterator<Item = NoteId> + '_ {
        self.unauthenticated_input_notes.iter().map(|note| note.id())
    }

    pub fn authenticated_input_note_ids(&self) -> impl Iterator<Item = NoteId> + '_ {
        let unauthenticated_note_ids: BTreeSet<NoteId> =
            BTreeSet::from_iter(self.unauthenticated_input_note_ids());

        self.input_notes()
            .iter()
            .map(|(note_id, _)| *note_id)
            .filter(move |note_id| !unauthenticated_note_ids.contains(note_id))
    }

    pub fn input_notes(&self) -> &BTreeMap<NoteId, Option<NoteArgs>> {
        &self.input_notes
    }

    pub fn get_input_note_ids(&self) -> Vec<NoteId> {
        self.input_notes.keys().cloned().collect()
    }

    pub fn get_note_args(&self) -> BTreeMap<NoteId, NoteArgs> {
        self.input_notes
            .iter()
            .filter_map(|(note, args)| args.map(|a| (*note, a)))
            .collect()
    }

    pub fn expected_output_notes(&self) -> impl Iterator<Item = &Note> {
        self.expected_output_notes.values()
    }

    pub fn expected_future_notes(&self) -> impl Iterator<Item = &NoteDetails> {
        self.expected_future_notes.values()
    }

    pub fn script_template(&self) -> &Option<TransactionScriptTemplate> {
        &self.script_template
    }

    pub fn advice_map(&self) -> &AdviceMap {
        &self.advice_map
    }

    pub fn merkle_store(&self) -> &MerkleStore {
        &self.merkle_store
    }

    pub(super) fn into_transaction_args(self, tx_script: TransactionScript) -> TransactionArgs {
        let note_args = self.get_note_args();
        let TransactionRequest {
            expected_output_notes,
            advice_map,
            merkle_store,
            ..
        } = self;

        let mut tx_args = TransactionArgs::new(Some(tx_script), note_args.into(), advice_map);

        tx_args.extend_expected_output_notes(expected_output_notes.into_values());
        tx_args.extend_merkle_store(merkle_store.inner_nodes());

        tx_args
    }
}

impl Serializable for TransactionRequest {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.unauthenticated_input_notes.write_into(target);
        self.input_notes.write_into(target);
        match &self.script_template {
            None => target.write_u8(0),
            Some(TransactionScriptTemplate::CustomScript(script)) => {
                target.write_u8(1);
                script.write_into(target);
            },
            Some(TransactionScriptTemplate::SendNotes(notes)) => {
                target.write_u8(2);
                notes.write_into(target);
            },
        }
        self.expected_output_notes.write_into(target);
        self.expected_future_notes.write_into(target);
        self.advice_map.clone().into_iter().collect::<Vec<_>>().write_into(target);
        self.merkle_store.write_into(target);
    }
}

impl Deserializable for TransactionRequest {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let unauthenticated_input_notes = Vec::<Note>::read_from(source)?;
        let input_notes = BTreeMap::<NoteId, Option<NoteArgs>>::read_from(source)?;

        let script_template = match source.read_u8()? {
            0 => None,
            1 => {
                let transaction_script = TransactionScript::read_from(source)?;
                Some(TransactionScriptTemplate::CustomScript(transaction_script))
            },
            2 => {
                let notes = Vec::<PartialNote>::read_from(source)?;
                Some(TransactionScriptTemplate::SendNotes(notes))
            },
            _ => {
                return Err(DeserializationError::InvalidValue(
                    "Invalid script template type".to_string(),
                ))
            },
        };

        let expected_output_notes = BTreeMap::<NoteId, Note>::read_from(source)?;
        let expected_future_notes = BTreeMap::<NoteId, NoteDetails>::read_from(source)?;

        let mut advice_map = AdviceMap::new();
        let advice_vec = Vec::<(Digest, Vec<Felt>)>::read_from(source)?;
        advice_map.extend(advice_vec);
        let merkle_store = MerkleStore::read_from(source)?;

        Ok(TransactionRequest {
            unauthenticated_input_notes,
            input_notes,
            script_template,
            expected_output_notes,
            expected_future_notes,
            advice_map,
            merkle_store,
        })
    }
}

impl PartialEq for TransactionRequest {
    fn eq(&self, other: &Self) -> bool {
        let same_advice_map_count = self.advice_map.clone().into_iter().count()
            == other.advice_map.clone().into_iter().count();
        let same_advice_map = same_advice_map_count
            && self
                .advice_map
                .clone()
                .into_iter()
                .all(|elem| other.advice_map.get(&elem.0).map_or(false, |v| v == elem.1));

        // TODO: Simplify this. Because [TransactionScript] does not deserialize exactly into the
        // original object, they are not directly comparable right now
        let same_script = match &self.script_template {
            Some(TransactionScriptTemplate::CustomScript(script)) => {
                if let Some(TransactionScriptTemplate::CustomScript(other_script)) =
                    &other.script_template
                {
                    other_script.hash() == script.hash()
                } else {
                    false
                }
            },
            Some(TransactionScriptTemplate::SendNotes(_)) => {
                self.script_template == other.script_template
            },
            None => other.script_template.is_none(),
        };

        same_script
            && self.unauthenticated_input_notes == other.unauthenticated_input_notes
            && self.input_notes == other.input_notes
            && self.expected_output_notes == other.expected_output_notes
            && self.expected_future_notes == other.expected_future_notes
            && same_advice_map
            && self.merkle_store == other.merkle_store
    }
}

impl Default for TransactionRequest {
    fn default() -> Self {
        Self::new()
    }
}

// TRANSACTION REQUEST ERROR
// ================================================================================================

#[derive(Debug)]
pub enum TransactionRequestError {
    InputNoteNotAuthenticated,
    InputNotesMapMissingUnauthenticatedNotes,
    InvalidNoteVariant,
    InvalidSenderAccount(AccountId),
    InvalidTransactionScript(AssemblyError),
    NoInputNotes,
    ScriptTemplateError(String),
    NoteNotFound(String),
    NoteCreationError(NoteError),
}
impl fmt::Display for TransactionRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputNoteNotAuthenticated => write!(f, "Every authenticated note to be consumed should be committed and contain a valid inclusion proof"),
            Self::InputNotesMapMissingUnauthenticatedNotes => write!(f, "The input notes map should include keys for all provided unauthenticated input notes"),
            Self::InvalidNoteVariant => write!(f, "Own notes should be either full or partial, but not header"),
            Self::InvalidSenderAccount(account_id) => write!(f, "Invalid sender account ID: {}", account_id),
            Self::InvalidTransactionScript(err) => write!(f, "Invalid transaction script: {}", err),
            Self::NoInputNotes => write!(f, "A transaction without output notes must have at least one input note"),
            Self::ScriptTemplateError(err) => write!(f, "Transaction script template error: {}", err),
            Self::NoteNotFound(err) => write!(f, "Note not found: {}", err),
            Self::NoteCreationError(err) => write!(f, "Note creation error: {}", err),
        }
    }
}

impl From<NoteError> for TransactionRequestError {
    fn from(err: NoteError) -> Self {
        Self::NoteCreationError(err)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for TransactionRequestError {}

// PAYMENT TRANSACTION DATA
// ================================================================================================

#[derive(Clone, Debug)]
pub struct PaymentTransactionData {
    asset: Asset,
    sender_account_id: AccountId,
    target_account_id: AccountId,
}

impl PaymentTransactionData {
    // CONSTRUCTORS
    // --------------------------------------------------------------------------------------------

    pub fn new(
        asset: Asset,
        sender_account_id: AccountId,
        target_account_id: AccountId,
    ) -> PaymentTransactionData {
        PaymentTransactionData {
            asset,
            sender_account_id,
            target_account_id,
        }
    }

    /// Returns the executor [AccountId]
    pub fn account_id(&self) -> AccountId {
        self.sender_account_id
    }

    /// Returns the target [AccountId]
    pub fn target_account_id(&self) -> AccountId {
        self.target_account_id
    }

    /// Returns the transaction [Asset]
    pub fn asset(&self) -> Asset {
        self.asset
    }
}

// SWAP TRANSACTION DATA
// ================================================================================================

#[derive(Clone, Debug)]
pub struct SwapTransactionData {
    sender_account_id: AccountId,
    offered_asset: Asset,
    requested_asset: Asset,
}

impl SwapTransactionData {
    // CONSTRUCTORS
    // --------------------------------------------------------------------------------------------

    pub fn new(
        sender_account_id: AccountId,
        offered_asset: Asset,
        requested_asset: Asset,
    ) -> SwapTransactionData {
        SwapTransactionData {
            sender_account_id,
            offered_asset,
            requested_asset,
        }
    }

    /// Returns the executor [AccountId]
    pub fn account_id(&self) -> AccountId {
        self.sender_account_id
    }

    /// Returns the transaction offered [Asset]
    pub fn offered_asset(&self) -> Asset {
        self.offered_asset
    }

    /// Returns the transaction requested [Asset]
    pub fn requested_asset(&self) -> Asset {
        self.requested_asset
    }
}

// KNOWN SCRIPT ROOTS
// ================================================================================================

// TODO: Remove this in favor of precompiled scripts
pub mod known_script_roots {
    pub const P2ID: &str = "0x39b8d330926f2617d631191af4566f953e39cd7b461ae4ede7cc4fde9b9c8de7";
    pub const P2IDR: &str = "0x0355e580bd492cc03ec7f779b58041f5de68d7fe3a4843cd5623554acfbc862b";
    pub const SWAP: &str = "0x76fbfd9b74214b9216ec1d50d0b864393e2e550a84b7737b28bbe4f2d5e85d77";
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use alloc::string::ToString;
    use std::vec::Vec;

    use miden_lib::notes::{create_p2id_note, create_p2idr_note, create_swap_note};
    use miden_objects::{
        accounts::{
            account_id::testing::{
                ACCOUNT_ID_FUNGIBLE_FAUCET_OFF_CHAIN, ACCOUNT_ID_FUNGIBLE_FAUCET_ON_CHAIN,
            },
            AccountId, AccountType,
        },
        assets::FungibleAsset,
        crypto::rand::{FeltRng, RpoRandomCoin},
        notes::NoteType,
        transaction::OutputNote,
        Digest, Felt, FieldElement, ZERO,
    };
    use miden_tx::utils::{Deserializable, Serializable};

    use super::TransactionRequest;
    use crate::transactions::known_script_roots::{P2ID, P2IDR, SWAP};

    // We need to make sure the script roots we use for filters are in line with the note scripts
    // coming from Miden objects
    #[test]
    fn ensure_correct_script_roots() {
        // create dummy data for the notes
        let faucet_id: AccountId = ACCOUNT_ID_FUNGIBLE_FAUCET_ON_CHAIN.try_into().unwrap();
        let account_id: AccountId = ACCOUNT_ID_FUNGIBLE_FAUCET_OFF_CHAIN.try_into().unwrap();
        let mut rng = RpoRandomCoin::new(Default::default());

        // create dummy notes to compare note script roots
        let p2id_note = create_p2id_note(
            account_id,
            account_id,
            vec![FungibleAsset::new(faucet_id, 100u64).unwrap().into()],
            NoteType::Private,
            Felt::ZERO,
            &mut rng,
        )
        .unwrap();
        let p2idr_note = create_p2idr_note(
            account_id,
            account_id,
            vec![FungibleAsset::new(faucet_id, 100u64).unwrap().into()],
            NoteType::Private,
            Felt::ZERO,
            10,
            &mut rng,
        )
        .unwrap();
        let (swap_note, _serial_num) = create_swap_note(
            account_id,
            FungibleAsset::new(faucet_id, 100u64).unwrap().into(),
            FungibleAsset::new(faucet_id, 100u64).unwrap().into(),
            NoteType::Private,
            Felt::ZERO,
            &mut rng,
        )
        .unwrap();

        assert_eq!(p2id_note.script().hash().to_string(), P2ID);
        assert_eq!(p2idr_note.script().hash().to_string(), P2IDR);
        assert_eq!(swap_note.script().hash().to_string(), SWAP);
    }

    #[test]
    fn transaction_request_serialization() {
        let sender_id = AccountId::new_dummy([0u8; 32], AccountType::RegularAccountImmutableCode);
        let target_id = AccountId::new_dummy([1u8; 32], AccountType::RegularAccountImmutableCode);
        let faucet_id = AccountId::new_dummy([2u8; 32], AccountType::FungibleFaucet);
        let mut rng = RpoRandomCoin::new(Default::default());

        let mut notes = vec![];
        for i in 0..6 {
            let note = create_p2id_note(
                sender_id,
                target_id,
                vec![FungibleAsset::new(faucet_id, 100 + i).unwrap().into()],
                NoteType::Private,
                ZERO,
                &mut rng,
            )
            .unwrap();
            notes.push(note);
        }

        let mut advice_vec: Vec<(Digest, Vec<Felt>)> = vec![];
        for i in 0..10 {
            advice_vec.push((Digest::new(rng.draw_word()), vec![Felt::new(i)]));
        }

        // This transaction request wouldn't be valid in a real scenario, it's intended for testing
        let tx_request = TransactionRequest::new()
            .with_authenticated_input_notes(vec![(notes.pop().unwrap().id(), None)])
            .with_unauthenticated_input_notes(vec![(notes.pop().unwrap(), None)])
            .with_expected_output_notes(vec![notes.pop().unwrap()])
            .with_expected_future_notes(vec![notes.pop().unwrap().into()])
            .extend_advice_map(advice_vec)
            .with_own_output_notes(vec![
                OutputNote::Full(notes.pop().unwrap()),
                OutputNote::Partial(notes.pop().unwrap().into()),
            ])
            .unwrap();

        let mut buffer = Vec::new();
        tx_request.write_into(&mut buffer);

        let deserialized_tx_request = TransactionRequest::read_from_bytes(&buffer).unwrap();
        assert_eq!(tx_request, deserialized_tx_request);
    }
}
