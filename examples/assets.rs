use anyhow::{Context, Result};
use ring402::helius::Helius;

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let api_key = std::env::var("HELIUS_API_KEY").context("HELIUS_API_KEY")?;
    let helius = match std::env::args().nth(1).as_deref() {
        Some("mainnet") => Helius::mainnet(&api_key),
        _ => Helius::devnet(&api_key),
    };

    let mut registered = helius.registered()?;
    registered.sort_by_key(|mint| mint.asset_id);
    println!(
        "{} SPL tokens registered on {}",
        registered.len(),
        helius.network()
    );
    for mint in registered {
        println!("{:>6}  {}", mint.asset_id, mint.asset);
    }
    Ok(())
}
