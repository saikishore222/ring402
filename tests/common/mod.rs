#![allow(dead_code)]

use ring402::Quote;
use solana_signature::Signature;
use spr_keypair::{ShieldedKeypair, SigningKey, ViewingKey, X25519Pubkey};
use spr_transaction::{
    instructions::transact::{ConfidentialTransaction, Shape},
    Address, Data, Mint, OutputContext, OutputSlot, ShieldedTransaction, Utxo, WalletUtxo,
};

pub const TOKEN: Mint = Mint::new(Address::new_from_array([9; 32]), 59);

const TREE: u16 = 7;

pub fn keypair(seed: u8) -> ShieldedKeypair {
    ShieldedKeypair::from_keypair(SigningKey::from_ed25519_bytes(&[seed; 32])).unwrap()
}

pub fn quote(merchant: &ShieldedKeypair, amount: u64) -> Quote {
    Quote {
        id: [0xAB; 16],
        asset: Mint::SOL,
        amount,
        pay_to: merchant.shielded_address().unwrap(),
    }
}

pub fn pay(
    agent: &ShieldedKeypair,
    quote: &Quote,
    balance: u64,
) -> (ShieldedTransaction, ViewingKey) {
    let sender = agent.shielded_address().unwrap();
    let mut tx = ConfidentialTransaction::new(
        vec![funded(agent, quote.asset, balance)],
        sender.solana_address().unwrap(),
    )
    .unwrap();
    let tx_key = agent
        .viewing_key
        .get_transaction_viewing_key(tx.first_nullifier())
        .unwrap();
    tx.add_output_utxo(quote.output().unwrap()).unwrap();
    tx.pad_utxos(Shape::IN1_OUT2, &sender).unwrap();
    let proof = tx.encrypt(agent).unwrap();
    let published = proof.external_data;
    let indexed = ShieldedTransaction {
        slot: 2,
        tx_signature: Signature::from([2; 64]),
        event_index: None,
        tx_viewing_pk: Some(X25519Pubkey::from_bytes(published.tx_viewing_pk).unwrap()),
        salt: Some(published.salt),
        output_slots: published
            .outputs
            .into_iter()
            .zip(0..)
            .map(|(output, leaf_index)| OutputSlot {
                view_tag: [0; 32],
                output_context: OutputContext {
                    hash: output.utxo_hash,
                    tree_id: proof.output_tree_id,
                    leaf_index,
                },
                payload: output.data.unwrap_or_default(),
            })
            .collect(),
        messages: Vec::new(),
        nullifiers: Vec::new(),
        proofless: false,
        ring_config: None,
        ring_program_id: None,
    };
    (indexed, tx_key)
}

fn funded(owner: &ShieldedKeypair, asset: Mint, amount: u64) -> WalletUtxo {
    let mut blinding = [0; 32];
    blinding[31] = 1;
    let utxo = Utxo {
        owner: owner.signing_pubkey(),
        asset,
        amount,
        blinding,
        ring_program_id: None,
        data: Data::default(),
    };
    let nullifier_pubkey = owner.shielded_address().unwrap().nullifier_pubkey;
    let utxo_hash = utxo
        .hash(&nullifier_pubkey, &[0; 32], &[0; 32], TREE)
        .unwrap();
    let nullifier = owner.nullifier(&utxo_hash, &blinding).unwrap();
    WalletUtxo {
        utxo,
        nullifier_pubkey,
        utxo_hash,
        nullifier,
        tx_viewing_key: None,
        data_hash: None,
        ring_data_hash: None,
        tree_id: TREE,
        leaf_index: 1,
        latest_tree_id: None,
        slot: 1,
        tx_signature: Signature::from([1; 64]),
        slot_index: 0,
    }
}
