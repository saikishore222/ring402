use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
};

use reqwest::StatusCode;
use solana_keypair::Keypair;
use solana_signature::Signature;
use solana_signer::Signer;
use spr_client::{ClientError, IndexerRpcConfig, Rpc, UtxoScan};
use spr_interface::pda;
use spr_keypair::{KeypairError, ShieldedAddress, ShieldedKeypair, ViewingKey};
use spr_program::instruction::{AssetDeposit, Deposit, DepositAsset, DepositSplAccounts, Transact};
use spr_transaction::{
    instructions::transact::ConfidentialTransaction, select_spend, Address, Mint,
    SpendableDecryptionResult, TransactionError, SOL_MINT,
};

use crate::{
    helius::Helius,
    registry,
    wire::{
        self, PaymentPayload, PaymentRequired, Proof, Requirements, Settlement, WireError,
        PAYMENT_REQUIRED, PAYMENT_RESPONSE, PAYMENT_SESSION, PAYMENT_SIGNATURE, SCHEME,
        X402_VERSION,
    },
    Quote,
};

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("no confidential offer on {0} in a token this agent may spend")]
    NoOffer(&'static str),
    #[error("price {price} is over the limit of {limit}")]
    OverLimit { price: u64, limit: u64 },
    #[error("mint {0} does not exist")]
    UnknownMint(Address),
    #[error("deposit: {0}")]
    Deposit(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error(transparent)]
    Quote(#[from] crate::Error),
    #[error(transparent)]
    Chain(#[from] ClientError),
    #[error(transparent)]
    Transaction(#[from] TransactionError),
    #[error(transparent)]
    Keypair(#[from] KeypairError),
    #[error(transparent)]
    Task(#[from] tokio::task::JoinError),
}

pub struct Agent {
    http: reqwest::Client,
    helius: Arc<Helius>,
    keys: Arc<ShieldedKeypair>,
    budgets: HashMap<Address, Budget>,
    sessions: Mutex<HashMap<String, String>>,
}

#[derive(Clone, Copy, Default)]
struct Budget {
    limit: u64,
    prepay: u64,
}

pub struct Paid {
    pub response: reqwest::Response,
    pub receipt: Option<Receipt>,
    pub credit: Option<u64>,
}

pub struct Receipt {
    pub signature: Signature,
    pub quote: Quote,
    pub key: ViewingKey,
}

impl Agent {
    pub fn new(helius: Helius, keypair: &Keypair) -> Result<Self, AgentError> {
        Ok(Self {
            http: reqwest::Client::new(),
            helius: Arc::new(helius),
            keys: Arc::new(ShieldedKeypair::from_keypair(keypair)?),
            budgets: HashMap::new(),
            sessions: Mutex::new(HashMap::new()),
        })
    }

    pub fn spend_limit(mut self, asset: Address, amount: u64) -> Self {
        self.budgets.entry(asset).or_default().limit = amount;
        self
    }

    pub fn prepay(mut self, asset: Address, amount: u64) -> Self {
        self.budgets.entry(asset).or_default().prepay = amount;
        self
    }

    pub fn address(&self) -> Result<ShieldedAddress, AgentError> {
        Ok(self.keys.shielded_address()?)
    }

    pub async fn balance(&self, asset: Address) -> Result<u64, AgentError> {
        self.blocking(move |helius, keys| {
            Ok(spendable(helius, keys, helius.mint(asset)?)?
                .balances
                .get_balance(asset)
                .map_or(0, |balance| balance.amount))
        })
        .await
    }

    pub async fn deposit(&self, asset: Address, amount: u64) -> Result<Signature, AgentError> {
        self.blocking(move |helius, keys| {
            let client = helius.client();
            let address = keys.shielded_address()?;
            let asset = if asset == SOL_MINT {
                DepositAsset::Sol
            } else {
                let token_program = client
                    .get_account(asset)?
                    .ok_or(AgentError::UnknownMint(asset))?
                    .owner;
                DepositAsset::Spl(DepositSplAccounts {
                    mint: asset,
                    user_token: pda::associated_token_address_with_program(
                        &keys.pubkey(),
                        &asset,
                        &token_program,
                    ),
                    token_program,
                })
            };
            let deposit = Deposit {
                tree: pda::tree(0),
                depositor: keys.pubkey(),
                deposits: vec![AssetDeposit {
                    asset,
                    view_tag: address.confidential_view_tag()?,
                    owner: address.owner_hash()?,
                    amount,
                    memo: None,
                }],
            }
            .instruction()
            .map_err(|error| AgentError::Deposit(format!("{error:?}")))?;
            Ok(client.create_and_send_transaction(
                &[deposit],
                keys.pubkey(),
                &[keys],
                client.compute_budget(),
            )?)
        })
        .await
    }

    pub async fn get(&self, url: &str) -> Result<Paid, AgentError> {
        let request = self.http.get(url).build()?;
        let origin = request.url().origin().ascii_serialization();
        let session = self.session(&origin);

        let response = self.send(url, session.as_deref(), None).await?;
        if response.status() != StatusCode::PAYMENT_REQUIRED {
            return Ok(self.paid(origin, response, None));
        }

        let network = self.helius.network();
        let required: PaymentRequired = response
            .headers()
            .get(PAYMENT_REQUIRED)
            .and_then(|value| value.to_str().ok())
            .ok_or(AgentError::NoOffer(network))
            .and_then(|header| Ok(wire::decode(header)?))?;
        let (accepted, budget) = required
            .accepts
            .into_iter()
            .filter(|offer| offer.scheme == SCHEME && offer.network == network)
            .find_map(|offer| {
                let budget = *self.budgets.get(&offer.asset().ok()?)?;
                Some((offer, budget))
            })
            .ok_or(AgentError::NoOffer(network))?;
        let price = accepted.price()?;
        if price > budget.limit {
            return Err(AgentError::OverLimit {
                price,
                limit: budget.limit,
            });
        }
        let amount = if accepted.extra.sessions {
            price.max(budget.prepay)
        } else {
            price
        };

        let terms = accepted.clone();
        let receipt = self
            .blocking(move |helius, keys| pay(helius, keys, &terms, amount))
            .await?;
        let payload = PaymentPayload {
            x402_version: X402_VERSION,
            resource: Some(required.resource),
            accepted,
            payload: Proof {
                signature: receipt.signature.to_string(),
            },
        };
        let response = self
            .send(url, session.as_deref(), Some(wire::encode(&payload)))
            .await?;
        Ok(self.paid(origin, response, Some(receipt)))
    }

    async fn send(
        &self,
        url: &str,
        session: Option<&str>,
        payment: Option<String>,
    ) -> Result<reqwest::Response, AgentError> {
        let mut request = self.http.get(url);
        if let Some(token) = session {
            request = request.header(PAYMENT_SESSION, token);
        }
        if let Some(payment) = payment {
            request = request.header(PAYMENT_SIGNATURE, payment);
        }
        Ok(request.send().await?)
    }

    fn session(&self, origin: &str) -> Option<String> {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(origin)
            .cloned()
    }

    fn paid(&self, origin: String, response: reqwest::Response, receipt: Option<Receipt>) -> Paid {
        let session = response
            .headers()
            .get(PAYMENT_RESPONSE)
            .and_then(|value| value.to_str().ok())
            .and_then(|header| wire::decode::<Settlement>(header).ok())
            .and_then(|settlement| settlement.extensions?.session);
        let credit = session
            .as_ref()
            .and_then(|session| session.balance.parse().ok());
        if let Some(session) = session {
            self.sessions
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(origin, session.token);
        }
        Paid {
            response,
            receipt,
            credit,
        }
    }

    async fn blocking<T, F>(&self, work: F) -> Result<T, AgentError>
    where
        T: Send + 'static,
        F: FnOnce(&Helius, &ShieldedKeypair) -> Result<T, AgentError> + Send + 'static,
    {
        let helius = self.helius.clone();
        let keys = self.keys.clone();
        tokio::task::spawn_blocking(move || work(&helius, &keys)).await?
    }
}

fn spendable(
    helius: &Helius,
    keys: &ShieldedKeypair,
    asset: Mint,
) -> Result<SpendableDecryptionResult, AgentError> {
    let client = helius.client();
    let slot = client.get_slot()?;
    Ok(UtxoScan::new(keys, &registry(asset)?)
        .with_indexer_config(IndexerRpcConfig::at_slot(slot))
        .fetch(client)?
        .spendable)
}

fn pay(
    helius: &Helius,
    keys: &ShieldedKeypair,
    terms: &Requirements,
    amount: u64,
) -> Result<Receipt, AgentError> {
    let client = helius.client();
    let asset = helius.mint(terms.asset()?)?;
    let quote = Quote {
        amount,
        ..terms.quote(asset)?
    };
    let funds = spendable(helius, keys, asset)?;
    let inputs = select_spend(funds.utxos(), asset.asset, amount)?;

    let mut transfer = ConfidentialTransaction::new(inputs, keys.pubkey())?;
    let key = keys
        .viewing_key
        .get_transaction_viewing_key(transfer.first_nullifier())?;
    transfer.add_output_utxo(quote.output()?)?;
    let input_trees = transfer
        .input_tree_ids()
        .iter()
        .map(|id| pda::tree(*id))
        .collect();
    let proof_inputs = transfer.encrypt(keys)?;
    let output_tree = pda::tree(proof_inputs.output_tree_id);
    let data = client.prove_transact(proof_inputs, None, keys)?;

    let instruction = Transact {
        payer: keys.pubkey(),
        input_trees,
        output_tree,
        owner_signers: vec![keys.pubkey()],
        interface_transfer_accounts: Vec::new(),
        data,
    }
    .instruction();
    let signature = client.create_and_send_transaction(
        &[instruction],
        keys.pubkey(),
        &[keys],
        client.compute_budget(),
    )?;
    Ok(Receipt {
        signature,
        quote,
        key,
    })
}
