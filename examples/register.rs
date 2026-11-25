use anyhow::{anyhow, Context, Result};
use ring402::helius::Helius;
use solana_keypair::read_keypair_file;
use solana_signer::Signer;
use spr_client::Rpc;
use spr_interface::pda;
use spr_program::instruction::CreateSplInterface;
use spr_transaction::Address;

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let api_key = std::env::var("HELIUS_API_KEY").context("HELIUS_API_KEY")?;
    let path = std::env::var("PAYER_KEYPAIR").unwrap_or_else(|_| "~/.config/solana/id.json".into());
    let payer = read_keypair_file(shellexpand::tilde(&path).as_ref())
        .map_err(|e| anyhow!("load {path}: {e}"))?;
    let mint: Address = std::env::args()
        .nth(1)
        .context("usage: register <mint>")?
        .parse()
        .map_err(|_| anyhow!("not a mint address"))?;

    let helius = Helius::devnet(&api_key);
    let client = helius.client();
    if client.get_account(pda::spl_interface(&mint))?.is_none() {
        let token_program = client.get_account(mint)?.context("mint not found")?.owner;
        let create = CreateSplInterface {
            authority: payer.pubkey(),
            mint,
            token_program,
        }
        .instruction();
        let signature = client.create_and_send_transaction(
            &[create],
            payer.pubkey(),
            &[&payer],
            client.compute_budget(),
        )?;
        println!("registered {signature}");
    }
    println!("asset id   {}", helius.mint(mint)?.asset_id);
    Ok(())
}
