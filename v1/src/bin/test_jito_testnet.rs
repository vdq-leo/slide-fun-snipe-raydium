use base64::Engine;
use bincode;
use dotenvy;
use slidefun_raydium_snipe::config::Config;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    message::{v0::Message, VersionedMessage},
    pubkey::Pubkey,
    signer::Signer,
    system_instruction,
    transaction::VersionedTransaction,
};
use std::str::FromStr;
use std::sync::Arc;
use chrono;

/// Encode a versioned transaction to base64 (Jito's recommended format; base58 is deprecated).
fn encode(tx: &VersionedTransaction) -> String {
    let bytes = bincode::serialize(tx).unwrap();
    base64::engine::general_purpose::STANDARD.encode(&bytes)
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    
    // 1. Load configuration and private keys
    let config = Config::from_env();
    let main_wallet = config.keypair.pubkey();
    
    // 2. Target network is Solana Testnet
    let rpc_url = "https://api.testnet.solana.com".to_string();
    let rpc = Arc::new(RpcClient::new(rpc_url.clone()));
    
    println!("============================================================");
    println!("   🧪 JITO BUNDLE TESTNET TESTER (0 Cost Sandbox)");
    println!("============================================================");
    println!("   Target Network : SOLANA TESTNET");
    println!("   RPC URL        : {}", rpc_url);
    println!("   Main Wallet    : {}", main_wallet);
    
    // 3. Check Testnet balance
    let balance = match rpc.get_balance(&main_wallet).await {
        Ok(bal) => bal,
        Err(e) => {
            println!("❌ Error fetching balance: {}", e);
            println!("💡 Make sure your wallet is active on Testnet.");
            return;
        }
    };
    
    println!("   Wallet Balance : {:.6} SOL", balance as f64 / 1e9);
    
    // 4. Request airdrop if balance is low
    if balance < 10_000_000 { // Less than 0.01 SOL
        println!("\n⚠️  Balance is very low. Requesting a 1 SOL testnet airdrop...");
        match rpc.request_airdrop(&main_wallet, 1_000_000_000).await {
            Ok(sig) => {
                println!("✅ Airdrop requested! TX: {}", sig);
                println!("   Waiting for confirmation...");
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
            Err(e) => {
                println!("❌ Airdrop request failed: {:?}", e);
                println!("💡 Solana testnet faucets are heavily rate-limited.");
                println!("   Please get free Testnet SOL from https://faucet.solana.com/ (select Testnet)");
                println!("   and transfer it to: {}", main_wallet);
                return;
            }
        }
    }
    
    // 5. Get fresh blockhash
    let bh = match rpc.get_latest_blockhash().await {
        Ok(hash) => {
            println!("\n✅ Blockhash cached: {}", hash);
            hash
        }
        Err(e) => {
            println!("❌ Failed to get recent blockhash: {}", e);
            return;
        }
    };
    
    // 6. Define Jito Testnet Block Engine URL
    let jito_testnet_engine = "https://ny.testnet.block-engine.jito.wtf/api/v1/bundles";
    println!("   Jito Testnet Engine: {}", jito_testnet_engine);
    
    // 7. Query active tip accounts dynamically from Jito Testnet Block Engine
    println!("\n🔍 Fetching active Jito Testnet tip accounts dynamically...");
    let http_client = reqwest::Client::new();
    let tip_accounts_payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getTipAccounts",
        "params": []
    });
    
    let tip_accounts: Vec<String> = match http_client.post(jito_testnet_engine).json(&tip_accounts_payload).send().await {
        Ok(res) => {
            let json_res: serde_json::Value = res.json().await.unwrap_or_default();
            if let Some(arr) = json_res["result"].as_array() {
                arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()
            } else {
                vec![]
            }
        }
        Err(e) => {
            println!("⚠️  Failed to query tip accounts: {}. Falling back to default testnet tip account.", e);
            vec![]
        }
    };
    
    let jito_testnet_tip_acc_str = if !tip_accounts.is_empty() {
        let index = (chrono::Utc::now().timestamp_millis() as usize) % tip_accounts.len();
        let picked = tip_accounts[index].clone();
        println!("✅ Fetched {} active Jito tip accounts dynamically.", tip_accounts.len());
        picked
    } else {
        // Fallback Jito Testnet tip account
        "84DrGKhycCUGfLzw8hXsUYX9SnWdh2wW3ozsTPrC5xyg".to_string()
    };
    
    let jito_testnet_tip_acc = Pubkey::from_str(&jito_testnet_tip_acc_str).unwrap();
    let testnet_tip_lamports = 100_000; // Tiny 0.0001 Testnet SOL tip (0 real cost!)
    
    println!("   Jito Selected Tip Account: {}", jito_testnet_tip_acc);
    println!("   Jito Tip Amount          : {} lamports (0.0001 SOL)", testnet_tip_lamports);
    
    // 8. Build Transaction 1: The Testnet Jito Tip
    println!("\n🔨 Building Transaction 1 (Jito Tip)...");
    let tip_ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(3_000),
        system_instruction::transfer(&main_wallet, &jito_testnet_tip_acc, testnet_tip_lamports),
    ];
    let tip_msg = Message::try_compile(&main_wallet, &tip_ixs, &[], bh).unwrap();
    let tip_tx = VersionedTransaction::try_new(VersionedMessage::V0(tip_msg), &[&config.keypair]).unwrap();
    let tip_encoded = encode(&tip_tx);
    
    // 9. Build Transaction 2: Simple Testnet System Transfer (transfer 1 lamport to itself to create unique sig)
    println!("🔨 Building Transaction 2 (Sandboxed Transfer)...");
    let tx2_ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(3_000),
        system_instruction::transfer(&main_wallet, &main_wallet, 1),
    ];
    let tx2_msg = Message::try_compile(&main_wallet, &tx2_ixs, &[], bh).unwrap();
    let tx2 = VersionedTransaction::try_new(VersionedMessage::V0(tx2_msg), &[&config.keypair]).unwrap();
    let tx2_encoded = encode(&tx2);
    
    // 10. Assemble into a Jito Bundle
    let bundle = vec![tip_encoded, tx2_encoded];
    println!("\n📦 Jito Bundle Packed successfully ({} transactions included).", bundle.len());
    
    // 11. Fire bundle to Jito Testnet Block Engine
    // IMPORTANT: Jito requires base64 encoding (base58 is deprecated).
    // Pass the encoding option as the second element of params array.
    println!("\n🚀 Firing Bundle to Jito Testnet Block Engine (base64-encoded)...");
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendBundle",
        "params": [bundle, {"encoding": "base64"}]
    });
    
    let response = match http_client.post(jito_testnet_engine).json(&payload).send().await {
        Ok(res) => res,
        Err(e) => {
            println!("❌ HTTP request to Jito Testnet Block Engine failed: {}", e);
            return;
        }
    };
    
    let json_res: serde_json::Value = response.json().await.unwrap_or_default();
    let bundle_id = match json_res["result"].as_str() {
        Some(id) => {
            println!("✅ Bundle submitted! Bundle ID: {}", id);
            println!("   🔗 Track on Jito Explorer: https://explorer.jito.wtf/bundle/{}", id);
            println!("   🔗 Track on Solscan (Testnet): https://solscan.io/tx/{}?cluster=testnet", tip_tx.signatures[0]);
            id.to_string()
        }
        _ => {
            println!("❌ Jito Testnet Block Engine rejected the bundle.");
            println!("   Response: {:?}", json_res);
            println!("💡 Common reasons:");
            println!("   Common reasons:");
            println!("   1. No Testnet validators running Jito-Solana client won a recent slot.");
            println!("   2. Testnet engine is undergoing maintenance or experiencing downtime.");
            return;
        }
    };
    
    // 12. Poll bundle status on Testnet to check for atomic inclusion
    println!("\n⏳ Polling Jito Testnet Engine for bundle confirmation status...");
    
    for attempt in 1..=8 {
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
        
        let status_payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getBundleStatuses",
            "params": [[bundle_id.clone()]]
        });
        
        let status_res = match http_client.post(jito_testnet_engine).json(&status_payload).send().await {
            Ok(res) => res.json::<serde_json::Value>().await.unwrap_or_default(),
            Err(_) => continue,
        };
        
        let value = &status_res["result"]["value"];
        println!("   [Attempt {}/8] Status: {:?}", attempt, value);
        
        if value.is_array() && !value.as_array().unwrap().is_empty() {
            let confirmation = &value[0];
            let slot = confirmation["slot"].as_u64().unwrap_or(0);
            let confirmations = confirmation["confirmation_status"].as_str().unwrap_or("unknown");
            
            println!("\n🎉 BUNDLE CONFIRMED ON TESTNET!");
            println!("   Slot                : {}", slot);
            println!("   Confirmation Status : {}", confirmations);
            println!("   Verify on Solscan   : https://solscan.io/tx/{}?cluster=testnet", tip_tx.signatures[0]);
            return;
        }
    }
    
    println!("\n⚠️  Bundle did not get included within 24 seconds.");
    println!("   If the bundle was dropped, it is likely because no Testnet validators currently running");
    println!("   the Jito-Solana client won the leadership slot in the last few seconds (very common on Testnet).");
    println!("   However, your Testnet SOL remains completely safe and 0 real money was spent!");
}
