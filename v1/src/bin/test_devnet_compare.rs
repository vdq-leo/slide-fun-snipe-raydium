use dotenvy;
use slidefun_raydium_snipe::{bundle_buy, config::Config, pool::get_pool_info_from_amm_account};
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
// DEVNET SWAP COMPARISON TESTER
//
// Compares two swap delivery methods on Solana Devnet:
//
//   Method A — Single RPC Send:
//     Build swap TX, call sendTransaction once.
//
//   Method B — RPC Spam (8x):
//     Build same swap TX, fire 8 concurrent sendTransaction calls.
//     First one to land wins; duplicates are silently dropped.
//
//   Method C — Jito Devnet Bundle (expected to FAIL):
//     Build swap TX + tip TX, send as Jito bundle to Devnet block engine.
//     Jito does NOT operate on Devnet → DNS failure / timeout.
//     Jito swap TX is then submitted via single RPC as a fallback.
//
// Recorded: confirmation latency (ms), slot landed, final status.
// ─────────────────────────────────────────────────────────────────

const DEVNET_RAYDIUM_PROGRAM: &str = "HWy1jotHpo6UqeQxx49dpYYdQB8wj9Qk9MdxwjLvDHB8";
const DEFAULT_DEVNET_POOL:    &str = "GYERH2LCgTmfv3RSJ8emP5E5SWQoiUL5LbrDwuHKhhWZ";
const JITO_DEVNET_ENGINE:     &str = "https://devnet.block-engine.jito.wtf/api/v1/bundles";
const JITO_DEVNET_TIP_ADDR:   &str = "Cw8CFytr45SKCx4YUNmbvWGzKrrJ4M7CQA44qyRPFs1A";
const SPAM_COUNT:              usize = 8;
const POLL_TIMEOUT_SECS:       u64  = 30;

fn encode_tx(tx: &VersionedTransaction) -> String {
    let bytes = bincode::serialize(tx).unwrap();
    base64::engine::general_purpose::STANDARD.encode(&bytes)
}

/// Status of a confirmed/failed TX
#[derive(Clone)]
struct TxResult {
    status:  String,
    latency: Option<Duration>,
    slot:    Option<u64>,
}

impl TxResult {
    fn pending() -> Self { Self { status: "⏳ Pending...".to_string(), latency: None, slot: None } }
    fn timeout() -> Self { Self { status: "⌛ Timeout / Not landed".to_string(), latency: None, slot: None } }
    fn confirmed(latency: Duration, slot: u64) -> Self {
        Self { status: "✅ Confirmed".to_string(), latency: Some(latency), slot: Some(slot) }
    }
    fn failed(err: String, latency: Duration, slot: u64) -> Self {
        // Truncate long error strings for table readability
        let short = if err.len() > 25 { format!("{}…", &err[..25]) } else { err };
        Self { status: format!("❌ {}", short), latency: Some(latency), slot: Some(slot) }
    }
    fn rejected(reason: &str) -> Self {
        let short = if reason.len() > 24 { format!("{}…", &reason[..24]) } else { reason.to_string() };
        Self { status: format!("🚫 {}", short), latency: None, slot: None }
    }
    fn latency_str(&self) -> String {
        self.latency.map(|d| format!("{} ms", d.as_millis())).unwrap_or_else(|| "N/A".to_string())
    }
    fn slot_str(&self) -> String {
        self.slot.map(|s| s.to_string()).unwrap_or_else(|| "N/A".to_string())
    }
}

/// Poll a signature until confirmed/failed or timeout. Returns TxResult.
async fn poll_sig(
    rpc: Arc<RpcClient>,
    sig: solana_sdk::signature::Signature,
    start: Instant,
    timeout: Duration,
) -> TxResult {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(400)).await;
        if let Ok(resp) = rpc.get_signature_statuses(&[sig]).await {
            if let Some(Some(s)) = resp.value.first() {
                if s.confirmation_status.is_some() {
                    let elapsed = start.elapsed();
                    let slot = s.slot;
                    return if s.err.is_none() {
                        TxResult::confirmed(elapsed, slot)
                    } else {
                        TxResult::failed(format!("{:?}", s.err), elapsed, slot)
                    };
                }
            }
        }
    }
    TxResult::timeout()
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    // ── Config & RPC ────────────────────────────────────────────────────────────
    let config = Config::from_env();
    let main_wallet = config.keypair.pubkey();

    let rpc_url = if !config.helius_api_key.is_empty() {
        format!("https://devnet.helius-rpc.com/?api-key={}", config.helius_api_key)
    } else {
        "https://api.devnet.solana.com".to_string()
    };
    let rpc = Arc::new(RpcClient::new(rpc_url.clone()));

    // ── CLI args ─────────────────────────────────────────────────────────────────
    let args: Vec<String> = std::env::args().collect();
    let pool_id_str = if args.len() >= 2 { args[1].as_str() } else { DEFAULT_DEVNET_POOL };
    let pool_id = match Pubkey::from_str(pool_id_str) {
        Ok(pk) => pk,
        Err(_) => { println!("❌ Invalid pool address: {}", pool_id_str); return; }
    };
    let amm_program = Pubkey::from_str(DEVNET_RAYDIUM_PROGRAM).unwrap();

    println!("============================================================");
    println!(" 📊 DEVNET SWAP COMPARISON: Single RPC / Spam / Jito Bundle");
    println!("============================================================");
    println!("   Network        : SOLANA DEVNET (forced)");
    println!("   RPC URL        : {}", rpc_url);
    println!("   Main Wallet    : {}", main_wallet);
    println!("   AMM Pool       : {}", pool_id_str);
    println!("   AMM Program    : {}", amm_program);
    println!("   Spam count     : {}x concurrent sends", SPAM_COUNT);
    println!("============================================================");

    // ── Balance check ─────────────────────────────────────────────────────────
    let wallet_balance = match rpc.get_balance(&main_wallet).await {
        Ok(b) => b,
        Err(e) => { println!("❌ Cannot connect to Devnet: {}", e); return; }
    };
    println!("   Wallet Balance : {:.6} SOL (Devnet)", wallet_balance as f64 / 1e9);
    if wallet_balance < 100_000_000 {
        println!("❌ Need at least 0.1 SOL. Airdrop: solana airdrop 2 {} --url devnet", main_wallet);
        return;
    }

    // ── Pool info ─────────────────────────────────────────────────────────────
    println!("\n🔍 Fetching PoolInfo from AMM account...");
    let pool_info = match get_pool_info_from_amm_account(&rpc, &pool_id, amm_program).await {
        Some(info) => { println!("✅ PoolInfo ready."); info }
        None => { println!("❌ Failed to reconstruct PoolInfo."); return; }
    };

    // ── Fresh blockhash ───────────────────────────────────────────────────────
    let bh = match rpc.get_latest_blockhash().await {
        Ok(h) => h,
        Err(e) => { println!("❌ Failed to get blockhash: {}", e); return; }
    };

    // ── Build 3 independent swap transactions ─────────────────────────────────
    // Use slightly different amounts so each TX gets a unique signature.
    let build = |lamports: u64| {
        bundle_buy::build_raydium_buy_tx_for_wallet(
            &config.keypair,
            &pool_info,
            lamports,
            0,
            config.cu_limit,
            config.priority_fee,
            bh,
        )
    };

    let tx_single = match build(10_000_000) { Some(t) => t, None => { println!("❌ Build tx_single failed"); return; } };
    let tx_spam   = match build(10_000_001) { Some(t) => t, None => { println!("❌ Build tx_spam failed");   return; } };
    let tx_jito   = match build(10_000_002) { Some(t) => t, None => { println!("❌ Build tx_jito failed");   return; } };

    let sig_single = tx_single.signatures[0];
    let sig_spam   = tx_spam.signatures[0];
    let sig_jito   = tx_jito.signatures[0];

    // Build Jito tip TX for the bundle
    let tip_acc = Pubkey::from_str(JITO_DEVNET_TIP_ADDR).unwrap();
    #[allow(deprecated)]
    let tip_ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(3_000),
        system_instruction::transfer(&main_wallet, &tip_acc, 100_000),
    ];
    let tip_msg = Message::try_compile(&main_wallet, &tip_ixs, &[], bh).unwrap();
    let tx_tip  = VersionedTransaction::try_new(VersionedMessage::V0(tip_msg), &[&config.keypair]).unwrap();

    let bundle = vec![encode_tx(&tx_tip), encode_tx(&tx_jito)];

    println!("\n📦 Built 3 unique swap transactions:");
    println!("   [A] Single RPC  sig: {}", sig_single);
    println!("   [B] RPC Spam    sig: {}", sig_spam);
    println!("   [C] Jito Bundle sig: {}", sig_jito);

    // ── Fire all 3 methods concurrently ──────────────────────────────────────
    println!("\n🚀 Firing all 3 methods simultaneously...");

    // [A] Single send
    let rpc_a = Arc::clone(&rpc);
    let tx_a  = tx_single.clone();
    let start_a = Instant::now();
    let h_a = tokio::spawn(async move {
        match rpc_a.send_transaction(&tx_a).await {
            Ok(_)  => println!("   [A] Single RPC: sendTransaction OK"),
            Err(e) => println!("   [A] Single RPC: sendTransaction error: {}", e),
        }
    });

    // [B] Spam (SPAM_COUNT × concurrent)
    let rpc_b = Arc::clone(&rpc);
    let tx_b  = tx_spam.clone();
    let start_b = Instant::now();
    let h_b = tokio::spawn(async move {
        let futs: Vec<_> = (0..SPAM_COUNT).map(|_| {
            let r  = Arc::clone(&rpc_b);
            let tx = tx_b.clone();
            async move { let _ = r.send_transaction(&tx).await; }
        }).collect();
        futures::future::join_all(futs).await;
        println!("   [B] RPC Spam: all {} sends fired", SPAM_COUNT);
    });

    // [C] Jito bundle → fallback to single RPC send if bundle fails
    let rpc_c  = Arc::clone(&rpc);
    let tx_c   = tx_jito.clone();
    let start_c = Instant::now();
    let http    = reqwest::Client::builder()
        .timeout(Duration::from_secs(6))
        .build().unwrap();
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendBundle",
        "params": [bundle, {"encoding": "base64"}]
    });

    let mut jito_submission = "bundle_not_sent".to_string();
    let mut jito_bundle_id: Option<String> = None;

    match http.post(JITO_DEVNET_ENGINE).json(&payload).send().await {
        Ok(res) => {
            match res.json::<serde_json::Value>().await {
                Ok(json) => {
                    if let Some(bid) = json["result"].as_str() {
                        jito_bundle_id = Some(bid.to_string());
                        jito_submission = format!("bundle_accepted:{}", &bid[..bid.len().min(20)]);
                        println!("   [C] Jito Bundle: ✅ accepted — bundle_id {}", bid);
                    } else {
                        let err_msg = json["error"]["message"].as_str().unwrap_or("unknown error");
                        jito_submission = format!("bundle_rejected: {}", err_msg);
                        println!("   [C] Jito Bundle: ❌ rejected — {}", err_msg);
                        // Fallback: send Jito swap TX directly via RPC
                        println!("   [C] Jito Bundle: ⚡ fallback → single RPC send");
                        let _ = rpc_c.send_transaction(&tx_c).await;
                    }
                }
                Err(e) => {
                    jito_submission = format!("decode_error: {}", e);
                    println!("   [C] Jito Bundle: ❌ decode error — {}", e);
                    let _ = rpc_c.send_transaction(&tx_c).await;
                }
            }
        }
        Err(e) => {
            // DNS / connection error — Jito Devnet is offline
            let reason = if e.to_string().contains("dns error") || e.to_string().contains("nodename") {
                "Jito Devnet offline (DNS)"
            } else {
                "Connection failed"
            };
            jito_submission = format!("engine_offline: {}", reason);
            println!("   [C] Jito Bundle: 🚫 {} — falling back to RPC", reason);
            // Fallback: send Jito swap TX directly via RPC
            let _ = rpc_c.send_transaction(&tx_c).await;
        }
    }

    // Wait for fire tasks to finish
    let _ = tokio::join!(h_a, h_b);

    // ── Poll all 3 signatures concurrently ────────────────────────────────────
    println!("\n⏳ Polling for confirmations (up to {}s)...", POLL_TIMEOUT_SECS);
    let timeout = Duration::from_secs(POLL_TIMEOUT_SECS);

    let (res_a, res_b, res_c) = tokio::join!(
        poll_sig(Arc::clone(&rpc), sig_single, start_a, timeout),
        poll_sig(Arc::clone(&rpc), sig_spam,   start_b, timeout),
        poll_sig(Arc::clone(&rpc), sig_jito,   start_c, timeout),
    );

    // Overlay Jito rejection status if bundle was never accepted
    let res_c = if jito_bundle_id.is_none() && res_c.status.contains("Timeout") {
        TxResult::rejected(&jito_submission)
    } else {
        res_c
    };

    // ── Print comparison table ────────────────────────────────────────────────
    println!("\n=======================================================================================================");
    println!("                           🏆 FINAL COMPARISON — DEVNET SWAP DELIVERY METHODS");
    println!("=======================================================================================================");
    println!(" ┌──────────────────────────┬──────────────────────────┬──────────────────────────┬──────────────────┐");
    println!(" │ Metric                   │ [A] Single RPC Send      │ [B] RPC Spam ({}x)        │ [C] Jito Bundle  │", SPAM_COUNT);
    println!(" ├──────────────────────────┼──────────────────────────┼──────────────────────────┼──────────────────┤");
    println!(" │ Status                   │ {:<24} │ {:<24} │ {:<16} │", res_a.status, res_b.status, res_c.status);
    println!(" │ Confirmation Latency     │ {:<24} │ {:<24} │ {:<16} │", res_a.latency_str(), res_b.latency_str(), res_c.latency_str());
    println!(" │ Slot Landed              │ {:<24} │ {:<24} │ {:<16} │", res_a.slot_str(), res_b.slot_str(), res_c.slot_str());
    println!(" └──────────────────────────┴──────────────────────────┴──────────────────────────┴──────────────────┘");
    println!("=======================================================================================================");
    println!();
    println!(" Jito Devnet submission : {}", jito_submission);
    if let Some(bid) = &jito_bundle_id {
        println!(" Jito Bundle Explorer  : https://explorer.jito.wtf/bundle/{}", bid);
    }
    println!();
    println!(" 🔗 [A] https://solscan.io/tx/{}?cluster=devnet", sig_single);
    println!(" 🔗 [B] https://solscan.io/tx/{}?cluster=devnet", sig_spam);
    println!(" 🔗 [C] https://solscan.io/tx/{}?cluster=devnet", sig_jito);
    println!("=======================================================================================================");
    println!();
    println!(" Notes:");
    println!("   • Jito does NOT operate a block engine on Devnet.");
    println!("     The [C] swap TX is submitted via direct RPC after bundle failure.");
    println!("   • On Mainnet, replace JITO_DEVNET_ENGINE with:");
    println!("     https://ny.mainnet.block-engine.jito.wtf/api/v1/bundles");
    println!("   • RPC Spam wins on Devnet; Jito excels on Mainnet for atomic inclusion.");
    println!("=======================================================================================================");
}
