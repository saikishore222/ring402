use std::{thread, time::Duration};

use solana_signature::Signature;
use spr_client::{
    fetch_asset_id, fetch_spl_asset_registrations,
    prover::tee::{SignedTeePolicy, TeeNetwork},
    ClientError, IndexerRpcConfig, ProofDataSource, ProverClient, Rpc, SolanaRpc, SprClient,
    SprIndexer,
};
use spr_transaction::{Address, Mint, ShieldedTransaction};

use crate::wire;

const CONFIRMATION_POLLS: u32 = 10;
const CONFIRMATION_INTERVAL: Duration = Duration::from_millis(400);

pub struct Helius {
    client: SprClient<SolanaRpc>,
    network: &'static str,
}

impl Helius {
    pub fn devnet(api_key: &str) -> Self {
        Self::connect("devnet", TeeNetwork::Devnet, wire::SOLANA_DEVNET, api_key)
    }

    pub fn mainnet(api_key: &str) -> Self {
        Self::connect(
            "mainnet",
            TeeNetwork::Mainnet,
            wire::SOLANA_MAINNET,
            api_key,
        )
    }

    fn connect(cluster: &str, tee: TeeNetwork, network: &'static str, api_key: &str) -> Self {
        let rpc = format!("https://{cluster}.helius-rpc.com/?api-key={api_key}");
        let spr = format!("https://{cluster}.helius-rpc.com/v1/spr?api-key={api_key}");
        let prover =
            ProverClient::new(spr.clone()).with_signed_tee(SignedTeePolicy::for_network(tee));
        let client = SprClient::new(SolanaRpc::new(rpc), SprIndexer::new(&spr), prover)
            .with_proof_data_source(ProofDataSource::Prover);
        Self { client, network }
    }

    pub fn network(&self) -> &'static str {
        self.network
    }

    pub fn client(&self) -> &SprClient<SolanaRpc> {
        &self.client
    }

    pub fn mint(&self, asset: Address) -> Result<Mint, ClientError> {
        Ok(Mint::new(asset, fetch_asset_id(&self.client, asset)?))
    }

    pub fn registered(&self) -> Result<Vec<Mint>, ClientError> {
        fetch_spl_asset_registrations(&self.client, None)
    }

    pub fn transactions(
        &self,
        signature: Signature,
    ) -> Result<Vec<ShieldedTransaction>, ClientError> {
        let Some(slot) = self.landed_slot(signature)? else {
            return Ok(Vec::new());
        };
        Ok(self
            .client
            .get_shielded_transactions_by_signature(
                signature,
                Some(IndexerRpcConfig::at_slot(slot)),
            )?
            .transactions
            .into_iter()
            .map(|indexed| indexed.transaction)
            .collect())
    }

    fn landed_slot(&self, signature: Signature) -> Result<Option<u64>, ClientError> {
        for attempt in 0..CONFIRMATION_POLLS {
            if attempt > 0 {
                thread::sleep(CONFIRMATION_INTERVAL);
            }
            let statuses = self.client.get_signature_statuses(vec![signature])?;
            if let Some(status) = statuses.into_iter().next().flatten() {
                return Ok(Some(status.slot));
            }
        }
        Ok(None)
    }
}
