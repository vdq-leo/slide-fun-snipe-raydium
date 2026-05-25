use base64::Engine;
use bincode;
use dotenvy;
use slidefun_raydium_snipe::config::Config;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    message::{v0::Message, VersionedMessage},
    pubkey::Pubkey,
    signer::{keypair::Keypair, Signer},
    system_instruction,
    transaction::VersionedTransaction,
};
use std::str::FromStr;
use std::sync::Arc;

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
    
    // 2. Select Mainnet RPC URL
    let rpc_url = if !config.helius_api_key.is_empty() {
        format!("https://mainnet.helius-rpc.com/?api-key={}", config.helius_api_key)
    } else {
        "https://api.mainnet-beta.solana.com".to_string()
    };
    
    let rpc = Arc::new(RpcClient::new(rpc_url.clone()));
    
    println!("============================================================");
    println!("   🔥 JITO BUNDLE MAINNET MICRO-TIP TESTER (Micro-Cost Sandbox)");
    println!("============================================================");
    println!("   Target Network : SOLANA MAINNET");
    println!("   RPC URL        : {}", rpc_url);
    println!("   Main Wallet    : {}", main_wallet);
    
    // 3. Check mainnet balance
    let balance = match rpc.get_balance(&main_wallet).await {
        Ok(bal) => bal,
        Err(e) => {
            println!("❌ Error fetching balance: {}", e);
            println!("💡 Make sure you have configured a valid private key in config.json.");
            return;
        }
    };
    
    println!("   Wallet Balance : {:.8} SOL", balance as f64 / 1e9);
    
    if balance < 100_000 { // Less than 0.0001 SOL
        println!("\n❌ Insufficient balance to run a micro-tip test.");
        println!("💡 Since Jito does not run on Devnet, you must run this on Mainnet.");
        println!("   However, the cost is extremely microscopic! You only need ~0.00005 SOL ($0.008 USD) total.");
        println!("   Please paste a funded private key in config.json and try again.");
        return;
    }
    
    // 4. Get fresh blockhash
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
    
    // 5. Select Jito Mainnet Block Engine and Jito Mainnet Tip Account
    let jito_mainnet_engine = "https://ny.mainnet.block-engine.jito.wtf/api/v1/bundles";
    let jito_mainnet_tip_acc = Pubkey::from_str("96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5").unwrap();
    
    // Set a microscopic tip: 10,000 lamports (0.00001 SOL = $0.0015 USD!)
    // Jito requires at least 1,000 lamports for a tip, so 10,000 lamports is fully valid.
    let tip_lamports = 10_000; 
    
    println!("   Jito Mainnet Engine : {}", jito_mainnet_engine);
    println!("   Jito Mainnet Tip Acc: {}", jito_mainnet_tip_acc);
    println!("   Micro Tip Amount    : {} lamports (0.00001 SOL / ~$0.0015 USD)", tip_lamports);
    
    // 6. Build Transaction 1: The Micro-Tip
    println!("\n🔨 Building Transaction 1 (Micro Jito Tip)...");
    let tip_ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(3_000),
        system_instruction::transfer(&main_wallet, &jito_mainnet_tip_acc, tip_lamports),
    ];
    let tip_msg = Message::try_compile(&main_wallet, &tip_ixs, &[], bh).unwrap();
    let tip_tx = VersionedTransaction::try_new(VersionedMessage::V0(tip_msg), &[&config.keypair]).unwrap();
    let tip_encoded = encode(&tip_tx);
    
    // 7. Build Transaction 2: Micro transfer (transfer 1 lamport to itself for signature uniqueness)
    println!("🔨 Building Transaction 2 (Mock Swap/Transfer)...");
    let tx2_ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(3_000),
        system_instruction::transfer(&main_wallet, &main_wallet, 1),
    ];
    let tx2_msg = Message::try_compile(&main_wallet, &tx2_ixs, &[], bh).unwrap();
    let tx2 = VersionedTransaction::try_new(VersionedMessage::V0(tx2_msg), &[&config.keypair]).unwrap();
    let tx2_encoded = encode(&tx2);
    
    // 8. Pack bundle
    let bundle = vec![tip_encoded, tx2_encoded];
    println!("\n📦 Jito Bundle Packed ({} transactions included).", bundle.len());
    
    // 9. Fire to Jito Block Engine
    // IMPORTANT: Jito requires base64 encoding (base58 is deprecated).
    // Pass the encoding option as second element of the params array.
    println!("\n🚀 Firing Bundle to Jito Mainnet Block Engine (base64-encoded)...");
    let http_client = reqwest::Client::new();
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendBundle",
        "params": [bundle, {"encoding": "base64"}]
    });
    
    let response = match http_client.post(jito_mainnet_engine).json(&payload).send().await {
        Ok(res) => res,
        Err(e) => {
            println!("❌ HTTP request to Jito Mainnet Block Engine failed: {}", e);
            return;
        }
    };
    
    let json_res: serde_json::Value = response.json().await.unwrap_or_default();
    let bundle_id = match json_res["result"].as_str() {
        Some(id) => {
            println!("✅ Bundle submitted successfully! ID: {}", id);
            println!("   Check status at: https://explorer.jito.wtf/bundle/{}", id);
            id.to_string()
        }
        _ => {
            println!("❌ Jito Mainnet Block Engine rejected the bundle: {:?}", json_res);
            return;
        }
    };
    
    // 10. Poll status
    println!("\n⏳ Polling Jito Mainnet for bundle confirmation...");
    for attempt in 1..=12 {
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        
        let status_payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getBundleStatuses",
            "params": [[bundle_id.clone()]]
        });
        
        let status_res = match http_client.post(jito_mainnet_engine).json(&status_payload).send().await {
            Ok(res) => res.json::<serde_json::Value>().await.unwrap_or_default(),
            Err(_) => continue,
        };
        
        let value = &status_res["result"]["value"];
        println!("   [Attempt {}/12] Status: {:?}", attempt, value);
        
        if value.is_array() && !value.as_array().unwrap().is_empty() {
            let confirmation = &value[0];
            let slot = confirmation["slot"].as_u64().unwrap_or(0);
            let confirmations = confirmation["confirmation_status"].as_str().unwrap_or("unknown");
            
            println!("\n🎉 BUNDLE CONFIRMED ON MAINNET!");
            println!("   Slot                : {}", slot);
            println!("   Confirmation Status : {}", confirmations);
            println!("   Verify on Solscan   : https://solscan.io/tx/{}", tip_tx.signatures[0]);
            return;
        }
    }
    
    println!("\n⚠️  Bundle did not get included within 24 seconds (dropped/expired).");
    println!("   Mainnet block engines may occasionally drop bundles if validator contention is high.");
    println!("   However, if it dropped, you lost 0 SOL!");
}
