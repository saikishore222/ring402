use anyhow::{anyhow, Context, Result};
use ring402::{client::Agent, helius::Helius};
use solana_keypair::read_keypair_file;
use spr_transaction::{Address, SOL_MINT};

const CALLS: usize = 3;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let api_key = std::env::var("HELIUS_API_KEY").context("HELIUS_API_KEY")?;
    let path = std::env::var("PAYER_KEYPAIR").unwrap_or_else(|_| "~/.config/solana/id.json".into());
    let payer = read_keypair_file(shellexpand::tilde(&path).as_ref())
        .map_err(|e| anyhow!("load {path}: {e}"))?;
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:4020/forecast".into());
    let (asset, limit, prepay) = match std::env::var("MINT") {
        Ok(mint) => {
            let mint: Address = mint
                .parse()
                .map_err(|_| anyhow!("MINT is not an address"))?;
            (mint, 50_000, 100_000)
        }
        Err(_) => (SOL_MINT, 5_000_000, 10_000_000),
    };

    let agent = Agent::new(Helius::devnet(&api_key), &payer)?
        .spend_limit(asset, limit)
        .prepay(asset, prepay);
    let balance = agent.balance(asset).await?;
    println!("private balance  {balance}");
    if balance < prepay {
        println!("deposit          {}", agent.deposit(asset, prepay).await?);
    }

    for call in 1..=CALLS {
        let paid = agent.get(&url).await?;
        println!("call {call}           {}", paid.response.status());
        match &paid.receipt {
            Some(receipt) => println!(
                "  onchain        {} in {}",
                receipt.quote.amount, receipt.signature
            ),
            None => println!("  onchain        nothing"),
        }
        if let Some(credit) = paid.credit {
            println!("  credit left    {credit}");
        }
        println!("  {}", paid.response.text().await?);
    }
    Ok(())
}
