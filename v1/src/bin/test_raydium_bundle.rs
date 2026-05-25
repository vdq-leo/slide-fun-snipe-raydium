// ==================== RAYDIUM BUNDLE BUY TEST ====================
// Test full Raydium swap + multi-wallet bundle buy using a real pool.
//
// Usage:
//   cargo run --bin test_raydium_bundle -- <POOL_INIT_TX_SIGNATURE>
//   cargo run --bin test_raydium_bundle -- <POOL_INIT_TX_SIGNATURE> live
//
// Steps:
//   1. Fetch pool info from the given TX (AMM ID, vaults, serum market...)
//   2. Build buy TX for main wallet (same as normal snipe)
//   3. Build buy TXs for all sub-wallets from wallets.json
//   4. Pack into Jito bundles and fire
//
// Find a real pool TX: https://solscan.io/txs?filter=Successful&programId=675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8
// Look for "initialize2" instructions.

use dotenvy;
use slidefun_raydium_snipe::{bundle_buy, config::Config, constants, pool::get_pool_info_from_amm_account};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSimulateTransactionConfig;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    native_token::LAMPORTS_PER_SOL,
    pubkey::Pubkey,
    signer::{keypair::Keypair, Signer},
};
use spl_associated_token_account::{
    get_associated_token_address, get_associated_token_address_with_program_id,
};
use std::str::FromStr;
use std::sync::Arc;

/// Typical rent-exempt minimum for a legacy SPL token account (165 bytes).
const TOKEN_ACCOUNT_RENT_LAMPORTS: u64 = 2_039_280;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 1 {
        eprintln!("\n  Usage:");
        eprintln!("    cargo run --bin test_raydium_bundle");
        eprintln!("    cargo run --bin test_raydium_bundle -- <POOL_AMM_ID>");
        eprintln!("    cargo run --bin test_raydium_bundle -- <POOL_AMM_ID> live\n");
        std::process::exit(1);
    }

    let force_live = args.iter().any(|a| a.eq_ignore_ascii_case("live"));
    let force_dry = args.iter().any(|a| a.eq_ignore_ascii_case("dry"));

    let default_amm = "67ZsVEcuHt9VkWWWGYLhMdx1VMs1H9xHnhnuK8UqdNZF";
    let amm_id_str = args
        .iter()
        .skip(1)
        .find(|a| *a != "live" && *a != "dry")
        .map(|s| s.as_str())
        .unwrap_or(default_amm);

    let amm_pubkey = Pubkey::from_str(amm_id_str).unwrap_or_else(|_| {
        eprintln!("❌ Invalid AMM ID: {}", amm_id_str);
        std::process::exit(1);
    });

    // Explicit CLI mode override
    if force_live {
        unsafe {
            std::env::set_var("DRY_RUN", "false");
        }
    } else if force_dry {
        unsafe {
            std::env::set_var("DRY_RUN", "true");
        }
    }

    let config = Config::from_env();
    let rpc_url = format!(
        "https://mainnet.helius-rpc.com/?api-key={}",
        config.helius_api_key
    );
    let rpc = Arc::new(RpcClient::new(rpc_url));

    println!("\n╔════════════════════════════════════════════════════════╗");
    println!("║       RAYDIUM BUNDLE BUY TEST                        ║");
    println!("╚════════════════════════════════════════════════════════╝");
    println!(
        "  Mode       : {}",
        if config.dry_run {
            "DRY RUN (no TX sent)"
        } else {
            "⚡ LIVE (real SOL!)"
        }
    );
    println!("  AMM ID     : {}", amm_id_str);
    println!("  Main wallet: {}", config.keypair.pubkey());
    println!("  SOL/main   : {} SOL", config.sol_amount);
    println!("  SOL/wallet : {} SOL", config.app.slidefun_pump_amount);
    println!("  Jito tip   : {} SOL", config.jito_tip);
    println!();

    // Step 1: Fetch pool info from TX
    println!("  [1/4] Fetching pool info from AMM account...");
    let pool_info = match get_pool_info_from_amm_account(&rpc, &amm_pubkey, config.raydium_program()).await {
        Some(p) => {
            println!("        AMM ID   : {}", p.amm_id);
            println!("        Token    : {}", p.base_mint);
            if p.pool_sol_amount > 0 {
                println!(
                    "        Pool size: {:.4} SOL",
                    p.pool_sol_amount as f64 / 1e9
                );
            }
            p
        }
        None => {
            eprintln!("\n  ❌ Could not reconstruct PoolInfo from AMM account: {}", amm_id_str);
            std::process::exit(1);
        }
    };

    // Step 2: Load sub-wallets
    println!("\n  [2/4] Loading sub-wallets from config...");
    let mut wallets: Vec<(Keypair, f64)> = config.enabled_bundle_keypairs();
    if wallets.is_empty() {
        println!("        ⚠️  No sub-wallets loaded — using main wallet as a dummy sub-wallet for testing.");
        wallets.push((
            Keypair::try_from(&config.keypair.to_bytes()[..]).unwrap(),
            0.001, // Use 0.001 SOL so the main wallet has enough balance to test
        ));
    } else {
        println!("        {} sub-wallets loaded", wallets.len());
        for (i, (kp, sol)) in wallets.iter().enumerate() {
            println!("        [wallet {:>2}] {} ({} SOL)", i, kp.pubkey(), sol);
        }
    }

    // Step 3: Main wallet buy (same as real snipe)
    println!("\n  [3/4] Main wallet buy...");
    println!("        Skipping main wallet `handle_buy` spam in this test to save");
    println!("        Jito rate limit (1 req/sec) for the actual bundle test.");

    // Step 4: Bundle buy for sub-wallets
    if !wallets.is_empty() {
        println!("\n  [4/4] Bundle buy for {} sub-wallets...", wallets.len());

        let bh = {
            let h = slidefun_raydium_snipe::blockhash::get_blockhash();
            if h == solana_sdk::hash::Hash::default() {
                rpc.get_latest_blockhash().await.unwrap_or_default()
            } else {
                h
            }
        };

        let sol_lamports = (wallets[0].1 * LAMPORTS_PER_SOL as f64) as u64;

        let w0 = wallets[0].0.pubkey();
        let wsol_mint = Pubkey::from_str(constants::WSOL_MINT).unwrap();
        let wsol_ata = get_associated_token_address(&w0, &wsol_mint);
        let token_ata = get_associated_token_address_with_program_id(
            &w0,
            &pool_info.base_mint,
            &pool_info.base_token_program,
        );
        let need_wsol_ata = rpc.get_account(&wsol_ata).await.is_err();
        let need_token_ata = rpc.get_account(&token_ata).await.is_err();
        let mut min_lamports = sol_lamports.saturating_add(100_000);
        if need_wsol_ata {
            min_lamports = min_lamports.saturating_add(TOKEN_ACCOUNT_RENT_LAMPORTS);
        }
        if need_token_ata {
            min_lamports = min_lamports.saturating_add(TOKEN_ACCOUNT_RENT_LAMPORTS);
        }
        let bal0 = rpc.get_balance(&w0).await.unwrap_or(0);
        if bal0 < min_lamports {
            let mut parts: Vec<&str> = Vec::new();
            if need_wsol_ata {
                parts.push("new WSOL ATA rent");
            }
            if need_token_ata {
                parts.push("new token ATA rent");
            }
            parts.push("swap lamports + fee buffer");
            let parts = parts.join(", ");
            println!(
                "        ❌ wallet[0] balance too low: {} lamports (≈ {:.6} SOL)",
                bal0,
                bal0 as f64 / LAMPORTS_PER_SOL as f64
            );
            println!(
                "           Need ≥ {} lamports (≈ {:.6} SOL) for: {}",
                min_lamports,
                min_lamports as f64 / LAMPORTS_PER_SOL as f64,
                parts
            );
            println!("           Fund sub-wallets on mainnet, then retry.\n");
            println!("\n  ⛔ Aborting — insufficient lamports.\n");
            return;
        }

        if config.dry_run {
            println!("\n  [SIM] Simulating wallet[0] buy TX via RPC...");
            match bundle_buy::build_raydium_buy_tx_for_wallet(
                &wallets[0].0,
                &pool_info,
                sol_lamports,
                0,
                config.cu_limit,
                config.priority_fee,
                bh,
            ) {
                None => println!("        ❌ Failed to build TX"),
                Some(tx) => {
                    let sim_cfg = RpcSimulateTransactionConfig {
                        sig_verify: false,
                        replace_recent_blockhash: true,
                        commitment: Some(CommitmentConfig::confirmed()),
                        ..Default::default()
                    };
                    match rpc.simulate_transaction_with_config(&tx, sim_cfg).await {
                        Err(e) => println!("        ❌ RPC simulate error: {}", e),
                        Ok(result) => {
                            if let Some(err) = &result.value.err {
                                println!("        ❌ Simulation FAILED: {:?}", err);
                                if let Some(logs) = &result.value.logs {
                                    println!("        Logs:");
                                    for log in logs.iter().take(20) {
                                        println!("          {}", log);
                                    }
                                }
                                println!("\n  ⛔ Aborting — fix simulation error before sending live bundle.");
                                println!(
                                    "     Hint: InstructionError(_, Custom(1)) on Token program often means \
                                     InsufficientFunds (sub-wallet needs more SOL for ATA rent + swap).\n"
                                );
                                return;
                            } else {
                                let units = result.value.units_consumed.unwrap_or(0);
                                println!("        ✅ Simulation PASSED (units: {})", units);
                            }
                        }
                    }
                }
            }
        } else {
            println!("\n  [LIVE] Skipping RPC simulation — firing real Jito bundles.");
        }

        if config.dry_run {
            println!("        DRY RUN — not sending");
            println!(
                "        Would fire {} bundles × 4 Jito endpoints",
                (wallets.len() + 3) / 4
            );
        } else {
            let pool_arc = Arc::new(pool_info);
            let dummy_trades = slidefun_raydium_snipe::trades::TradesStore::new();
            bundle_buy::raydium_bundle_buy(&config, &wallets, pool_arc, bh, dummy_trades).await;
        }
    }

    println!("\n  ✅ Test complete.\n");
}
