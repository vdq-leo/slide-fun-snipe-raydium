// ==================== HANDLER: Execute the snipe ====================
// When a Slide.fun graduated token's Raydium pool is detected,
// build and fire the swap transaction as fast as possible.

use crate::{
    blockhash::get_blockhash, config::Config, constants::WSOL_MINT, log_info,
    trades::{Trade, TradesStore}, transaction::build_swap_instruction, types::PoolInfo,
};
use base64::Engine;
use solana_client::{nonblocking::rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};
#[allow(deprecated)]
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    message::{v0::Message, VersionedMessage},
    native_token::LAMPORTS_PER_SOL,
    pubkey::Pubkey,
    signer::Signer,
    system_instruction,
    transaction::VersionedTransaction,
};
use spl_associated_token_account::{
    get_associated_token_address, get_associated_token_address_with_program_id,
    instruction::create_associated_token_account_idempotent,
};
use std::{str::FromStr, sync::Arc};

pub async fn handle_buy(
    config: &Config,
    rpc_client: Arc<RpcClient>,
    pool_info: PoolInfo,
    ata_pre_created: bool,
    trades: Arc<TradesStore>,
    event_time: std::time::Instant,
) {
    log_info!("\n[SNIPE] 🎯 SLIDE-FUN GRADUATED TOKEN DETECTED ON RAYDIUM!");
    log_info!("   AMM ID: {}", pool_info.amm_id);
    log_info!("   Token: {}", pool_info.base_mint);
    if pool_info.pool_sol_amount > 0 {
        log_info!(
            "   Pool size: {:.4} SOL",
            pool_info.pool_sol_amount as f64 / 1e9
        );
    }

    let user = config.keypair.pubkey();
    let base_token_program = pool_info.base_token_program;
    let jito_tip_address = Pubkey::from_str(crate::constants::jito_tip_address()).unwrap();
    let wsol_mint = Pubkey::from_str(WSOL_MINT).unwrap();

    let sol_lamports = (config.sol_amount * LAMPORTS_PER_SOL as f64) as u64;
    let min_token_out = 1; // 100% slippage - buy at any price for speed

    let user_token_account = get_associated_token_address_with_program_id(
        &user,
        &pool_info.base_mint,
        &base_token_program,
    );
    let user_wsol_account = get_associated_token_address(&user, &wsol_mint);

    // === BUILD TX: Minimal instructions for maximum speed ===
    let mut instructions: Vec<Instruction> = vec![];

    // 1-2. Compute budget
    instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(
        config.cu_limit,
    ));
    instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
        config.priority_fee,
    ));

    // 3. Create ATA for new token (skip if already pre-created during graduation detection)
    if !ata_pre_created {
        instructions.push(create_associated_token_account_idempotent(
            &user,
            &user,
            &pool_info.base_mint,
            &base_token_program,
        ));
        log_info!("   [+] ATA creation included in TX");
    } else {
        log_info!("   [+] ATA pre-created ✅ - skipping instruction (4 ix instead of 5)");
    }

    // 4. Swap WSOL -> Token
    instructions.push(build_swap_instruction(
        &pool_info,
        user,
        user_wsol_account,
        user_token_account,
        sol_lamports,
        min_token_out,
        base_token_program,
    ));

    // 5. Jito tip for priority
    instructions.push(system_instruction::transfer(
        &user,
        &jito_tip_address,
        (config.jito_tip * LAMPORTS_PER_SOL as f64) as u64,
    ));

    // DRY RUN: show info only
    if config.dry_run {
        let blockhash = get_blockhash();
        let serialized = {
            let msg = Message::try_compile(&user, &instructions, &[], blockhash).unwrap();
            let tx = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[&config.keypair])
                .unwrap();
            bincode::serialize(&tx).unwrap()
        };
        log_info!("\n[TEST] === DRY RUN MODE ===");
        log_info!("   [+] Transaction created successfully!");
        log_info!("   [+] Instructions: {}", instructions.len());
        log_info!("   [+] SOL amount: {} SOL", config.sol_amount);
        log_info!("   [+] Jito tip: {} SOL", config.jito_tip);
        log_info!("   [+] TX size: {} bytes", serialized.len());
        log_info!("   [WARN] NOT SENDING - DRY_RUN mode");
        log_info!("   [TIP] Set DRY_RUN=false in .env to trade for real\n");
        return;
    }

    // === LIVE MODE: SPAM for maximum chance ===
    // Slide.fun pools open immediately (open_time = 0), so we fire aggressively
    log_info!(
        "[SNIPE] 🚀 FIRING TX - {} instructions, Jito tip: {} SOL",
        instructions.len(),
        config.jito_tip
    );

    // Strategy: Send via BOTH RPC and Jito simultaneously for max coverage
    let spam_count = 8;

    // Use cached blockhash from background updater (0ms, no RPC call).
    // The updater runs every 400ms using default commitment to match what RPC expects.
    let mut blockhash = get_blockhash();
    if blockhash == solana_sdk::hash::Hash::default() {
        // Fallback: if cache is empty (e.g. in test tools), fetch directly
        blockhash = rpc_client.get_latest_blockhash().await.unwrap_or_default();
    }

    let message = match Message::try_compile(&user, &instructions, &[], blockhash) {
        Ok(m) => m,
        Err(e) => {
            log_info!("[WARN] compile err: {}", e);
            return;
        }
    };
    let transaction =
        match VersionedTransaction::try_new(VersionedMessage::V0(message), &[&config.keypair]) {
            Ok(t) => t,
            Err(e) => {
                log_info!("[WARN] sign err: {}", e);
                return;
            }
        };

    let serialized = bincode::serialize(&transaction).unwrap();
    // base64 encoding is Jito's recommended format; base58 is deprecated
    let encoded = base64::engine::general_purpose::STANDARD.encode(&serialized);

    // Log the transaction size to monitor the 1232-byte limit
    log_info!(
        "[SNIPE] TX Size: {} bytes (Limit: 1232 bytes)",
        serialized.len()
    );

    for attempt in 1..=spam_count {
        // Send via RPC
        let rpc_clone = rpc_client.clone();
        let attempt_num = attempt;
        let tx_clone = transaction.clone();
        let trades_c = trades.clone();
        let mint_c = pool_info.base_mint.to_string();
        let sol_c = config.sol_amount;
        let network_c = config.network.clone();
        let wallet_c = config.keypair.pubkey().to_string();
        // Fill-first mode: always skip preflight for maximum speed.
        let skip_pf = true;
        tokio::spawn(async move {
            match rpc_clone
                .send_transaction_with_config(
                    &tx_clone,
                    RpcSendTransactionConfig {
                        skip_preflight: skip_pf,
                        max_retries: Some(0),
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(sig) => {
                    log_info!("[OK] RPC #{} sent: {}", attempt_num, sig);
                    let cluster_suffix = if network_c.to_lowercase() == "devnet" { "?cluster=devnet" } else { "" };
                    log_info!("   https://solscan.io/tx/{}{}", sig, cluster_suffix);

                    // Only record once — on first successful attempt (same sig for all retries)
                    if attempt_num == 1 {
                        let latency = event_time.elapsed().as_millis() as u64;
                        let t_store = trades_c.clone();
                        let t_sig = sig.to_string();
                        let t_mint = mint_c.clone();
                        let t_sol = sol_c;
                        let t_wallet = wallet_c.clone();
                        tokio::spawn(async move {
                            t_store.add_trade(Trade {
                                id: t_sig,
                                timestamp: chrono::Utc::now(),
                                mint: t_mint,
                                mode: "raydium".to_string(),
                                sol_amount: t_sol,
                                token_amount: 0.0,
                                status: "pending".to_string(),
                                latency_ms: Some(latency),
                                wallet: Some(t_wallet),
                                wallet_type: Some("main".to_string()),
                            }).await;
                        });
                    }
                }
                Err(e) => {
                    log_info!("[WARN] RPC #{} error: {}", attempt_num, e);
                }
            }
        });

        // Send via Jito (all 4 regional endpoints simultaneously)
        let encoded_clone = encoded.clone();
        let attempt_num2 = attempt;
        tokio::spawn(async move {
            match crate::transaction::send_via_jito(&[encoded_clone]).await {
                Ok(bundle_id) => {
                    log_info!("[OK] Jito #{} bundle: {}", attempt_num2, bundle_id);
                }
                Err(e) => {
                    log_info!("[WARN] Jito #{}: {}", attempt_num2, e);
                }
            }
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(35)).await;
    }

    log_info!(
        "[SNIPE] Done - fired {} attempts via RPC + Jito",
        spam_count
    );
}
