use dotenvy;
use slidefun_raydium_snipe::{bundle_buy, config::Config, pool::get_pool_info_from_amm_account};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::native_token::LAMPORTS_PER_SOL;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::Signer;
use std::str::FromStr;
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────
// DEVNET CONFIGURATION
//
// This test ALWAYS runs on Solana Devnet regardless of config.json.
//
// Default pool: A known standard Raydium AMM V4 pool on Devnet
//   Program  : HWy1jotHpo6UqeQxx49dpYYdQB8wj9Qk9MdxwjLvDHB8
//   Pool AMM : GYERH2LCgTmfv3RSJ8emP5E5SWQoiUL5LbrDwuHKhhWZ  (WSOL / token pair)
//
// To use a different pool:
//   cargo run --bin test_devnet_swap -- <POOL_AMM_ID>
// ─────────────────────────────────────────────────────────────────

// Standard Raydium AMM V4 program on Devnet (752-byte account layout)
const DEVNET_RAYDIUM_PROGRAM: &str = "HWy1jotHpo6UqeQxx49dpYYdQB8wj9Qk9MdxwjLvDHB8";

// Default Devnet test pool (standard AMM V4, 752 bytes)
const DEFAULT_DEVNET_POOL: &str = "GYERH2LCgTmfv3RSJ8emP5E5SWQoiUL5LbrDwuHKhhWZ";

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    // 1. Load configuration
    let config = Config::from_env();
    let main_wallet = config.keypair.pubkey();

    // 2. Always connect to Devnet (override config.json network)
    let rpc_url = if !config.helius_api_key.is_empty() {
        format!("https://devnet.helius-rpc.com/?api-key={}", config.helius_api_key)
    } else {
        "https://api.devnet.solana.com".to_string()
    };
    let rpc = Arc::new(RpcClient::new(rpc_url.clone()));

    // 3. Parse CLI arguments
    let args: Vec<String> = std::env::args().collect();
    let pool_id_str = if args.len() >= 2 {
        args[1].as_str()
    } else {
        DEFAULT_DEVNET_POOL
    };
    let pool_id = match Pubkey::from_str(pool_id_str) {
        Ok(pk) => pk,
        Err(_) => {
            println!("❌ Invalid pool address: {}", pool_id_str);
            return;
        }
    };

    // Always use the correct Devnet Raydium AMM V4 program
    let amm_program = Pubkey::from_str(DEVNET_RAYDIUM_PROGRAM).unwrap();

    println!("============================================================");
    println!("   🧪 LIVE DEVNET RAYDIUM SWAP TESTER (Direct RPC Mode)");
    println!("============================================================");
    println!("   Network        : SOLANA DEVNET (forced)");
    println!("   RPC URL        : {}", rpc_url);
    println!("   Main Wallet    : {}", main_wallet);
    println!("   AMM Pool       : {}", pool_id_str);
    println!("   AMM Program    : {}", amm_program);
    println!();
    println!("   Note: Jito does NOT run on Devnet.");
    println!("         Txs are sent directly via standard RPC.");

    // 4. Check wallet balance on Devnet
    let wallet_balance = match rpc.get_balance(&main_wallet).await {
        Ok(b) => b,
        Err(e) => {
            println!("\n❌ Cannot connect to Devnet: {}", e);
            return;
        }
    };
    println!("\n   Wallet Balance : {:.6} SOL (Devnet)", wallet_balance as f64 / 1e9);

    if wallet_balance < 50_000_000 { // < 0.05 SOL
        println!("\n❌ Insufficient Devnet balance. Airdrop first:");
        println!("   solana airdrop 2 {} --url devnet", main_wallet);
        println!("   Or visit: https://faucet.solana.com/");
        return;
    }

    // 5. Reconstruct PoolInfo directly from AMM account (1 RPC call, any pool age)
    println!("\n🔍 Fetching PoolInfo from AMM account data...");
    let pool_info = match get_pool_info_from_amm_account(&rpc, &pool_id, amm_program).await {
        Some(info) => {
            println!("\n✅ PoolInfo reconstructed!");
            println!("   AMM ID        : {}", info.amm_id);
            println!("   Token Mint    : {}", info.base_mint);
            println!("   Quote (WSOL)  : {}", info.quote_mint);
            println!("   Serum Market  : {}", info.market_id);
            println!("   Token Program : {}", info.base_token_program);
            info
        }
        None => {
            println!("\n❌ Failed to reconstruct PoolInfo.");
            println!("   Possible reasons:");
            println!("   1. Pool is not a standard Raydium AMM V4 (752-byte state)");
            println!("      — The pool at {} uses a different format", pool_id_str);
            println!("   2. Wrong program ID: {}", amm_program);
            println!("   3. Pool does not exist on Devnet");
            println!();
            println!("   Try with the default pool:");
            println!("   cargo run --bin test_devnet_swap");
            println!("   Or pass your own: cargo run --bin test_devnet_swap -- <AMM_ID>");
            return;
        }
    };

    // 6. Build the Buy Transaction (0.01 Devnet SOL for testing)
    let buy_amount_sol = 0.01;
    let sol_lamports = (buy_amount_sol * LAMPORTS_PER_SOL as f64) as u64;
    println!("\n🔨 Building swap TX (Buy {:.4} Devnet SOL worth of tokens)...", buy_amount_sol);

    let bh = match rpc.get_latest_blockhash().await {
        Ok(h) => h,
        Err(e) => {
            println!("❌ Failed to get blockhash: {}", e);
            return;
        }
    };

    let tx = match bundle_buy::build_raydium_buy_tx_for_wallet(
        &config.keypair,
        &pool_info,
        sol_lamports,
        0,               // min_token_out = 0 (accept any price, max speed)
        config.cu_limit,
        config.priority_fee,
        bh,
    ) {
        Some(t) => t,
        None => {
            println!("❌ Failed to build transaction (TX too large or account error).");
            return;
        }
    };

    // 7. Send directly via RPC (no Jito on Devnet)
    println!("\n🚀 Sending transaction to Devnet...");
    match rpc.send_transaction(&tx).await {
        Ok(sig) => {
            println!("✅ TX sent! Signature: {}", sig);
            println!("   🔗 https://solscan.io/tx/{}?cluster=devnet", sig);
            println!("   Waiting 5s for confirmation...");
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

            match rpc
                .get_signature_status_with_commitment(&sig, CommitmentConfig::confirmed())
                .await
            {
                Ok(Some(Ok(()))) => {
                    println!("\n🎉 SWAP CONFIRMED ON DEVNET!");
                    println!(
                        "   ✅ https://solscan.io/tx/{}?cluster=devnet",
                        sig
                    );
                    println!("\n   Swap logic is correct — ready to test on Mainnet.");
                }
                Ok(Some(Err(e))) => {
                    println!("\n❌ TX landed but failed on-chain: {:?}", e);
                    println!("   Common causes:");
                    println!("   1. Pool has insufficient liquidity");
                    println!("   2. WSOL ATA needs funding (0.002 SOL for rent)");
                    println!("   Check: https://solscan.io/tx/{}?cluster=devnet", sig);
                }
                _ => {
                    println!("⏳ TX still processing. Check:");
                    println!("   https://solscan.io/tx/{}?cluster=devnet", sig);
                }
            }
        }
        Err(e) => {
            println!("❌ TX broadcast failed: {}", e);
            println!("   Common causes:");
            println!("   1. Pool accounts are wrong (wrong program / pool mismatch)");
            println!("   2. No liquidity in pool");
            println!("   3. Simulation error (try with a different pool)");
        }
    }
}
