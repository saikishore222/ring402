use spr_event::OutputDataEncoding;
use spr_keypair::{ShieldedAddress, ViewingKey};
use spr_transaction::{
    instructions::transact::SppProofOutputUtxo,
    serialization::confidential::{Confidential, ConfidentialOutputPlaintext},
    Address, AssetRegistry, DecodeCx, EncryptedScheme, Mint, OutputContext, ShieldedTransaction,
    TransactionError, UtxoSerialization,
};

pub mod wire;

#[cfg(feature = "helius")]
pub mod helius;

#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "client")]
pub mod client;

pub mod usdc {
    use super::Address;

    pub const MAINNET: Address =
        Address::from_str_const("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
    pub const DEVNET: Address =
        Address::from_str_const("4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU");
}

const MEMO_PREFIX: &[u8] = b"x402:";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no output in this transaction pays the quote")]
    NotPaid,
    #[error("paid {paid}, quote asks {asked}")]
    Underpaid { paid: u64, asked: u64 },
    #[error("output does not match its onchain commitment")]
    CommitmentMismatch,
    #[error(transparent)]
    Transaction(#[from] TransactionError),
}

#[derive(Clone, Debug)]
pub struct Quote {
    pub id: [u8; 16],
    pub asset: Mint,
    pub amount: u64,
    pub pay_to: ShieldedAddress,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payment {
    pub quote_id: [u8; 16],
    pub asset_id: u64,
    pub amount: u64,
    pub utxo_hash: [u8; 32],
}

pub enum Opener<'a> {
    Recipient(&'a ViewingKey),
    Disclosed(&'a ViewingKey),
}

impl Quote {
    pub fn memo(&self) -> Vec<u8> {
        [MEMO_PREFIX, &self.id].concat()
    }

    pub fn output(&self) -> Result<SppProofOutputUtxo, Error> {
        Ok(SppProofOutputUtxo::new(self.asset, self.amount, self.pay_to)?.with_memo(self.memo()))
    }

    pub fn settle(&self, tx: &ShieldedTransaction, opener: Opener<'_>) -> Result<Payment, Error> {
        let (Some(tx_viewing_pk), Some(salt)) = (tx.tx_viewing_pk, tx.salt) else {
            return Err(Error::NotPaid);
        };
        let memo = self.memo();
        let mut underpaid = None;

        for (index, slot) in tx.output_slots.iter().enumerate() {
            let Some(body) = slot.output_data().and_then(confidential_body) else {
                continue;
            };
            if Confidential::embedded_viewing_pk(&body).ok() != Some(self.pay_to.viewing_pubkey) {
                continue;
            }
            let slot_index = index as u32;
            let opened = match &opener {
                Opener::Recipient(viewing_key) => Confidential::decode(
                    &body,
                    &DecodeCx {
                        viewing_key,
                        tx_viewing_pk: Some(tx_viewing_pk),
                        salt: Some(salt),
                        slot_index,
                        first_nullifier: None,
                    },
                ),
                Opener::Disclosed(tx_key) => {
                    Confidential::decrypt_with_tx_key(tx_key, &body, salt, slot_index)
                }
            };
            let Ok(plaintext) = opened else { continue };
            if plaintext.data.memo() != Some(memo.as_slice())
                || plaintext.asset_id != self.asset.asset_id
            {
                continue;
            }
            if plaintext.amount < self.amount {
                underpaid = Some(plaintext.amount);
                continue;
            }
            return self.committed(plaintext, &slot.output_context);
        }

        Err(match underpaid {
            Some(paid) => Error::Underpaid {
                paid,
                asked: self.amount,
            },
            None => Error::NotPaid,
        })
    }

    fn committed(
        &self,
        plaintext: ConfidentialOutputPlaintext,
        onchain: &OutputContext,
    ) -> Result<Payment, Error> {
        let asset_id = plaintext.asset_id;
        let amount = plaintext.amount;
        let note = plaintext.into_utxo(self.pay_to.signing_pubkey, &registry(self.asset)?)?;
        let expected = note.hash(
            &self.pay_to.nullifier_pubkey,
            &[0; 32],
            &[0; 32],
            onchain.tree_id,
        )?;
        if expected != onchain.hash {
            return Err(Error::CommitmentMismatch);
        }
        Ok(Payment {
            quote_id: self.id,
            asset_id,
            amount,
            utxo_hash: onchain.hash,
        })
    }
}

pub fn registry(asset: Mint) -> Result<AssetRegistry, TransactionError> {
    if asset == Mint::SOL {
        return Ok(AssetRegistry::default());
    }
    AssetRegistry::new([(asset.asset_id, asset.asset)])
}

fn confidential_body(data: OutputDataEncoding) -> Option<Vec<u8>> {
    let OutputDataEncoding::Encrypted(blob) = data else {
        return None;
    };
    let (scheme, body) = blob.split_first()?;
    (*scheme == EncryptedScheme::Confidential.as_byte()).then(|| body.to_vec())
}
