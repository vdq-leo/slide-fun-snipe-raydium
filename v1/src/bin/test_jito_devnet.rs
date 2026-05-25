use bincode;
use bs58;
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

fn encode(tx: &VersionedTransaction) -> String {
    let bytes = bincode::serialize(tx).unwrap();
    bs58::encode(&bytes).into_string()
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    
    // 1. Load configuration and private keys
    let config = Config::from_env();
    let main_wallet = config.keypair.pubkey();
    
    // 2. Select RPC URL (use Helius Devnet if key is available, fallback to public Devnet)
    let rpc_url = if !config.helius_api_key.is_empty() {
        format!("https://devnet.helius-rpc.com/?api-key={}", config.helius_api_key)
    } else {
        "https://api.devnet.solana.com".to_string()
    };
    
    let rpc = Arc::new(RpcClient::new(rpc_url.clone()));
    
    println!("============================================================");
    println!("   🧪 JITO BUNDLE DEVNET TESTER (0 Cost Sandbox)");
    println!("============================================================");
    println!("   Target Network : SOLANA DEVNET");
    println!("   RPC URL        : {}", rpc_url);
    println!("   Main Wallet    : {}", main_wallet);
    
    // 3. Check devnet balance
    let balance = match rpc.get_balance(&main_wallet).await {
        Ok(bal) => bal,
        Err(e) => {
            println!("❌ Error fetching balance: {}", e);
            println!("💡 Make sure your wallet is active on Devnet.");
            return;
        }
    };
    
    println!("   Wallet Balance : {:.6} SOL", balance as f64 / 1e9);
    
    // 4. Request airdrop if balance is low
    if balance < 10_000_000 { // Less than 0.01 SOL
        println!("\n⚠️  Balance is very low. Requesting a 1 SOL devnet airdrop...");
        match rpc.request_airdrop(&main_wallet, 1_000_000_000).await {
            Ok(sig) => {
                println!("✅ Airdrop requested! TX: {}", sig);
                println!("   Waiting for confirmation...");
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
            Err(e) => {
                println!("❌ Airdrop request failed: {:?}", e);
                println!("💡 Solana devnet faucets are heavily rate-limited.");
                println!("   Please get free Devnet SOL from https://faucet.solana.com/ or https://solfaucet.com/");
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
    
    // 6. Select Jito Devnet Block Engine and Jito Devnet Tip Account
    // Devnet engine: https://devnet.block-engine.jito.wtf/api/v1/bundles
    // Devnet tip account (any of the 8 valid ones): Cw8CFytr45SKCx4YUNmbvWGzKrrJ4M7CQA44qyRPFs1A
    let jito_devnet_engine = "https://devnet.block-engine.jito.wtf/api/v1/bundles";
    let jito_devnet_tip_acc = Pubkey::from_str("Cw8CFytr45SKCx4YUNmbvWGzKrrJ4M7CQA44qyRPFs1A").unwrap();
    let devnet_tip_lamports = 100_000; // Tiny 0.0001 Devnet SOL tip (0 real cost!)
    
    println!("   Jito Devnet Engine : {}", jito_devnet_engine);
    println!("   Jito Devnet Tip Acc: {}", jito_devnet_tip_acc);
    println!("   Jito Tip Amount    : {} lamports (0.0001 SOL)", devnet_tip_lamports);
    
    // 7. Build Transaction 1: The Devnet Jito Tip
    println!("\n🔨 Building Transaction 1 (Jito Tip)...");
    let tip_ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(3_000),
        system_instruction::transfer(&main_wallet, &jito_devnet_tip_acc, devnet_tip_lamports),
    ];
    let tip_msg = Message::try_compile(&main_wallet, &tip_ixs, &[], bh).unwrap();
    let tip_tx = VersionedTransaction::try_new(VersionedMessage::V0(tip_msg), &[&config.keypair]).unwrap();
    let tip_encoded = encode(&tip_tx);
    
    // 8. Build Transaction 2: Simple Devnet System Transfer (transfer 1 lamport to itself to create unique sig)
    println!("🔨 Building Transaction 2 (Sandboxed Buy/Transfer)...");
    let tx2_ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(3_000),
        system_instruction::transfer(&main_wallet, &main_wallet, 1),
    ];
    let tx2_msg = Message::try_compile(&main_wallet, &tx2_ixs, &[], bh).unwrap();
    let tx2 = VersionedTransaction::try_new(VersionedMessage::V0(tx2_msg), &[&config.keypair]).unwrap();
    let tx2_encoded = encode(&tx2);
    
    // 9. Assemble into a Jito Bundle
    let bundle = vec![tip_encoded, tx2_encoded];
    println!("\n📦 Jito Bundle Packed successfully ({} transactions included).", bundle.len());
    
    // 10. Fire bundle to Jito Devnet Block Engine
    println!("\n🚀 Firing Bundle to Jito Devnet Block Engine...");
    let http_client = reqwest::Client::new();
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendBundle",
        "params": [bundle]
    });
    
    let jito_result = async {
        let response = http_client.post(jito_devnet_engine).json(&payload).send().await?;
        let json_res: serde_json::Value = response.json().await?;
        let bundle_id = json_res["result"].as_str().ok_or("No bundle_id in response")?.to_string();
        Ok::<String, Box<dyn std::error::Error + Send + Sync>>(bundle_id)
    }.await;

    match jito_result {
        Ok(bundle_id) => {
            println!("✅ Bundle submitted successfully! ID: {}", bundle_id);
            println!("   Check status at: https://explorer.jito.wtf/bundle/{}", bundle_id);
            
            // 11. Poll bundle status on Devnet to check for atomic inclusion
            println!("\n⏳ Polling Jito Devnet Engine for bundle confirmation status...");
            for attempt in 1..=8 {
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                
                let status_payload = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "getBundleStatuses",
                    "params": [[bundle_id.clone()]]
                });
                
                let status_res = match http_client.post(jito_devnet_engine).json(&status_payload).send().await {
                    Ok(res) => res.json::<serde_json::Value>().await.unwrap_or_default(),
                    Err(_) => continue,
                };
                
                let value = &status_res["result"]["value"];
                println!("   [Attempt {}/8] Status: {:?}", attempt, value);
                
                if value.is_array() && !value.as_array().unwrap().is_empty() {
                    let confirmation = &value[0];
                    let slot = confirmation["slot"].as_u64().unwrap_or(0);
                    let confirmations = confirmation["confirmation_status"].as_str().unwrap_or("unknown");
                    
                    println!("\n🎉 BUNDLE CONFIRMED ON DEVNET!");
                    println!("   Slot                : {}", slot);
                    println!("   Confirmation Status : {}", confirmations);
                    println!("   Verify on Solscan   : https://solscan.io/tx/{}?cluster=devnet", tip_tx.signatures[0]);
                    return;
                }
            }
            
            println!("\n⚠️  Bundle did not get included within 24 seconds.");
            println!("   If the bundle was dropped, it is likely because no Devnet validators currently running");
            println!("   the Jito-Solana client won the leadership slot in the last few seconds.");
        }
        Err(e) => {
            println!("⚠️  Jito Devnet Block Engine is offline or rejected the bundle: {:?}", e);
            println!("\n============================================================");
            println!("⚠️  JITO BYPASSED: FALLING BACK TO DIRECT RPC DEVNET MODE");
            println!("============================================================");
            println!("   Since Jito does not run Block Engines on Solana Devnet, the bot");
            println!("   automatically falls back to sending transactions directly via RPC.");
            println!("   Executing Transaction 2 (Sandboxed Transfer) directly via RPC...");
            
            match rpc.send_transaction(&tx2).await {
                Ok(sig) => {
                    println!("✅ Transaction sent directly! TX: {}", sig);
                    println!("   Waiting for Devnet confirmation...");
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    
                    match rpc.get_signature_status(&sig).await {
                        Ok(Some(Ok(()))) => {
                            println!("\n🎉 TRANSACTION CONFIRMED ON DEVNET!");
                            println!("   Verify on Solscan: https://solscan.io/tx/{}?cluster=devnet", sig);
                        }
                        _ => {
                            println!("⏳ Transaction sent, check Solscan later: https://solscan.io/tx/{}?cluster=devnet", sig);
                        }
                    }
                }
                Err(err) => {
                    println!("❌ Direct RPC send failed: {}", err);
                }
            }
        }
    }
}
