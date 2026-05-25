// ==================== MULTI-WALLET BUNDLE BUY ====================
//
// This module enables buying tokens simultaneously with multiple wallets
// by packing buy transactions into Jito bundles and firing them in parallel.
//
// Architecture:
//   1. Load N "sub-wallets" from wallets.json
//   2. For each sub-wallet: build a standalone buy TX (SOL → Token)
//   3. Pack TXs into Jito bundles (max 5 TXs per bundle)
//      - First TX of each bundle = Jito tip TX (from main wallet)
//      - Remaining 4 TXs = buy TXs from sub-wallets
//   4. Fire each bundle to exactly ONE Jito Block Engine endpoint (see JITO_BUNDLE_ENDPOINT_INDEX)
//
// wallets.json format:
//   [
//     {"private_key": "base58_private_key_1"},
//     {"private_key": "base58_private_key_2"},
//     ...
//   ]
//
// Jito bundle limits:
//   - Max 5 TXs per bundle
//   - First TX in each bundle pays the Jito tip
//   - SO: 1 tip TX + 4 buy TXs = 5 TXs per bundle
//   - For N wallets → ceil(N / 4) bundles

use crate::{constants, log_info};
use crate::config::Config;
use crate::trades::{Trade, TradesStore};
// use crate::transaction::send_bundle_to_url;
use crate::types::PoolInfo;
use base64::Engine;
use bincode;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    message::{v0::Message, VersionedMessage},
    native_token::LAMPORTS_PER_SOL,
    pubkey::Pubkey,
    signature::Signature,
    signer::{keypair::Keypair, Signer},
    system_instruction,
    transaction::VersionedTransaction,
};
use spl_associated_token_account::{
    get_associated_token_address,
    get_associated_token_address_with_program_id,
    instruction::create_associated_token_account_idempotent,
};
use std::str::FromStr;
use std::sync::Arc;

// ============================================
// Transaction builders
// ============================================

/// Build a "Jito tip" transaction from the main wallet.
/// This must be the FIRST TX in every bundle.
/// `bundle_index` is added as extra lamports to make each tip TX unique
/// (same keypair + blockhash + amount = identical signature → Jito drops duplicates).
fn build_tip_tx(
    main_keypair: &Keypair,
    jito_tip_lamports: u64,
    bundle_index: usize,
    blockhash: Hash,
) -> Option<VersionedTransaction> {
    let user = main_keypair.pubkey();
    let jito_tip_address = Pubkey::from_str(constants::jito_tip_address()).unwrap();

    // Add bundle_index lamports so each tip TX has a unique signature
    let tip = jito_tip_lamports + bundle_index as u64;

    let ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(3_000),
        system_instruction::transfer(&user, &jito_tip_address, tip),
    ];

    match Message::try_compile(&user, &ixs, &[], blockhash) {
        Ok(msg) => {
            match VersionedTransaction::try_new(VersionedMessage::V0(msg), &[main_keypair]) {
                Ok(tx) => Some(tx),
                Err(e) => { log_info!("[BUNDLE] Tip TX sign error: {}", e); None }
            }
        }
        Err(e) => { log_info!("[BUNDLE] Tip TX compile error: {}", e); None }
    }
}

/// Build a single **Raydium AMM V4 buy** transaction for a sub-wallet.
pub fn build_raydium_buy_tx_for_wallet(
    keypair: &Keypair,
    pool_info: &PoolInfo,
    sol_lamports: u64,
    min_token_out: u64,
    cu_limit: u32,
    priority_fee: u64,
    blockhash: Hash,
) -> Option<VersionedTransaction> {
    use crate::transaction::build_swap_instruction;

    let user = keypair.pubkey();
    let wsol_mint = Pubkey::from_str(constants::WSOL_MINT).unwrap();
    let wsol_token_program = Pubkey::from_str(constants::TOKEN_PROGRAM).unwrap();
    let base_token_program = pool_info.base_token_program;
    let user_wsol_ata = get_associated_token_address(&user, &wsol_mint);
    let user_token_ata = get_associated_token_address_with_program_id(
        &user,
        &pool_info.base_mint,
        &base_token_program,
    );

    let swap_ix = build_swap_instruction(
        pool_info,
        user,
        user_wsol_ata,
        user_token_ata,
        sol_lamports,
        min_token_out,
        base_token_program,
    );

    let ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
        ComputeBudgetInstruction::set_compute_unit_price(priority_fee),
        create_associated_token_account_idempotent(&user, &user, &wsol_mint, &wsol_token_program),
        system_instruction::transfer(&user, &user_wsol_ata, sol_lamports),
        Instruction {
            program_id: wsol_token_program,
            accounts: vec![AccountMeta::new(user_wsol_ata, false)],
            data: vec![17], // SyncNative
        },
        create_associated_token_account_idempotent(
            &user,
            &user,
            &pool_info.base_mint,
            &base_token_program,
        ),
        swap_ix,
    ];

    match Message::try_compile(&user, &ixs, &[], blockhash) {
        Ok(msg) => {
            match VersionedTransaction::try_new(VersionedMessage::V0(msg), &[keypair]) {
                Ok(tx) => {
                    let bytes = bincode::serialize(&tx).unwrap_or_default();
                    if bytes.len() > 1232 {
                        log_info!("[BUNDLE] Wallet {} TX too large: {} bytes", user, bytes.len());
                        return None;
                    }
                    Some(tx)
                }
                Err(e) => { log_info!("[BUNDLE] Wallet {} sign error: {}", user, e); None }
            }
        }
        Err(e) => { log_info!("[BUNDLE] Wallet {} compile error: {}", user, e); None }
    }
}

/// Build a single **Slide.fun bonding curve buy** transaction for a sub-wallet.
pub fn build_slidefun_buy_tx_for_wallet(
    keypair: &Keypair,
    token_mint: &Pubkey,
    token_program: &Pubkey,
    fee_to: &Pubkey,
    program_id: &Pubkey,
    sol_lamports: u64,
    cu_limit: u32,
    priority_fee: u64,
    blockhash: Hash,
) -> Option<VersionedTransaction> {
    use crate::slidefun_snipe::build_slidefun_buy_instruction;

    let user = keypair.pubkey();
    let wsol_mint = Pubkey::from_str(constants::WSOL_MINT).unwrap();
    let user_payment_ata = get_associated_token_address(&user, &wsol_mint);

    let buy_ix = build_slidefun_buy_instruction(
        &user,
        token_mint,
        &wsol_mint,
        fee_to,
        program_id,
        token_program,
        sol_lamports,
        0, // no min tokens out (max speed)
    );

    let ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
        ComputeBudgetInstruction::set_compute_unit_price(priority_fee),
        create_associated_token_account_idempotent(&user, &user, &wsol_mint, token_program),
        system_instruction::transfer(&user, &user_payment_ata, sol_lamports),
        Instruction {
            program_id: *token_program,
            accounts: vec![AccountMeta::new(user_payment_ata, false)],
            data: vec![17], // SyncNative
        },
        create_associated_token_account_idempotent(&user, &user, token_mint, token_program),
        buy_ix,
    ];

    match Message::try_compile(&user, &ixs, &[], blockhash) {
        Ok(msg) => {
            match VersionedTransaction::try_new(VersionedMessage::V0(msg), &[keypair]) {
                Ok(tx) => {
                    let bytes = bincode::serialize(&tx).unwrap_or_default();
                    if bytes.len() > 1232 {
                        log_info!("[BUNDLE] Wallet {} SF TX too large: {} bytes", user, bytes.len());
                        return None;
                    }
                    Some(tx)
                }
                Err(e) => { log_info!("[BUNDLE] Wallet {} SF sign error: {}", user, e); None }
            }
        }
        Err(e) => { log_info!("[BUNDLE] Wallet {} SF compile error: {}", user, e); None }
    }
}

// ============================================
// Bundle packing + sending
// ============================================

/// Encode a VersionedTransaction to base64 (Jito's recommended format; base58 is deprecated).
fn encode_tx(tx: &VersionedTransaction) -> Option<String> {
    let bytes = bincode::serialize(tx).ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(&bytes))
}

fn sig_short(sig: &Signature) -> String {
    let t = sig.to_string();
    if t.len() > 16 {
        format!("{}…", &t[..16])
    } else {
        t
    }
}

/// Log Solscan URLs for tip + each buy TX (same construction as `pack_into_bundles`).
fn log_bundle_solscan_links(
    buy_txs: &[VersionedTransaction],
    pending_wallet_indices: &[usize],
    main_keypair: &Keypair,
    jito_tip_lamports: u64,
    blockhash: Hash,
) {
    let n_bundles = (buy_txs.len() + 3) / 4;
    log_info!("[BUNDLE] TX preview (Solscan) — same sigs as in Jito bundle:");
    for chunk_i in 0..n_bundles {
        if let Some(tip) = build_tip_tx(main_keypair, jito_tip_lamports, chunk_i, blockhash) {
            if let Some(sig) = tip.signatures.first() {
                log_info!(
                    "[BUNDLE]   tip bundle[{}]: https://solscan.io/tx/{}",
                    chunk_i,
                    sig
                );
            }
        }
    }
    for (i, tx) in buy_txs.iter().enumerate() {
        if let Some(sig) = tx.signatures.first() {
            let wi = pending_wallet_indices.get(i).copied().unwrap_or(i);
            log_info!(
                "[BUNDLE]   buy wallet[{}]: https://solscan.io/tx/{}",
                wi,
                sig
            );
        }
    }
}

/// After submit: whether each signature landed and succeeded (`searchTransactionHistory = true`).
async fn log_signature_onchain_status(rpc: &RpcClient, label: &str, sig: &Signature) {
    match rpc
        .get_signature_status_with_commitment_and_history(sig, CommitmentConfig::confirmed(), true)
        .await
    {
        Ok(Some(Ok(()))) => log_info!(
            "[BUNDLE]   {} {} → ✅ confirmed",
            label,
            sig_short(sig)
        ),
        Ok(Some(Err(e))) => log_info!(
            "[BUNDLE]   {} {} → ❌ failed: {:?}",
            label,
            sig_short(sig),
            e
        ),
        Ok(None) => log_info!(
            "[BUNDLE]   {} {} → ⏳ not in ledger (dropped / expired / bundle not included)",
            label,
            sig_short(sig)
        ),
        Err(e) => log_info!("[BUNDLE]   {} {} → RPC error: {}", label, sig_short(sig), e),
    }
}

async fn log_bundle_onchain_after_wait(
    rpc: &RpcClient,
    buy_txs: &[VersionedTransaction],
    pending_wallet_indices: &[usize],
    main_keypair: &Keypair,
    jito_tip_lamports: u64,
    blockhash: Hash,
) {
    log_info!("[BUNDLE] On-chain status (getSignatureStatuses + history):");
    let n_bundles = (buy_txs.len() + 3) / 4;
    for chunk_i in 0..n_bundles {
        if let Some(tip) = build_tip_tx(main_keypair, jito_tip_lamports, chunk_i, blockhash) {
            if let Some(sig) = tip.signatures.first() {
                log_signature_onchain_status(rpc, &format!("tip bundle[{}]", chunk_i), sig).await;
            }
        }
    }
    for (i, tx) in buy_txs.iter().enumerate() {
        if let Some(sig) = tx.signatures.first() {
            let wi = pending_wallet_indices.get(i).copied().unwrap_or(i);
            log_signature_onchain_status(rpc, &format!("buy wallet[{}]", wi), sig).await;
        }
    }
}

/// Broadcast each bundle to **all** Jito Block Engine endpoints in parallel.
/// → If any endpoint is rate-limited, the others will still accept the bundle.
/// → First acceptance wins; subsequent acceptances are silently ignored.
async fn fire_bundles(bundles: Vec<Vec<String>>) {
    use crate::constants::jito_bundle_urls;

    for (i, bundle) in bundles.into_iter().enumerate() {
        let urls = jito_bundle_urls();
        let futs: Vec<_> = urls
            .iter()
            .map(|url| {
                let b = bundle.clone();
                let url_str = url.clone();
                async move {
                    let result = crate::transaction::send_bundle_to_url(&b, &url_str).await;
                    (url_str, result)
                }
            })
            .collect();

        let results = futures::future::join_all(futs).await;

        let mut accepted = false;
        let mut errors = Vec::new();

        for (region, result) in &results {
            match result {
                Ok(id) => {
                    if !accepted {
                        log_info!(
                            "[BUNDLE] Bundle[{}] → {} ✅ {}",
                            i,
                            region,
                            &id[..id.len().min(20)]
                        );
                        log_info!(
                            "[BUNDLE]   Jito bundle id: {} — https://explorer.jito.wtf/bundle-watch",
                            id
                        );
                        accepted = true;
                    }
                }
                Err(e) => {
                    // Suppress "already processed" logs if we already got an acceptance from another region
                    let err_str = format!("{:?}", e);
                    if !err_str.contains("already processed") {
                        errors.push((region, err_str));
                    }
                }
            }
        }

        if !accepted {
            log_info!("[BUNDLE] Bundle[{}] → ❌ ALL endpoints rejected/rate-limited", i);
        }
    }
}

// ============================================
// Public entry points
// ============================================

/// Helper: pack a list of buy TXs into Jito bundles (tip TX + up to 4 buy TXs each).
fn pack_into_bundles(
    buy_txs: &[VersionedTransaction],
    main_keypair: &solana_sdk::signer::keypair::Keypair,
    jito_tip_lamports: u64,
    blockhash: Hash,
) -> Vec<Vec<String>> {
    let mut bundles: Vec<Vec<String>> = Vec::new();
    for chunk in buy_txs.chunks(4) {
        let bundle_index = bundles.len();
        let tip_tx = match build_tip_tx(main_keypair, jito_tip_lamports, bundle_index, blockhash) {
            Some(tx) => tx,
            None => { log_info!("[BUNDLE] Could not build tip TX for bundle {}", bundle_index); continue; }
        };
        let mut bundle: Vec<String> = Vec::new();
        if let Some(encoded) = encode_tx(&tip_tx) { bundle.push(encoded); }
        for buy_tx in chunk {
            if let Some(encoded) = encode_tx(buy_tx) { bundle.push(encoded); }
        }
        log_info!("[BUNDLE]   Bundle[{}] → {} TXs", bundles.len(), bundle.len());
        bundles.push(bundle);
    }
    bundles
}

/// Fire a multi-wallet bundle buy on Raydium AMM V4.
///
/// Strategy:
///   1. Build all buy TXs and record initial wallet balances.
///   2. Fire all bundles (each routed to a dedicated Jito region).
///   3. Wait CONFIRM_WAIT_SECS for bundles to land.
///   4. Check which wallets actually bought (balance decreased ≥ sol/2).
///   5. Retry up to MAX_RETRIES times for wallets that didn't buy.
pub async fn raydium_bundle_buy(
    config: &Config,
    sub_wallets: &[(Keypair, f64)],
    pool_info: Arc<PoolInfo>,
    blockhash: Hash,
    trades: Arc<TradesStore>,
) {
    const MAX_RETRIES: usize = 2;
    const CONFIRM_WAIT_SECS: u64 = 6;

    let jito_tip_lamports = (config.jito_tip * LAMPORTS_PER_SOL as f64) as u64;

    // Combine Main Wallet + Sub Wallets for a UNIFIED bundle (prevents conflicts)
    let mut all_wallets = Vec::new();
    let mut seen_pubkeys = std::collections::HashSet::new();
    
    // Add Main Wallet as index 0 (if sol_amount > 0 AND not already provided)
    let main_in_sub = sub_wallets.iter().any(|(kp, _)| kp.pubkey() == config.keypair.pubkey());
    if !main_in_sub && config.sol_amount > 0.0 {
        all_wallets.push((&config.keypair, config.sol_amount, "main"));
        seen_pubkeys.insert(config.keypair.pubkey());
    }
    
    // Add Sub Wallets (with strict deduplication)
    for (kp, sol) in sub_wallets {
        if seen_pubkeys.insert(kp.pubkey()) {
            let role = if kp.pubkey() == config.keypair.pubkey() { "main" } else { "sub" };
            all_wallets.push((kp, *sol, role));
        } else {
            log_info!("[BUNDLE] Skipping duplicate wallet: {}", kp.pubkey());
        }
    }

    if all_wallets.is_empty() {
        log_info!("[BUNDLE] No wallets to buy with — aborting");
        return;
    }

    log_info!("[BUNDLE] 🚀 Unified Raydium bundle buy: {} wallet(s) (Main + {} Sub)",
        all_wallets.len(), sub_wallets.len());

    // RPC client for balance checks / fresh blockhash on retry
    let base_url = if config.network.to_lowercase() == "devnet" {
        "devnet.helius-rpc.com"
    } else {
        "mainnet.helius-rpc.com"
    };
    let rpc_url = format!("https://{}?api-key={}", base_url, config.helius_api_key);
    let rpc = Arc::new(RpcClient::new(rpc_url));

    // Track which wallets have confirmed their buy
    let mut wallet_bought: Vec<bool> = vec![false; all_wallets.len()];

    let mut current_bh = blockhash;
    for attempt in 0..=MAX_RETRIES {
        let pending: Vec<usize> = (0..all_wallets.len())
            .filter(|&i| !wallet_bought[i])
            .collect();

        if pending.is_empty() {
            log_info!("[BUNDLE] 🎉 All {} wallets confirmed bought!", all_wallets.len());
            break;
        }
        if attempt > 0 {
            log_info!("[BUNDLE] ⟳ Retry {}/{}: {} wallet(s) pending — fetching fresh blockhash...",
                attempt, MAX_RETRIES, pending.len());
            current_bh = match rpc.get_latest_blockhash().await {
                Ok(bh) => bh,
                Err(e) => { log_info!("[BUNDLE] ❌ Failed to get fresh blockhash: {}", e); break; }
            };
        }
        // Build buy TXs for pending wallets only
        let mut buy_txs: Vec<VersionedTransaction> = Vec::new();
        for &wi in &pending {
            let (keypair, sol_amount, _wtype) = all_wallets[wi];
            let sol_lamports = (sol_amount * LAMPORTS_PER_SOL as f64) as u64;
            match build_raydium_buy_tx_for_wallet(
                keypair, &pool_info, sol_lamports, 0,
                config.cu_limit + attempt as u32, config.priority_fee, current_bh,
            ) {
                Some(tx) => {
                    let bytes = bincode::serialize(&tx).unwrap_or_default();
                    log_info!("[BUNDLE]   Wallet[{}] {} → {} bytes ({:.2} SOL)",
                        wi, keypair.pubkey(), bytes.len(), sol_amount);
                    buy_txs.push(tx);
                }
                None => log_info!("[BUNDLE]   Wallet[{}] failed to build TX", wi),
            }
        }

        if buy_txs.is_empty() {
            log_info!("[BUNDLE] No valid buy TXs — aborting");
            break;
        }

        // --- TARGET NETWORK FALLBACK (Devnet: RPC direct, no Jito) ---
        if config.network.to_lowercase() == "devnet" {
            log_info!("[BUNDLE] Target Network: Devnet — sending individual TXs via RPC");
            // Use a shared set so spawned tasks can mark wallets as done immediately
            let sent_set: Arc<tokio::sync::Mutex<std::collections::HashSet<usize>>> =
                Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));

            let mut handles = Vec::new();
            for (idx, tx) in buy_txs.iter().enumerate() {
                let rpc_c = rpc.clone();
                let tx_c = tx.clone();
                let wi = pending[idx];
                let trades_c = trades.clone();
                let mint_c = pool_info.base_mint.to_string();
                let sol_c = all_wallets[wi].1;
                let wallet_c = all_wallets[wi].0.pubkey().to_string();
                let wtype_c = all_wallets[wi].2.to_string();
                let network_c = config.network.clone();
                let sent_c = sent_set.clone();
                let h = tokio::spawn(async move {
                    let config_rpc = solana_client::rpc_config::RpcSendTransactionConfig {
                        skip_preflight: true,
                        max_retries: Some(0),
                        ..Default::default()
                    };
                    match rpc_c.send_transaction_with_config(&tx_c, config_rpc).await {
                        Ok(sig) => {
                            let cluster = if network_c.to_lowercase() == "devnet" { "?cluster=devnet" } else { "" };
                            log_info!("[BUNDLE] Devnet TX OK: {}{}", sig, cluster);
                            log_info!("   https://solscan.io/tx/{}{}", sig, cluster);
                            // Mark this wallet as successfully sent — no retry needed
                            sent_c.lock().await.insert(wi);
                            // Record trade only once (on first successful send)
                            trades_c.add_trade(Trade {
                                id: sig.to_string(),
                                timestamp: chrono::Utc::now(),
                                mint: mint_c,
                                mode: "raydium".to_string(),
                                sol_amount: sol_c,
                                token_amount: 0.0,
                                status: "pending".to_string(),
                                latency_ms: None,
                                wallet: Some(wallet_c),
                                wallet_type: Some(wtype_c),
                            }).await;
                        }
                        Err(e) => log_info!("[BUNDLE] Devnet TX Error: {:?}", e),
                    }
                });
                handles.push(h);
            }
            log_info!("[BUNDLE] Waiting {}s for transactions to land...", CONFIRM_WAIT_SECS);
            // Wait for all spawned sends to complete
            for h in handles { let _ = h.await; }
            tokio::time::sleep(tokio::time::Duration::from_secs(CONFIRM_WAIT_SECS)).await;

            // Mark as bought for any wallet whose send succeeded
            for &wi in sent_set.lock().await.iter() {
                wallet_bought[wi] = true;
                log_info!("[BUNDLE] ✅ Wallet[{}] sent OK — marking done", wi);
            }
            continue;
        }


        let adjusted_tip = jito_tip_lamports + (attempt as u64 * 10);
        let bundles = pack_into_bundles(&buy_txs, &config.keypair, adjusted_tip, current_bh);
        log_bundle_solscan_links(
            &buy_txs,
            &pending,
            &config.keypair,
            adjusted_tip,
            current_bh,
        );
        log_info!("[BUNDLE] Firing {} bundle(s) (attempt {}/{})...",
            bundles.len(), attempt + 1, MAX_RETRIES + 1);
        fire_bundles(bundles).await;

        // Wait for bundles to land on-chain before checking balances
        log_info!("[BUNDLE] Waiting {}s for bundles to land...", CONFIRM_WAIT_SECS);
        tokio::time::sleep(tokio::time::Duration::from_secs(CONFIRM_WAIT_SECS)).await;

        log_bundle_onchain_after_wait(
            &rpc,
            &buy_txs,
            &pending,
            &config.keypair,
            jito_tip_lamports + (attempt as u64 * 10),
            current_bh,
        )
        .await;

        // Update bought status based on signature status
        for (idx, tx) in buy_txs.iter().enumerate() {
            if let Some(sig) = tx.signatures.first() {
                let wi = pending[idx];
                match rpc.get_signature_status(sig).await {
                    Ok(Some(Ok(()))) => {
                        wallet_bought[wi] = true;
                        log_info!("[BUNDLE] ✅ Wallet[{}] confirmed bought (sig: {})", wi, sig_short(sig));
                    }
                    _ => {}
                }
            }
        }

        if attempt == MAX_RETRIES {
            let unbought: Vec<usize> = (0..all_wallets.len()).filter(|&i| !wallet_bought[i]).collect();
            if !unbought.is_empty() {
                log_info!("[BUNDLE] ⚠️  {} wallet(s) still unconfirmed after {} attempts: {:?}",
                    unbought.len(), MAX_RETRIES + 1, unbought);
            } else {
                log_info!("[BUNDLE] 🎉 All {} wallets confirmed!", all_wallets.len());
            }
        }
    }

    log_info!("[BUNDLE] ✅ Done");
}

// ============================================
// Manual Bundle Selling
// ============================================

/// Build a single **Raydium AMM V4 sell** transaction for a sub-wallet.
/// Sells `percent` (0.0 - 100.0) of the current token balance.
pub async fn build_raydium_sell_tx_for_wallet(
    rpc: &RpcClient,
    keypair: &Keypair,
    pool_info: &PoolInfo,
    percent: f64,
    blockhash: Hash,
) -> Option<VersionedTransaction> {
    use crate::transaction::build_swap_instruction;

    let user = keypair.pubkey();
    let token_mint = pool_info.base_mint;
    let base_token_program = pool_info.base_token_program;
    let wsol_mint = Pubkey::from_str(constants::WSOL_MINT).unwrap();
    let token_program = Pubkey::from_str(constants::TOKEN_PROGRAM).unwrap();

    let user_token_ata = get_associated_token_address_with_program_id(
        &user,
        &token_mint,
        &base_token_program,
    );
    let user_wsol_ata = get_associated_token_address(&user, &wsol_mint);

    // Fetch token balance
    let balance_resp = rpc.get_token_account_balance(&user_token_ata).await.ok()?;
    let amount_u64 = balance_resp.amount.parse::<u64>().ok()?;
    if amount_u64 == 0 {
        return None;
    }

    let sell_amount = if percent >= 100.0 {
        amount_u64
    } else {
        (amount_u64 as f64 * (percent / 100.0)) as u64
    };

    if sell_amount == 0 {
        return None;
    }

    let swap_ix = build_swap_instruction(
        pool_info,
        user,
        user_token_ata,  // src = Token
        user_wsol_ata,   // dst = WSOL
        sell_amount,
        0,               // min_sol_out = 0 (manual sell)
        base_token_program,
    );

    let ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(200_000),
        ComputeBudgetInstruction::set_compute_unit_price(100_000),
        create_associated_token_account_idempotent(&user, &user, &wsol_mint, &token_program),
        swap_ix,
        // Close WSOL account to get SOL back
        Instruction {
            program_id: token_program,
            accounts: vec![
                AccountMeta::new(user_wsol_ata, false),
                AccountMeta::new(user, false),
                AccountMeta::new_readonly(user, true),
            ],
            data: vec![9], // CloseAccount
        },
    ];

    match Message::try_compile(&user, &ixs, &[], blockhash) {
        Ok(msg) => {
            match VersionedTransaction::try_new(VersionedMessage::V0(msg), &[keypair]) {
                Ok(tx) => Some(tx),
                Err(e) => { log_info!("[BUNDLE] Wallet {} sell sign error: {}", user, e); None }
            }
        }
        Err(e) => { log_info!("[BUNDLE] Wallet {} sell compile error: {}", user, e); None }
    }
}

/// Execute a multi-wallet bundle sell on Raydium AMM V4.
pub async fn raydium_bundle_sell(
    config: &Config,
    wallets: &[(Keypair, f64)],
    pool_info: Arc<PoolInfo>,
    percent: f64,
    blockhash: Hash,
) {
    if wallets.is_empty() {
        log_info!("[BUNDLE] No wallets for bundle sell");
        return;
    }

    log_info!("[BUNDLE] 🚀 Multi-wallet SELL: {} wallets, {:.1}% of tokens", wallets.len(), percent);

    let base_url = if config.network.to_lowercase() == "devnet" {
        "devnet.helius-rpc.com"
    } else {
        "mainnet.helius-rpc.com"
    };
    let rpc_url = format!("https://{}?api-key={}", base_url, config.helius_api_key);
    let rpc = Arc::new(RpcClient::new(rpc_url));

    let mut sell_txs = Vec::new();
    for (i, (wallet, _sol)) in wallets.iter().enumerate() {
        if let Some(tx) = build_raydium_sell_tx_for_wallet(&rpc, wallet, &pool_info, percent, blockhash).await {
            log_info!("[BUNDLE]   Wallet[{}] {} → built sell TX", i, wallet.pubkey());
            sell_txs.push(tx);
        } else {
            log_info!("[BUNDLE]   Wallet[{}] {} → skipped (no balance or error)", i, wallet.pubkey());
        }
    }

    if sell_txs.is_empty() {
        log_info!("[BUNDLE] No valid sell transactions built — aborting");
        return;
    }

    if config.network.to_lowercase() == "devnet" {
        for tx in sell_txs.into_iter() {
            match rpc.send_transaction_with_config(&tx, solana_client::rpc_config::RpcSendTransactionConfig {
                skip_preflight: true,
                max_retries: Some(0),
                ..Default::default()
            }).await {
                Ok(sig) => log_info!("[BUNDLE] Devnet Raydium SELL OK: {}", sig),
                Err(e) => log_info!("[BUNDLE] Devnet Raydium SELL Error: {:?}", e),
            }
        }
        log_info!("[BUNDLE] ✅ Done");
        return;
    }

    let jito_tip_lamports = (config.jito_tip * LAMPORTS_PER_SOL as f64) as u64;
    let bundles = pack_into_bundles(&sell_txs, &config.keypair, jito_tip_lamports, blockhash);

    log_info!("[BUNDLE] Firing {} sell bundle(s)...", bundles.len());
    fire_bundles(bundles).await;
    log_info!("[BUNDLE] Manual bundle sell sent");
}

/// Fire a multi-wallet bundle buy on Slide.fun Bonding Curve.
/// Same retry strategy as `raydium_bundle_buy`.
pub async fn slidefun_bundle_buy(
    config: &Config,
    sub_wallets: &[(Keypair, f64)],
    token_mint: &str,
    token_program: Pubkey,
    fee_to: &Pubkey,
    blockhash: Hash,
    trades: Arc<TradesStore>,
) {
    let token_mint_pk = match Pubkey::from_str(token_mint) {
        Ok(pk) => pk,
        Err(e) => { log_info!("[BUNDLE] Invalid token mint: {}", e); return; }
    };

    const MAX_RETRIES: usize = 2;
    const CONFIRM_WAIT_SECS: u64 = 6;

    let jito_tip_lamports = (config.jito_tip * LAMPORTS_PER_SOL as f64) as u64;

    // Combine Main Wallet + Sub Wallets for a UNIFIED bundle
    let mut all_wallets = Vec::new();
    let mut seen_pubkeys = std::collections::HashSet::new();
    let main_in_sub = sub_wallets.iter().any(|(kp, _)| kp.pubkey() == config.keypair.pubkey());
    if !main_in_sub && config.slidefun_pump_amount > 0.0 {
        all_wallets.push((&config.keypair, config.slidefun_pump_amount, "main"));
        seen_pubkeys.insert(config.keypair.pubkey());
    }
    for (kp, sol) in sub_wallets {
        if seen_pubkeys.insert(kp.pubkey()) {
            let role = if kp.pubkey() == config.keypair.pubkey() { "main" } else { "sub" };
            all_wallets.push((kp, *sol, role));
        } else {
            log_info!("[BUNDLE] Skipping duplicate wallet: {}", kp.pubkey());
        }
    }

    if all_wallets.is_empty() {
        log_info!("[BUNDLE] No wallets to buy with — aborting");
        return;
    }

    log_info!("[BUNDLE] 🚀 Unified Slide.fun bundle buy: {} wallet(s) (Main + {} Sub)",
        all_wallets.len(), sub_wallets.len());

    let base_url = if config.network.to_lowercase() == "devnet" {
        "devnet.helius-rpc.com"
    } else {
        "mainnet.helius-rpc.com"
    };
    let rpc_url = format!("https://{}?api-key={}", base_url, config.helius_api_key);
    let rpc = Arc::new(RpcClient::new(rpc_url));

    // Track which wallets have confirmed their buy
    let mut wallet_bought: Vec<bool> = vec![false; all_wallets.len()];
    let mut current_bh = blockhash;

    for attempt in 0..=MAX_RETRIES {
        let pending: Vec<usize> = (0..all_wallets.len()).filter(|&i| !wallet_bought[i]).collect();
        if pending.is_empty() {
            log_info!("[BUNDLE] 🎉 All {} wallets confirmed bought!", all_wallets.len());
            break;
        }

        if attempt > 0 {
            log_info!("[BUNDLE] ⟳ Retry {}/{}: {} wallet(s) pending...",
                attempt, MAX_RETRIES, pending.len());
            current_bh = match rpc.get_latest_blockhash().await {
                Ok(bh) => bh,
                Err(e) => { log_info!("[BUNDLE] ❌ Fresh blockhash failed: {}", e); break; }
            };
        }

        let mut buy_txs: Vec<VersionedTransaction> = Vec::new();
        for &wi in &pending {
            let (keypair, sol_amount, _wtype) = all_wallets[wi];
            let sol_lamports = (sol_amount * LAMPORTS_PER_SOL as f64) as u64;
            let slidefun_program = config.slidefun_program();
            match build_slidefun_buy_tx_for_wallet(
                keypair, &token_mint_pk, &token_program, fee_to, &slidefun_program, sol_lamports,
                config.cu_limit + attempt as u32, config.priority_fee, current_bh,
            ) {
                Some(tx) => {
                    let bytes = bincode::serialize(&tx).unwrap_or_default();
                    log_info!("[BUNDLE]   Wallet[{}] {} → {} bytes ({:.2} SOL)",
                        wi, keypair.pubkey(), bytes.len(), sol_amount);
                    buy_txs.push(tx);
                }
                None => log_info!("[BUNDLE]   Wallet[{}] failed to build TX", wi),
            }
        }

        if buy_txs.is_empty() { break; }
        
        // --- TARGET NETWORK FALLBACK (Devnet: RPC direct, no Jito) ---
        if config.network.to_lowercase() == "devnet" {
            log_info!("[BUNDLE] Target Network: Devnet — sending individual TXs via RPC");
            let sent_set: Arc<tokio::sync::Mutex<std::collections::HashSet<usize>>> =
                Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));

            let mut handles = Vec::new();
            for (idx, tx) in buy_txs.iter().enumerate() {
                let rpc_c = rpc.clone();
                let tx_c = tx.clone();
                let wi = pending[idx];
                let trades_c = trades.clone();
                let mint_c = token_mint.to_string();
                let sol_c = all_wallets[wi].1;
                let wallet_c = all_wallets[wi].0.pubkey().to_string();
                let wtype_c = all_wallets[wi].2.to_string();
                let network_c = config.network.clone();
                let sent_c = sent_set.clone();
                let h = tokio::spawn(async move {
                    let config_rpc = solana_client::rpc_config::RpcSendTransactionConfig {
                        skip_preflight: true,
                        max_retries: Some(0),
                        ..Default::default()
                    };
                    match rpc_c.send_transaction_with_config(&tx_c, config_rpc).await {
                        Ok(sig) => {
                            let cluster = if network_c.to_lowercase() == "devnet" { "?cluster=devnet" } else { "" };
                            log_info!("[BUNDLE] Devnet SF TX OK: {}{}", sig, cluster);
                            log_info!("   https://solscan.io/tx/{}{}", sig, cluster);
                            // Mark wallet done — no retry
                            sent_c.lock().await.insert(wi);
                            // Record trade only once
                            trades_c.add_trade(Trade {
                                id: sig.to_string(),
                                timestamp: chrono::Utc::now(),
                                mint: mint_c,
                                mode: "slidefun".to_string(),
                                sol_amount: sol_c,
                                token_amount: 0.0,
                                status: "pending".to_string(),
                                latency_ms: None,
                                wallet: Some(wallet_c),
                                wallet_type: Some(wtype_c),
                            }).await;
                        }
                        Err(e) => log_info!("[BUNDLE] Devnet SF TX Error: {:?}", e),
                    }
                });
                handles.push(h);
            }
            log_info!("[BUNDLE] Waiting {}s for transactions to land...", CONFIRM_WAIT_SECS);
            for h in handles { let _ = h.await; }
            tokio::time::sleep(tokio::time::Duration::from_secs(CONFIRM_WAIT_SECS)).await;

            for &wi in sent_set.lock().await.iter() {
                wallet_bought[wi] = true;
                log_info!("[BUNDLE] ✅ Wallet[{}] SF sent OK — marking done", wi);
            }
            continue;
        }


        let adjusted_tip = jito_tip_lamports + (attempt as u64 * 10);
        let bundles = pack_into_bundles(&buy_txs, &config.keypair, adjusted_tip, current_bh);
        log_bundle_solscan_links(
            &buy_txs,
            &pending,
            &config.keypair,
            adjusted_tip,
            current_bh,
        );
        log_info!("[BUNDLE] Firing {} bundle(s) (attempt {}/{})...",
            bundles.len(), attempt + 1, MAX_RETRIES + 1);
        fire_bundles(bundles).await;

        log_info!("[BUNDLE] Waiting {}s for bundles to land...", CONFIRM_WAIT_SECS);
        tokio::time::sleep(tokio::time::Duration::from_secs(CONFIRM_WAIT_SECS)).await;

        log_bundle_onchain_after_wait(
            &rpc,
            &buy_txs,
            &pending,
            &config.keypair,
            jito_tip_lamports + (attempt as u64 * 10),
            current_bh,
        )
        .await;

        // Update status via signatures
        for (idx, tx) in buy_txs.iter().enumerate() {
            if let Some(sig) = tx.signatures.first() {
                let wi = pending[idx];
                match rpc.get_signature_status(sig).await {
                    Ok(Some(Ok(()))) => {
                        wallet_bought[wi] = true;
                        log_info!("[BUNDLE] ✅ Wallet[{}] confirmed bought (sig: {})", wi, sig_short(sig));
                    }
                    _ => {}
                }
            }
        }

        if attempt == MAX_RETRIES {
            let unbought: Vec<usize> = (0..all_wallets.len()).filter(|&i| !wallet_bought[i]).collect();
            if !unbought.is_empty() {
                log_info!("[BUNDLE] ⚠️  {} wallet(s) unconfirmed after {} attempts: {:?}",
                    unbought.len(), MAX_RETRIES + 1, unbought);
            }
        }
    }

    log_info!("[BUNDLE] ✅ Done");
}

/// Build a single **Slide.fun bonding curve sell** transaction for a sub-wallet.
pub async fn build_slidefun_sell_tx_for_wallet(
    rpc: &RpcClient,
    keypair: &Keypair,
    token_mint: &Pubkey,
    token_program: &Pubkey,
    fee_to: &Pubkey,
    program_id: &Pubkey,
    percent: f64,
    cu_limit: u32,
    priority_fee: u64,
    blockhash: Hash,
) -> Option<VersionedTransaction> {
    use crate::slidefun_snipe::build_slidefun_sell_instruction;

    let user = keypair.pubkey();
    let wsol_mint = Pubkey::from_str(constants::WSOL_MINT).unwrap();
    let user_token_ata = get_associated_token_address_with_program_id(&user, token_mint, token_program);

    // Fetch token balance
    let balance_resp = rpc.get_token_account_balance(&user_token_ata).await.ok()?;
    let amount_u64 = balance_resp.amount.parse::<u64>().ok()?;
    if amount_u64 == 0 {
        return None;
    }

    let sell_amount = if percent >= 100.0 {
        amount_u64
    } else {
        (amount_u64 as f64 * (percent / 100.0)) as u64
    };

    if sell_amount == 0 {
        return None;
    }

    let sell_ix = build_slidefun_sell_instruction(
        &user,
        token_mint,
        &wsol_mint,
        fee_to,
        program_id,
        token_program,
        sell_amount,
        0, // no min SOL out (max speed)
    );

    let ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
        ComputeBudgetInstruction::set_compute_unit_price(priority_fee),
        sell_ix,
    ];

    match Message::try_compile(&user, &ixs, &[], blockhash) {
        Ok(msg) => {
            match VersionedTransaction::try_new(VersionedMessage::V0(msg), &[keypair]) {
                Ok(tx) => Some(tx),
                Err(e) => { log_info!("[BUNDLE] Wallet {} SF sell sign error: {}", user, e); None }
            }
        }
        Err(e) => { log_info!("[BUNDLE] Wallet {} SF sell compile error: {}", user, e); None }
    }
}

/// Fire a multi-wallet bundle sell on Slide.fun Bonding Curve.
pub async fn slidefun_bundle_sell(
    config: &Config,
    wallets: &[(Keypair, f64)],
    token_mint: &str,
    token_program: Pubkey,
    fee_to: &Pubkey,
    percent: f64,
    blockhash: Hash,
) {
    if wallets.is_empty() {
        log_info!("[BUNDLE] No bundle wallets loaded — skipping bundle sell");
        return;
    }

    let token_mint_pk = match Pubkey::from_str(token_mint) {
        Ok(pk) => pk,
        Err(e) => { log_info!("[BUNDLE] Invalid token mint: {}", e); return; }
    };

    let rpc = Arc::new(RpcClient::new(config.rpc_url()));
    let jito_tip_lamports = (config.jito_tip * LAMPORTS_PER_SOL as f64) as u64;
    let slidefun_program = config.slidefun_program();

    log_info!("[BUNDLE] 🚀 Multi-wallet Slide.fun SELL: {} wallets, {:.1}% of tokens", wallets.len(), percent);

    let mut sell_txs: Vec<VersionedTransaction> = Vec::new();
    let mut wallet_indices = Vec::new();

    for (wi, (kp, _)) in wallets.iter().enumerate() {
        if let Some(tx) = build_slidefun_sell_tx_for_wallet(
            &rpc,
            kp,
            &token_mint_pk,
            &token_program,
            fee_to,
            &slidefun_program,
            percent,
            config.cu_limit,
            config.priority_fee,
            blockhash,
        ).await {
            sell_txs.push(tx);
            wallet_indices.push(wi);
        }
    }

    if sell_txs.is_empty() {
        log_info!("[BUNDLE] ⚠️  No wallets have tokens to sell.");
        return;
    }

    if config.network.to_lowercase() == "devnet" {
        for tx in sell_txs.into_iter() {
            match rpc.send_transaction_with_config(&tx, solana_client::rpc_config::RpcSendTransactionConfig {
                skip_preflight: true,
                max_retries: Some(0),
                ..Default::default()
            }).await {
                Ok(sig) => log_info!("[BUNDLE] Devnet SF SELL OK: {}", sig),
                Err(e) => log_info!("[BUNDLE] Devnet SF SELL Error: {:?}", e),
            }
        }
        log_info!("[BUNDLE] ✅ Done");
        return;
    }

    let bundles = pack_into_bundles(&sell_txs, &config.keypair, jito_tip_lamports, blockhash);
    
    log_info!("[BUNDLE] Firing {} sell bundle(s)...", bundles.len());
    fire_bundles(bundles).await;
    log_info!("[BUNDLE] ✅ Sell bundles fired.");
}

/// Check balances of all enabled sub-wallets at startup.
pub async fn check_sub_wallet_balances(config: &Config, wallets: &[(Keypair, f64)]) {
    if wallets.is_empty() { return; }
    
    let base_url = if config.network.to_lowercase() == "devnet" {
        "devnet.helius-rpc.com"
    } else {
        "mainnet.helius-rpc.com"
    };
    let rpc_url = format!("https://{}?api-key={}", base_url, config.helius_api_key);
    let rpc = RpcClient::new(rpc_url);
    
    log_info!("[BUNDLE] Checking sub-wallet balances...");
    let pks: Vec<Pubkey> = wallets.iter().map(|(kp, _)| kp.pubkey()).collect();
    let futs: Vec<_> = pks.iter().map(|pk| rpc.get_balance(pk)).collect();
    let results = futures::future::join_all(futs).await;
    
    for (i, res) in results.into_iter().enumerate() {
        let pk = wallets[i].0.pubkey();
        match res {
            Ok(bal) => {
                log_info!("[BUNDLE]   Wallet[{}] {}: {:.4} SOL", i, pk, bal as f64 / 1e9);
                if bal < 10_000_000 {
                    log_info!("[WARN]   Wallet[{}] is nearly empty!", i);
                }
            }
            Err(e) => log_info!("[BUNDLE]   Wallet[{}] {}: Failed to fetch balance: {}", i, pk, e),
        }
    }
}
