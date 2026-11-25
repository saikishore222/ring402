use anyhow::{anyhow, Context, Result};
use ring402::{Opener, Quote};
use solana_keypair::{read_keypair_file, Keypair};
use solana_signature::Signature;
use solana_signer::Signer;
use spr_client::{
    prover::tee::{SignedTeePolicy, TeeNetwork},
    IndexerRpcConfig, ProofDataSource, ProverClient, Rpc, SolanaRpc, SprClient, SprIndexer,
};
use spr_interface::pda;
use spr_keypair::ShieldedKeypair;
use spr_program::instruction::{AssetDeposit, Deposit, DepositAsset, Transact};
use spr_transaction::{
    decrypt_spendable, instructions::transact::ConfidentialTransaction, AssetRegistry, Mint,
    ShieldedTransaction, SOL_MINT,
};

const DEPOSIT: u64 = 10_000_000;
const PRICE: u64 = 3_000_000;

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let api_key = std::env::var("HELIUS_API_KEY").context("HELIUS_API_KEY")?;
    let keypair_path =
        std::env::var("PAYER_KEYPAIR").unwrap_or_else(|_| "~/.config/solana/id.json".to_string());
    let payer = read_keypair_file(shellexpand::tilde(&keypair_path).as_ref())
        .map_err(|e| anyhow!("load {keypair_path}: {e}"))?;

    let rpc_url = format!("https://devnet.helius-rpc.com/?api-key={api_key}");
    let spr_url = format!("https://devnet.helius-rpc.com/v1/spr?api-key={api_key}");
    let prover = ProverClient::new(spr_url.clone())
        .with_signed_tee(SignedTeePolicy::for_network(TeeNetwork::Devnet));
    let client = SprClient::new(SolanaRpc::new(rpc_url), SprIndexer::new(&spr_url), prover)
        .with_proof_data_source(ProofDataSource::Prover);

    let tree = pda::tree(0);
    let assets = AssetRegistry::default();
    let agent = ShieldedKeypair::from_keypair(&payer)?;
    let merchant = ShieldedKeypair::from_keypair(&Keypair::new())?;
    let agent_address = agent.shielded_address()?;

    let deposit = Deposit {
        tree,
        depositor: agent.pubkey(),
        deposits: vec![AssetDeposit {
            asset: DepositAsset::Sol,
            view_tag: agent_address.confidential_view_tag()?,
            owner: agent_address.owner_hash()?,
            amount: DEPOSIT,
            memo: None,
        }],
    }
    .instruction()?;
    let signature = client.create_and_send_transaction(
        &[deposit],
        agent.pubkey(),
        &[&agent],
        client.compute_budget(),
    )?;
    println!("deposit   {signature}");
    let funded = decrypt_spendable(&agent, &indexed(&client, signature)?, &assets)
        .map_err(|e| anyhow!("decrypt deposit: {e:?}"))?
        .balances
        .get_balance(SOL_MINT)
        .and_then(|balance| balance.utxos.first().cloned())
        .context("deposit not found")?;

    let mut id = [0u8; 16];
    id.copy_from_slice(&Keypair::new().pubkey().to_bytes()[..16]);
    let quote = Quote {
        id,
        asset: Mint::SOL,
        amount: PRICE,
        pay_to: merchant.shielded_address()?,
    };

    let mut transfer = ConfidentialTransaction::new(vec![funded], agent.pubkey())?;
    let receipt_key = agent
        .viewing_key
        .get_transaction_viewing_key(transfer.first_nullifier())?;
    transfer.add_output_utxo(quote.output()?)?;
    let proof_inputs = transfer.encrypt(&agent)?;
    let data = client.prove_transact(proof_inputs, None, &agent)?;
    let pay = Transact {
        payer: agent.pubkey(),
        input_trees: vec![tree],
        output_tree: tree,
        owner_signers: vec![agent.pubkey()],
        interface_transfer_accounts: Vec::new(),
        data,
    }
    .instruction();
    let signature = client.create_and_send_transaction(
        &[pay],
        agent.pubkey(),
        &[&agent],
        client.compute_budget(),
    )?;
    println!("payment   {signature}");

    let onchain = indexed(&client, signature)?;
    let received = onchain
        .iter()
        .find_map(|tx| {
            quote
                .settle(tx, Opener::Recipient(&merchant.viewing_key))
                .ok()
        })
        .context("merchant could not settle the payment")?;
    let disclosed = onchain
        .iter()
        .find_map(|tx| quote.settle(tx, Opener::Disclosed(&receipt_key)).ok())
        .context("receipt key could not open the payment")?;
    let stranger = ShieldedKeypair::from_keypair(&Keypair::new())?;
    let snooped = onchain.iter().any(|tx| {
        quote
            .settle(tx, Opener::Recipient(&stranger.viewing_key))
            .is_ok()
    });

    println!("from      {}", agent.pubkey());
    println!("to        {}", merchant.pubkey());
    println!(
        "merchant  paid {} lamports for quote {:02x?}",
        received.amount, received.quote_id
    );
    println!(
        "receipt   paid {} lamports (one-payment key)",
        disclosed.amount
    );
    println!("stranger  can read amount: {snooped}");
    anyhow::ensure!(received == disclosed && !snooped);
    Ok(())
}

fn indexed(
    client: &SprClient<SolanaRpc>,
    signature: Signature,
) -> Result<Vec<ShieldedTransaction>> {
    let slot = client
        .get_signature_statuses(vec![signature])?
        .first()
        .and_then(|status| status.as_ref())
        .map(|status| status.slot)
        .context("transaction status missing")?;
    Ok(client
        .get_shielded_transactions_by_signature(signature, Some(IndexerRpcConfig::at_slot(slot)))?
        .transactions
        .into_iter()
        .map(|indexed| indexed.transaction)
        .collect())
}
