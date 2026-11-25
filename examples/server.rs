use anyhow::{anyhow, Context, Result};
use axum::{routing::get, Json, Router};
use ring402::{helius::Helius, server::Paywall, wire::SOLANA_DEVNET};
use serde_json::{json, Value};
use solana_keypair::Keypair;
use spr_keypair::ShieldedKeypair;
use spr_transaction::{Address, Mint};
use tokio::{net::TcpListener, runtime::Runtime};

const ADDR: &str = "127.0.0.1:4020";
const SOL_PRICE: u64 = 3_000_000;
const TOKEN_PRICE: u64 = 10_000;

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let api_key = std::env::var("HELIUS_API_KEY").context("HELIUS_API_KEY")?;
    let helius = Helius::devnet(&api_key);
    let (asset, price) = match std::env::var("MINT") {
        Ok(mint) => {
            let mint: Address = mint
                .parse()
                .map_err(|_| anyhow!("MINT is not an address"))?;
            (helius.mint(mint)?, TOKEN_PRICE)
        }
        Err(_) => (Mint::SOL, SOL_PRICE),
    };

    let merchant = ShieldedKeypair::from_keypair(&Keypair::new())?;
    let pay_to = merchant.shielded_address()?;
    let paywall = Paywall::new(helius, SOLANA_DEVNET, pay_to, merchant.viewing_key.clone());
    drop(merchant);

    let app = Router::new()
        .route(
            "/",
            get(|| async { "GET /forecast is paid, confidentially\n" }),
        )
        .route(
            "/forecast",
            get(forecast).layer(paywall.price(asset, price)),
        );

    println!("listening on http://{ADDR}");
    println!("merchant {}", pay_to.solana_address()?);
    println!("price    {price} of {}", asset.asset);
    Runtime::new()?.block_on(async {
        axum::serve(TcpListener::bind(ADDR).await?, app).await?;
        Ok(())
    })
}

async fn forecast() -> Json<Value> {
    Json(json!({ "city": "Bengaluru", "tomorrow": "light rain", "high_c": 27 }))
}
