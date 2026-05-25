use dotenvy;
use slidefun_raydium_snipe::{
    bundle_buy,
    config::Config,
    constants::{jito_tip_address, jito_bundle_urls},
    pool::get_pool_info_from_amm_account,
};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::message::{v0::Message, VersionedMessage};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::Signer;
#[allow(deprecated)]
use solana_sdk::system_instruction;
use solana_sdk::transaction::VersionedTransaction;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use base64::Engine;

// ─────────────────────────────────────────────────────────────────
// MAINNET JITO SWAP TESTER
//
// Sends a tiny real-money swap on Mainnet via a Jito bundle to verify
// that the Jito tip + swap bundle path works end-to-end.
//
// Usage:
//   cargo run --bin test_mainnet_jito_swap
//   cargo run --bin test_mainnet_jito_swap -- <POOL_AMM_ID> <SOL_AMOUNT>
//
// Defaults:
//   Pool    : 67zsvecuht9vkwwwgylhmdx1vms1h9xhnhnuk8uqdnzf (from config)
//   Amount  : 0.001 SOL (very small test buy)
//   Jito tip: from config.json → jito_tip (0.02 SOL by default — reduce if desired)
// ─────────────────────────────────────────────────────────────────

const MAINNET_RAYDIUM_PROGRAM: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
const DEFAULT_POOL:             &str = "67ZsVEcuHt9VkWWWGYLhMdx1VMs1H9xHnhnuK8UqdNZF";
const DEFAULT_SOL_AMOUNT:       f64  = 0.001; // 0.001 SOL test buy
const POLL_TIMEOUT_SECS:        u64  = 45;

fn encode_tx(tx: &VersionedTransaction) -> String {
    let bytes = bincode::serialize(tx).unwrap();
    base64::engine::general_purpose::STANDARD.encode(&bytes)
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    // ── Config ───────────────────────────────────────────────────────────────
    let config = Config::from_env();
    let main_wallet = config.keypair.pubkey();

    // ── Mainnet RPC ───────────────────────────────────────────────────────────
    let rpc_url = if !config.helius_api_key.is_empty() {
        format!("https://mainnet.helius-rpc.com/?api-key={}", config.helius_api_key)
    } else {
        "https://api.mainnet-beta.solana.com".to_string()
    };
    let rpc = Arc::new(RpcClient::new(rpc_url.clone()));

    // ── CLI args ─────────────────────────────────────────────────────────────
    let args: Vec<String> = std::env::args().collect();
    let pool_id_str  = if args.len() >= 2 { &args[1] } else { DEFAULT_POOL };
    let sol_amount   = if args.len() >= 3 {
        args[2].parse::<f64>().unwrap_or(DEFAULT_SOL_AMOUNT)
    } else {
        DEFAULT_SOL_AMOUNT
    };
    let lamports     = (sol_amount * 1_000_000_000.0) as u64;
    let jito_tip_lam = (config.jito_tip * 1_000_000_000.0) as u64;

    let pool_id     = match Pubkey::from_str(pool_id_str) {
        Ok(pk) => pk,
        Err(_) => { println!("❌ Invalid pool address: {}", pool_id_str); return; }
    };
    let amm_program = Pubkey::from_str(MAINNET_RAYDIUM_PROGRAM).unwrap();
    let tip_addr    = Pubkey::from_str(jito_tip_address()).unwrap();

    println!("============================================================");
    println!("   🚀 MAINNET JITO BUNDLE SWAP TESTER (Real SOL)");
    println!("============================================================");
    println!("   Network        : SOLANA MAINNET ⚠️  REAL MONEY");
    println!("   RPC URL        : {}", rpc_url);
    println!("   Main Wallet    : {}", main_wallet);
    println!("   AMM Pool       : {}", pool_id_str);
    println!("   AMM Program    : {}", amm_program);
    println!("   Swap Amount    : {:.6} SOL ({} lamports)", sol_amount, lamports);
    println!("   Jito Tip       : {:.6} SOL ({} lamports)", config.jito_tip, jito_tip_lam);
    println!("   Tip Address    : {}", tip_addr);
    println!("============================================================");

    // ── Balance check ─────────────────────────────────────────────────────────
    let balance = match rpc.get_balance(&main_wallet).await {
        Ok(b) => b,
        Err(e) => { println!("❌ Cannot reach Mainnet RPC: {}", e); return; }
    };
    println!("   Wallet Balance : {:.6} SOL", balance as f64 / 1e9);

    let min_needed = lamports + jito_tip_lam + 5_000_000; // swap + tip + fees
    if balance < min_needed {
        println!("❌ Insufficient balance. Need at least {:.6} SOL (swap + tip + fees).", min_needed as f64 / 1e9);
        println!("   Current balance: {:.6} SOL", balance as f64 / 1e9);
        return;
    }
    println!("   ✅ Balance sufficient.\n");

    // ── Fetch PoolInfo from AMM account ───────────────────────────────────────
    println!("🔍 Fetching PoolInfo from Mainnet AMM account...");
    let pool_info = match get_pool_info_from_amm_account(&rpc, &pool_id, amm_program).await {
        Some(info) => {
            println!("✅ PoolInfo ready.");
            println!("   Token Mint  : {}", info.base_mint);
            println!("   Quote (WSOL): {}", info.quote_mint);
            println!("   Market ID   : {}", info.market_id);
            info
        }
        None => {
            println!("❌ Failed to reconstruct PoolInfo.");
            println!("   Check that pool {} exists and uses Raydium AMM V4.", pool_id_str);
            return;
        }
    };

    // ── Get fresh blockhash ───────────────────────────────────────────────────
    let bh = match rpc.get_latest_blockhash().await {
        Ok(h) => h,
        Err(e) => { println!("❌ Failed to get blockhash: {}", e); return; }
    };
    println!("   Blockhash: {}\n", bh);

    // ── Build TX 1: Jito Tip ──────────────────────────────────────────────────
    #[allow(deprecated)]
    let tip_ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(3_000),
        system_instruction::transfer(&main_wallet, &tip_addr, jito_tip_lam),
    ];
    let tip_msg = match Message::try_compile(&main_wallet, &tip_ixs, &[], bh) {
        Ok(m) => m,
        Err(e) => { println!("❌ Failed to compile tip message: {}", e); return; }
    };
    let tx_tip = match VersionedTransaction::try_new(VersionedMessage::V0(tip_msg), &[&config.keypair]) {
        Ok(t) => t,
        Err(e) => { println!("❌ Failed to sign tip TX: {}", e); return; }
    };

    // ── Build TX 2: Raydium Swap ──────────────────────────────────────────────
    let tx_swap = match bundle_buy::build_raydium_buy_tx_for_wallet(
        &config.keypair,
        &pool_info,
        lamports,
        0,               // min_token_out = 0 (accept any price for test)
        config.cu_limit,
        config.priority_fee,
        bh,
    ) {
        Some(t) => t,
        None => { println!("❌ Failed to build swap transaction."); return; }
    };

    let sig_tip  = tx_tip.signatures[0];
    let sig_swap = tx_swap.signatures[0];

    println!("📦 Bundle assembled (2 transactions):");
    println!("   TX[0] Jito Tip  sig: {}", sig_tip);
    println!("   TX[1] Swap      sig: {}", sig_swap);
    println!("   🔗 Swap Solscan: https://solscan.io/tx/{}", sig_swap);

    println!("\n🔍 Simulating Swap TX to ensure it won't fail...");
    match rpc.simulate_transaction(&tx_swap).await {
        Ok(sim) => {
            if let Some(err) = sim.value.err {
                println!("❌ SIMULATION FAILED: {:?}", err);
                if let Some(logs) = sim.value.logs {
                    for log in logs {
                        println!("   Log: {}", log);
                    }
                }
                println!("⚠️  Jito validators automatically drop bundles containing failing transactions.");
                println!("   We will not submit this bundle.");
                return;
            } else {
                println!("✅ Simulation successful! The transaction is valid.");
            }
        }
        Err(e) => println!("⚠️ Failed to run simulation: {}", e),
    }


    // ── Fire bundle to ALL Jito Mainnet endpoints ─────────────────────────────
    let bundle  = vec![encode_tx(&tx_tip), encode_tx(&tx_swap)];
    let urls    = jito_bundle_urls();
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendBundle",
        "params": [bundle, {"encoding": "base64"}]
    });

    println!("\n🚀 Firing bundle to {} Jito Mainnet endpoints simultaneously...", urls.len());

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build().unwrap();

    let futs: Vec<_> = urls.iter().map(|url| {
        let http    = http.clone();
        let payload = payload.clone();
        let url     = url.clone();
        async move {
            let region = url.split("://").nth(1)
                .and_then(|s| s.split('.').next())
                .unwrap_or("?").to_string();
            match http.post(&url).json(&payload).send().await {
                Ok(res) => match res.json::<serde_json::Value>().await {
                    Ok(json) => {
                        if let Some(bid) = json["result"].as_str() {
                            println!("   ✅ [{}] bundle accepted → {}", region, &bid[..bid.len().min(24)]);
                            Ok((region, bid.to_string()))
                        } else {
                            let err = json["error"]["message"].as_str().unwrap_or("unknown");
                            println!("   ❌ [{}] rejected: {}", region, err);
                            Err(format!("[{}] {}", region, err))
                        }
                    }
                    Err(e) => { println!("   ❌ [{}] decode error: {}", region, e); Err(e.to_string()) }
                },
                Err(e) => {
                    let short = if e.to_string().contains("dns") || e.to_string().contains("timeout") {
                        "timeout/offline"
                    } else { "connection error" };
                    println!("   ❌ [{}] {}", region, short);
                    Err(short.to_string())
                }
            }
        }
    }).collect();

    let start        = Instant::now();
    let results      = futures::future::join_all(futs).await;
    let bundle_id: Option<String> = results.into_iter().find_map(|r| r.ok()).map(|(_, id)| id);

    match &bundle_id {
        Some(id) => println!("\n✅ Bundle submitted! ID: {}", id),
        None     => {
            println!("\n⚠️  All Jito endpoints rejected/timed out.");
            println!("   Possible reasons: rate-limited, blockhash expired, or TX error.");
            println!("   The swap TX was NOT sent to Mainnet.");
            return;
        }
    }

    // ── Poll for swap TX confirmation ─────────────────────────────────────────
    println!("\n⏳ Polling swap TX confirmation (up to {}s)...", POLL_TIMEOUT_SECS);
    let deadline = Instant::now() + Duration::from_secs(POLL_TIMEOUT_SECS);
    let mut confirmed = false;

    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(resp) = rpc.get_signature_statuses(&[sig_swap]).await {
            if let Some(Some(s)) = resp.value.first() {
                if s.confirmation_status.is_some() {
                    let elapsed = start.elapsed().as_millis();
                    if s.err.is_none() {
                        println!("\n🎉 SWAP CONFIRMED on Mainnet!");
                        println!("   Latency : {} ms", elapsed);
                        println!("   Slot    : {}", s.slot);
                        println!("   🔗 https://solscan.io/tx/{}", sig_swap);
                        if let Some(id) = &bundle_id {
                            println!("   🔗 https://explorer.jito.wtf/bundle/{}", id);
                        }
                    } else {
                        println!("\n❌ Swap TX landed but FAILED on-chain.");
                        println!("   Error   : {:?}", s.err);
                        println!("   Slot    : {}", s.slot);
                        println!("   Latency : {} ms", elapsed);
                        println!("   🔗 https://solscan.io/tx/{}", sig_swap);
                        println!("   Common causes:");
                        println!("     1. Pool has no liquidity");
                        println!("     2. WSOL ATA missing/unfunded");
                        println!("     3. Slippage: price moved, min_out not met");
                    }
                    confirmed = true;
                    break;
                }
            }
        }
        let elapsed = start.elapsed().as_secs();
        if elapsed % 5 == 0 {
            println!("   [{}s] waiting...", elapsed);
        }
    }

    if !confirmed {
        println!("\n⏳ Swap TX not confirmed within {}s.", POLL_TIMEOUT_SECS);
        println!("   The bundle may still be processing. Check:");
        println!("   🔗 https://solscan.io/tx/{}", sig_swap);
        if let Some(id) = &bundle_id {
            println!("   🔗 https://explorer.jito.wtf/bundle/{}", id);
        }
    }

    println!("\n============================================================");
    println!("   Tip TX  : https://solscan.io/tx/{}", sig_tip);
    println!("   Swap TX : https://solscan.io/tx/{}", sig_swap);
    println!("============================================================");
}
