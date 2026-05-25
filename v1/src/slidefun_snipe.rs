// ==================== SLIDE-FUN CREATION SNIPE ====================
//
// This module listens to the Slide.fun Meme Program for `create_bonding_curve`
// instructions. When a new token is created, we immediately send a `buy`
// instruction to the same program to purchase tokens on the bonding curve.
//
// Flow:
//   1. Listen for `create_bonding_curve` logs from Slide.fun program
//   2. Parse the transaction to extract:
//      - token_mint (the new token)
//      - payment mint (usually WSOL / native SOL)
//   3. Derive PDA accounts (config, bonding_curve, vaults)
//   4. Build + send a `buy` instruction via the Slide.fun program
//
// Slide.fun `buy` instruction (from IDL):
//   Discriminator: [102, 6, 61, 18, 1, 218, 235, 234]
//   Accounts in order:
//     [0]  user                  (writable, signer)
//     [1]  config                (PDA: seeds = b"config")
//     [2]  bonding_curve         (writable, PDA: seeds = [b"bonding_curve", token])
//     [3]  token                 (the token mint)
//     [4]  payment               (WSOL mint)
//     [5]  bonding_curve_token_ata (writable, PDA: ATA of bonding_curve for token)
//     [6]  bonding_curve_payment_ata (writable, PDA: ATA of bonding_curve for payment)
//     [7]  user_token_ata        (writable, PDA: ATA of user for token)
//     [8]  user_payment_ata      (writable, PDA: ATA of user for payment/WSOL)
//     [9]  fee_to_payment_ata    (writable, PDA: ATA of config.fee_to for payment)
//     [10] associated_token_program
//     [11] system_program
//     [12] token_program
//   Args: amount (u64), is_exact_in (bool), input_number (u64)

use crate::config::Config;
use crate::transaction::send_via_jito;
use crate::trades::{Trade, TradesStore};
use crate::{constants, log_info};
use base64::Engine;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::{RpcSendTransactionConfig, RpcTransactionConfig};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction},
    message::{v0::Message, VersionedMessage},
    native_token::LAMPORTS_PER_SOL,
    pubkey::Pubkey,
    signature::{Signature, Signer},
    system_instruction,
    transaction::VersionedTransaction,
};
use solana_transaction_status_client_types::UiTransactionEncoding;
use spl_associated_token_account::{
    get_associated_token_address, instruction::create_associated_token_account_idempotent,
};
use std::str::FromStr;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

// ==============================================================
// PDA derivation helpers
// ==============================================================

/// Derive config PDA: seeds = [b"config"]
pub fn derive_config_pda(program_id: &Pubkey) -> Pubkey {
    let (pda, _) = Pubkey::find_program_address(&[constants::SLIDEFUN_CONFIG_SEED], program_id);
    pda
}

/// Derive bonding_curve PDA: seeds = [b"bonding_curve", token_mint]
pub fn derive_bonding_curve_pda(token_mint: &Pubkey, program_id: &Pubkey) -> Pubkey {
    let (pda, _) = Pubkey::find_program_address(
        &[constants::SLIDEFUN_BONDING_CURVE_SEED, token_mint.as_ref()],
        program_id,
    );
    pda
}

// ==============================================================
// Buy instruction builder
// ==============================================================

/// Build the Slide.fun `buy` instruction.
/// - sol_amount: how many SOL to spend (fractional, e.g. 0.05)
/// - min_token_out: minimum tokens to receive (slippage protection, 0 = no min)
pub fn build_slidefun_buy_instruction(
    user: &Pubkey,
    token_mint: &Pubkey,
    payment_mint: &Pubkey, // usually WSOL / So1111...
    fee_to: &Pubkey,       // from config account on-chain, fetched separately
    program_id: &Pubkey,
    token_program: &Pubkey, // Token or Token-2022
    sol_amount_lamports: u64,
    min_token_out: u64,
) -> Instruction {
    let slidefun_program = program_id;
    let assoc_token_program =
        Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap();
    let system_program = solana_sdk::system_program::ID;

    let config_pda = derive_config_pda(slidefun_program);
    let bonding_curve_pda = derive_bonding_curve_pda(token_mint, slidefun_program);

    // ATAs
    // Use spl_associated_token_account::get_associated_token_address_with_program_id to support Token-2022
    let bonding_curve_token_ata =
        spl_associated_token_account::get_associated_token_address_with_program_id(
            &bonding_curve_pda,
            token_mint,
            token_program,
        );
    let bonding_curve_payment_ata = get_associated_token_address(&bonding_curve_pda, payment_mint); // WSOL is always standard Token
    let user_token_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
        user,
        token_mint,
        token_program,
    );
    let user_payment_ata = get_associated_token_address(user, payment_mint);
    let fee_to_payment_ata = get_associated_token_address(fee_to, payment_mint);

    let accounts = vec![
        AccountMeta::new(*user, true),                         // [0] user
        AccountMeta::new_readonly(config_pda, false),          // [1] config
        AccountMeta::new(bonding_curve_pda, false),            // [2] bonding_curve
        AccountMeta::new_readonly(*token_mint, false),         // [3] token (mint)
        AccountMeta::new_readonly(*payment_mint, false),       // [4] payment (WSOL)
        AccountMeta::new(bonding_curve_token_ata, false),      // [5] bonding_curve_token_ata
        AccountMeta::new(bonding_curve_payment_ata, false),    // [6] bonding_curve_payment_ata
        AccountMeta::new(user_token_ata, false),               // [7] user_token_ata
        AccountMeta::new(user_payment_ata, false),             // [8] user_payment_ata
        AccountMeta::new(fee_to_payment_ata, false),           // [9] fee_to_payment_ata
        AccountMeta::new_readonly(assoc_token_program, false), // [10] associated_token_program
        AccountMeta::new_readonly(system_program, false),      // [11] system_program
        AccountMeta::new_readonly(*token_program, false),      // [12] token_program
    ];

    // Instruction data: 8-byte discriminator + amount (u64) + is_exact_in (bool=true) + input_number (u64=0)
    let mut data = constants::SLIDEFUN_BUY_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&sol_amount_lamports.to_le_bytes()); // amount
    data.push(1u8); // is_exact_in = true (we specify exact SOL in, get min tokens out)
    data.extend_from_slice(&min_token_out.to_le_bytes()); // input_number (min tokens out)

    Instruction {
        program_id: *slidefun_program,
        accounts,
        data,
    }
}

pub fn build_slidefun_sell_instruction(
    user: &Pubkey,
    token_mint: &Pubkey,
    payment_mint: &Pubkey,
    fee_to: &Pubkey,
    program_id: &Pubkey,
    token_program: &Pubkey,
    token_amount: u64,
    min_sol_out: u64,
) -> Instruction {
    let slidefun_program = program_id;
    let assoc_token_program =
        Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap();
    let system_program = solana_sdk::system_program::ID;

    let config_pda = derive_config_pda(slidefun_program);
    let bonding_curve_pda = derive_bonding_curve_pda(token_mint, slidefun_program);

    let bonding_curve_token_ata =
        spl_associated_token_account::get_associated_token_address_with_program_id(
            &bonding_curve_pda,
            token_mint,
            token_program,
        );
    let bonding_curve_payment_ata = get_associated_token_address(&bonding_curve_pda, payment_mint);
    let user_token_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
        user,
        token_mint,
        token_program,
    );
    let user_payment_ata = get_associated_token_address(user, payment_mint);
    let fee_to_payment_ata = get_associated_token_address(fee_to, payment_mint);

    let accounts = vec![
        AccountMeta::new(*user, true),
        AccountMeta::new_readonly(config_pda, false),
        AccountMeta::new(bonding_curve_pda, false),
        AccountMeta::new_readonly(*token_mint, false),
        AccountMeta::new_readonly(*payment_mint, false),
        AccountMeta::new(bonding_curve_token_ata, false),
        AccountMeta::new(bonding_curve_payment_ata, false),
        AccountMeta::new(user_token_ata, false),
        AccountMeta::new(user_payment_ata, false),
        AccountMeta::new(fee_to_payment_ata, false),
        AccountMeta::new_readonly(assoc_token_program, false),
        AccountMeta::new_readonly(system_program, false),
        AccountMeta::new_readonly(*token_program, false),
    ];

    let mut data = constants::SLIDEFUN_SELL_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&token_amount.to_le_bytes());
    data.push(1u8); // is_exact_in = true
    data.extend_from_slice(&min_sol_out.to_le_bytes());

    Instruction {
        program_id: *slidefun_program,
        accounts,
        data,
    }
}

use std::sync::Mutex;

lazy_static::lazy_static! {
    static ref CACHED_FEE_TO: Mutex<Option<Pubkey>> = Mutex::new(None);
}

/// Clear the cached fee_to address (used when Program ID changes).
pub fn clear_fee_to_cache() {
    let mut cache = CACHED_FEE_TO.lock().unwrap();
    *cache = None;
}

/// Pre-fetch the fee_to address at startup to eliminate delay during trade.
pub async fn pre_fetch_fee_to(rpc_client: &RpcClient, program_id: &Pubkey) {
    if let Some(fee_to) = fetch_fee_to(rpc_client, program_id).await {
        let mut cache = CACHED_FEE_TO.lock().unwrap();
        *cache = Some(fee_to);
        log_info!("[SFSNIPE] Fee-to address cached: {}", fee_to);
    }
}

/// Get the cached fee_to address instantly.
pub fn get_cached_fee_to() -> Option<Pubkey> {
    let cache = CACHED_FEE_TO.lock().unwrap();
    *cache
}

/// Fetch the `fee_to` pubkey from the Slide.fun Config PDA account.
pub async fn fetch_fee_to(rpc_client: &RpcClient, program_id: &Pubkey) -> Option<Pubkey> {
    let config_pda = derive_config_pda(program_id);
    match rpc_client.get_account(&config_pda).await {
        Ok(account) => {
            let data = &account.data;
            if data.len() < 112 {
                log_info!("[SFSNIPE] Config account too short: {} bytes", data.len());
                return None;
            }
            let fee_to_bytes: [u8; 32] = data[80..112].try_into().ok()?;
            Some(Pubkey::from(fee_to_bytes))
        }
        Err(e) => {
            log_info!("[SFSNIPE] Failed to fetch config PDA: {}", e);
            None
        }
    }
}

// ==============================================================
// Main buy flow
// ==============================================================

/// Called when a new Slide.fun token is detected.
/// Builds and sends a buy transaction against the Slide.fun bonding curve.
pub async fn handle_slidefun_buy(
    config: &Config,
    rpc_client: Arc<RpcClient>,
    token_mint: &str,
    token_program_pk: Pubkey,
    trades: Arc<TradesStore>,
    event_time: std::time::Instant,
) {
    let user = config.keypair.pubkey();
    let sol_lamports = (config.slidefun_pump_amount * LAMPORTS_PER_SOL as f64) as u64;
    let jito_lamports = (config.jito_tip * LAMPORTS_PER_SOL as f64) as u64;

    log_info!("[SFSNIPE] >> Slide.fun BUY token: {}", token_mint);
    log_info!(
        "[SFSNIPE]    SOL amount: {} SOL ({} lamports)",
        config.slidefun_pump_amount,
        sol_lamports
    );

    let token_mint_pk = match Pubkey::from_str(token_mint) {
        Ok(pk) => pk,
        Err(e) => {
            log_info!("[SFSNIPE] Invalid token mint: {}", e);
            return;
        }
    };
    let payment_mint = Pubkey::from_str(constants::WSOL_MINT).unwrap();
    let token_program = Pubkey::from_str(constants::TOKEN_PROGRAM).unwrap();
    let user_payment_ata = get_associated_token_address(&user, &payment_mint);

    // Get fee_to from cache or fetch if missing
    let slidefun_program = config.slidefun_program();
    let fee_to = {
        let cached = CACHED_FEE_TO.lock().unwrap().clone();
        if let Some(pk) = cached {
            pk
        } else {
            // Fallback (should not happen after startup)
            match fetch_fee_to(&rpc_client, &slidefun_program).await {
                Some(pk) => pk,
                None => {
                    log_info!("[SFSNIPE] Could not get fee_to — aborting buy");
                    return;
                }
            }
        }
    };

    if config.dry_run {
        log_info!("[SFSNIPE] DRY RUN — not sending transaction");
        log_info!(
            "[SFSNIPE]    Would buy {} SOL of {}",
            config.slidefun_pump_amount,
            token_mint
        );
        log_info!("[SFSNIPE]    fee_to: {}", fee_to);
        return;
    }

    let blockhash = crate::blockhash::get_blockhash();

    // Build instruction list:
    // 1. ComputeBudget
    // 2. Create user WSOL ATA (idempotent)
    // 3. Transfer SOL -> WSOL ATA
    // 4. SyncNative (to make it valid WSOL)
    // 5. Create user token ATA (idempotent)
    // 6. The buy instruction
    // 7. Jito tip

    let actual_token_program = token_program_pk; // Use the program passed from the parser

    let jito_tip_address = Pubkey::from_str(crate::constants::jito_tip_address()).unwrap();

    let buy_ix = build_slidefun_buy_instruction(
        &user,
        &token_mint_pk,
        &payment_mint,
        &fee_to,
        &slidefun_program,
        &actual_token_program,
        sol_lamports,
        0, // min_token_out = 0 (no slippage protection, max speed)
    );

    let ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(config.cu_limit),
        ComputeBudgetInstruction::set_compute_unit_price(config.priority_fee),
        // Prepare WSOL (Always standard Token Program for WSOL)
        create_associated_token_account_idempotent(&user, &user, &payment_mint, &token_program),
        system_instruction::transfer(&user, &user_payment_ata, sol_lamports),
        Instruction {
            program_id: token_program,
            accounts: vec![AccountMeta::new(user_payment_ata, false)],
            data: vec![17], // SyncNative
        },
        // Create user token ATA using the ACTUAL program for the mint
        create_associated_token_account_idempotent(
            &user,
            &user,
            &token_mint_pk,
            &actual_token_program,
        ),
        // BUY!
        buy_ix,
        // Jito tip
        system_instruction::transfer(&user, &jito_tip_address, jito_lamports),
    ];

    let msg = match Message::try_compile(&user, &ixs, &[], blockhash) {
        Ok(m) => m,
        Err(e) => {
            log_info!("[SFSNIPE] Message compile error: {}", e);
            return;
        }
    };

    let tx = match VersionedTransaction::try_new(VersionedMessage::V0(msg), &[&config.keypair]) {
        Ok(t) => t,
        Err(e) => {
            log_info!("[SFSNIPE] Sign error: {}", e);
            return;
        }
    };

    let serialized = bincode::serialize(&tx).unwrap_or_default();
    log_info!(
        "[SFSNIPE] TX size: {} bytes (limit: 1232)",
        serialized.len()
    );
    if serialized.len() > 1232 {
        log_info!("[SFSNIPE] ERROR: TX too large! Aborting.");
        return;
    }

    // base64 encoding is Jito's recommended format; base58 is deprecated
    let encoded = base64::engine::general_purpose::STANDARD.encode(&serialized);

    log_info!(
        "[SFSNIPE] 🚀 Firing Slide.fun buy ({} SOL)...",
        config.slidefun_pump_amount
    );

    // Spam via RPC, but only send via Jito ONCE
    let spam_count = 4;
    for attempt in 1..=spam_count {
        let rpc_clone = rpc_client.clone();
        let tx_clone = tx.clone();
        let trades_c = trades.clone();
        let mint_c = token_mint.to_string();
        let sol_c = config.slidefun_pump_amount;
        let network_c = config.network.clone();
        let wallet_c = config.keypair.pubkey().to_string();

        tokio::spawn(async move {
            // Send via RPC
            let config_rpc = RpcSendTransactionConfig {
                skip_preflight: true, // Always skip preflight because the node might be behind the WS state
                max_retries: Some(0),
                ..Default::default()
            };
            match rpc_clone
                .send_transaction_with_config(&tx_clone, config_rpc)
                .await
            {
                Ok(sig) => {
                    let cluster_suffix = if network_c.to_lowercase() == "devnet" { "?cluster=devnet" } else { "" };
                    log_info!("[SFSNIPE] RPC attempt {} OK: {}{}", attempt, sig, cluster_suffix);

                    // Only record once — on first successful attempt (same sig for all retries)
                    if attempt == 1 {
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
                                mode: "slidefun".to_string(),
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
                    if attempt == 1 {
                        log_info!("[SFSNIPE] RPC attempt {} preflight error: {}", attempt, e);
                    }
                }
            }
        });

        // Send via Jito bundle ONLY on the first attempt and ONLY on Mainnet
        if attempt == 1 && config.network.to_lowercase() == "mainnet" {
            let encoded_clone2 = encoded.clone();
            tokio::spawn(async move {
                if let Err(e) = send_via_jito(&[encoded_clone2]).await {
                    log_info!("[SFSNIPE] Jito error: {}", e);
                }
            });
        }

        if attempt < spam_count {
            sleep(Duration::from_millis(10)).await;
        }
    }

    log_info!("[SFSNIPE] ✅ Slide.fun buy sent! Token: {}", token_mint);
}

// ==============================================================
// Transaction parser: detect new token creation
// ==============================================================

/// Check logs for a `create_bonding_curve` event from Slide.fun.
pub fn is_creation_signal(logs: &[String], program_id_str: &str) -> bool {
    let logs_str = logs.join(" ");
    logs_str.contains(program_id_str) && logs_str.contains("Instruction: CreateBondingCurve")
}

/// Parse the transaction to extract the new token mint from `create_bonding_curve`.
/// Returns (token_mint, creator_pubkey)
pub async fn extract_new_token_and_creator(
    rpc_client: &RpcClient,
    signature: &str,
    program_id: &Pubkey,
) -> Option<(String, String, Pubkey)> {
    let slidefun_program = *program_id;
    let sig = Signature::from_str(signature).ok()?;
    let config = RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::Base64),
        commitment: Some(CommitmentConfig::confirmed()),
        max_supported_transaction_version: Some(0),
    };

    for attempt in 0..5 {
        match rpc_client
            .get_transaction_with_config(&sig, config.clone())
            .await
        {
            Ok(tx) => {
                if let Some(decoded) = tx.transaction.transaction.decode() {
                    let message = &decoded.message;
                    let mut all_keys = message.static_account_keys().to_vec();

                    if let Some(meta) = &tx.transaction.meta {
                        use solana_transaction_status_client_types::option_serializer::OptionSerializer;
                        if let OptionSerializer::Some(loaded) = &meta.loaded_addresses {
                            for addr in &loaded.writable {
                                if let Ok(pk) = Pubkey::from_str(addr) {
                                    all_keys.push(pk);
                                }
                            }
                            for addr in &loaded.readonly {
                                if let Ok(pk) = Pubkey::from_str(addr) {
                                    all_keys.push(pk);
                                }
                            }
                        }
                    }

                    let instructions = match message {
                        solana_sdk::message::VersionedMessage::Legacy(m) => &m.instructions,
                        solana_sdk::message::VersionedMessage::V0(m) => &m.instructions,
                    };

                    for ix in instructions {
                        let program_id = all_keys[ix.program_id_index as usize];
                        if program_id != slidefun_program {
                            continue;
                        }

                        // Match create_bonding_curve discriminator
                        if ix.data.len() < 8
                            || ix.data[0..8]
                                != constants::SLIDEFUN_CREATE_BONDING_CURVE_DISCRIMINATOR
                        {
                            continue;
                        }

                        // account[2] = token mint
                        // The creator is typically the fee payer, which is all_keys[0], or we can extract the signer from the ix.accounts if it exists.
                        // On slide.fun IDL (standard pumpfun fork), the user who creates the curve is account 0 of the create_bonding_curve instruction or all_keys[0].
                        // Let's use all_keys[0] which is the fee payer and signer of the transaction.
                        if ix.accounts.len() > 2 {
                            let token_mint = all_keys[ix.accounts[2] as usize];
                            let creator = all_keys[0]; // Fee payer is the creator

                            // Determine which token program is used by checking if Token-2022 is anywhere in the transaction
                            let mut token_program_pk =
                                Pubkey::from_str(constants::TOKEN_PROGRAM).unwrap();
                            let token_2022 =
                                Pubkey::from_str(constants::TOKEN_2022_PROGRAM).unwrap();
                            if all_keys.contains(&token_2022) {
                                token_program_pk = token_2022;
                            }

                            log_info!(
                                "[SFSNIPE] ✅ New token detected: {} (Program: {}) by creator: {}",
                                token_mint,
                                if token_program_pk.to_string() == constants::TOKEN_PROGRAM {
                                    "Token"
                                } else {
                                    "Token-2022"
                                },
                                creator
                            );
                            return Some((
                                token_mint.to_string(),
                                creator.to_string(),
                                token_program_pk,
                            ));
                        }
                    }
                }
                return None;
            }
            Err(_) => {
                if attempt < 4 {
                    sleep(Duration::from_millis(10)).await;
                }
            }
        }
    }
    None
}
