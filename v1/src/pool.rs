use crate::{log_info, types::PoolInfo};
use solana_account_decoder::UiAccountEncoding;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_client::rpc_config::{
    RpcAccountInfoConfig, RpcProgramAccountsConfig, RpcTransactionConfig,
};
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey, signature::Signature};
use std::str::FromStr;
use tokio::time::{sleep, Duration};

use crate::constants::{TOKEN_2022_PROGRAM, TOKEN_PROGRAM, WSOL_MINT};
use solana_transaction_status_client_types::{UiInstruction, UiParsedInstruction};

/// Raydium AMM V4 initialize2 account layout (typically 21 accounts):
///   [4]  amm (writable)
///   [5]  amm_authority (readonly)
///   [6]  amm_open_orders (writable)
///   [7]  lp_mint (writable)
///   [8]  coin_mint (base)
///   [9]  pc_mint (quote)
///   [10] pool_coin_token_account (base vault)
///   [11] pool_pc_token_account (quote vault)
///   [12] pool_target_orders (writable)
///   [13] pool_lp_token_account
///   [14] pool_withdraw_queue
///   [15] pool_temp_lp_token_account
///   [16] serum_market (writable)  ← market_id
///   [17] serum_program (readonly) ← serum_program_id
///   NOTE: bids/asks/vaults are NOT in initialize2 accounts.
///         They must be fetched from the serum market account via RPC.

/// Serum/OpenBook market data (fetched from market account data)
#[derive(Debug, Clone)]
pub struct SerumMarketData {
    pub bids: Pubkey,
    pub asks: Pubkey,
    pub event_queue: Pubkey,
    pub coin_vault: Pubkey,
    pub pc_vault: Pubkey,
    pub vault_signer_nonce: u64,
}

/// Fetch and parse Serum market account to get swap-required accounts.
/// This is a necessary RPC call — these accounts are not in the initialize2 TX.
pub async fn parse_serum_market(
    rpc_client: &RpcClient,
    market_id: &Pubkey,
) -> Option<SerumMarketData> {
    // Retry a few times — market account should be available quickly
    for attempt in 0..5 {
        match rpc_client.get_account(market_id).await {
            Ok(account) => {
                let data = &account.data;
                // Serum v3 market account layout (with 5-byte padding prefix):
                // [0..5]    padding
                // [5..13]   account_flags
                // [13..45]  own_address (32 bytes)
                // [45..53]  vault_signer_nonce (8 bytes)  ← nonce here
                // [53..85]  base_mint (32 bytes)
                // [85..117] quote_mint (32 bytes)
                // [117..149] base_vault / coin_vault (32 bytes) ← coin_vault
                // [149..157] base_deposits_total
                // [13..45]  own_address
                // [45..53]  vault_signer_nonce  ← nonce
                // [53..85]  base_mint
                // [85..117] quote_mint
                // [117..149] coin_vault         ← base_vault
                // [149..165] (deposits + fees)
                // [165..197] pc_vault           ← quote_vault
                // [197..253] (fees, dust, etc.)
                // [253..285] event_queue         ← event_queue
                // [285..317] bids               ← bids
                // [317..349] asks               ← asks
                if data.len() < 350 {
                    log_info!(
                        "   [WARN] Serum market data too small: {} bytes (need 350+)",
                        data.len()
                    );
                    return None;
                }
                let vault_signer_nonce = u64::from_le_bytes(data[45..53].try_into().ok()?);
                let coin_vault = Pubkey::try_from(&data[117..149]).ok()?;
                let pc_vault = Pubkey::try_from(&data[165..197]).ok()?;
                let event_queue = Pubkey::try_from(&data[253..285]).ok()?;
                let bids = Pubkey::try_from(&data[285..317]).ok()?;
                let asks = Pubkey::try_from(&data[317..349]).ok()?;
                return Some(SerumMarketData {
                    bids,
                    asks,
                    event_queue,
                    coin_vault,
                    pc_vault,
                    vault_signer_nonce,
                });
            }
            Err(_) => {
                if attempt < 4 {
                    sleep(Duration::from_millis(20)).await;
                }
            }
        }
    }
    None
}

/// SPL Token (`Tokenkeg...`) or Token-2022 (`TokenzQd...`) from the mint account's program owner.
pub async fn mint_token_program_for_mint(rpc_client: &RpcClient, mint: &Pubkey) -> Pubkey {
    let tp = Pubkey::from_str(TOKEN_PROGRAM).unwrap();
    let t22 = Pubkey::from_str(TOKEN_2022_PROGRAM).unwrap();
    match rpc_client.get_account(mint).await {
        Ok(acc) => {
            if acc.owner == t22 {
                t22
            } else if acc.owner == tp {
                tp
            } else {
                log_info!(
                    "   [WARN] Mint {} owner {} is not SPL Token/Token-2022; defaulting to SPL Token",
                    mint,
                    acc.owner
                );
                tp
            }
        }
        Err(e) => {
            log_info!(
                "   [WARN] Could not fetch mint {}: {} — defaulting to SPL Token",
                mint,
                e
            );
            tp
        }
    }
}

/// Derive the Serum vault signer PDA
pub fn derive_vault_signer(market_id: &Pubkey, nonce: u64, serum_program: &Pubkey) -> Pubkey {
    let seeds: &[&[u8]] = &[market_id.as_ref(), &nonce.to_le_bytes()];
    Pubkey::create_program_address(seeds, serum_program).unwrap_or_default()
}

async fn build_pool_info_from_accounts(
    rpc_client: &RpcClient,
    accounts: &[Pubkey],
    data: &[u8],
    creator: Pubkey,
    amm_program: Pubkey,
) -> Option<PoolInfo> {
    let debug = std::env::var("DEBUG_POOL_PARSE").ok().as_deref() == Some("1");
    if debug {
        let disc = data.get(0).copied().unwrap_or(255);
        log_info!(
            "   [DEBUG] AMM ix candidate: accounts={}, data_len={}, disc={}",
            accounts.len(),
            data.len(),
            disc
        );
    }
    // initialize2 discriminator = 1
    if data.is_empty() || data[0] != 1 {
        return None;
    }
    if accounts.len() < 18 {
        log_info!(
            "   [DEBUG] AMM V4 only {} accounts (need 18+)",
            accounts.len()
        );
        return None;
    }

    let amm_id = accounts[4];
    let amm_authority = accounts[5];
    let open_orders = accounts[6];
    let lp_mint = accounts[7];
    let base_mint = accounts[8];
    let quote_mint = accounts[9];
    let base_vault = accounts[10];
    let quote_vault = accounts[11];
    let target_orders = accounts[12];
    let market_id = accounts[16];
    let serum_program = accounts[17];

    let wsol = Pubkey::from_str(WSOL_MINT).unwrap();
    if base_mint != wsol && quote_mint != wsol {
        log_info!("   [SKIP] Not a SOL pair");
        return None;
    }

    let token_mint = if base_mint == wsol {
        quote_mint
    } else {
        base_mint
    };
    let base_token_program = mint_token_program_for_mint(rpc_client, &token_mint).await;
    log_info!("   [+] Token program: {}", base_token_program);

    let (final_base_vault, final_quote_vault) = if base_mint == wsol {
        (quote_vault, base_vault)
    } else {
        (base_vault, quote_vault)
    };

    let open_time = if data.len() >= 10 {
        u64::from_le_bytes(data[2..10].try_into().unwrap_or([0; 8]))
    } else {
        0
    };
    let pool_sol_amount = if data.len() >= 26 {
        let init_pc = u64::from_le_bytes(data[10..18].try_into().unwrap_or([0; 8]));
        let init_coin = u64::from_le_bytes(data[18..26].try_into().unwrap_or([0; 8]));
        if base_mint == wsol {
            init_coin
        } else {
            init_pc
        }
    } else {
        0
    };

    log_info!("   [+] AMM ID: {}", amm_id);
    log_info!("   [+] Token:  {}", token_mint);
    log_info!("   [+] Pool:   {:.4} SOL", pool_sol_amount as f64 / 1e9);
    log_info!("   [+] Instruction accounts: {}", accounts.len());

    // HYBRID: Try to get serum accounts from TX first (Slide.fun SDK pools may have 24+).
    // Fallback to RPC call for standard Raydium UI pools (21 accounts).
    let (
        market_bids,
        market_asks,
        market_event_queue,
        market_coin_vault,
        market_pc_vault,
        market_vault_signer,
    ) = if accounts.len() >= 24 {
        log_info!("   [+] Serum accounts from TX (fast path, no extra RPC)");
        (
            accounts[18],
            accounts[19],
            accounts[20],
            accounts[21],
            accounts[22],
            accounts[23],
        )
    } else {
        log_info!("   [+] Fetching serum market accounts via RPC...");
        if let Some(data) = parse_serum_market(rpc_client, &market_id).await {
            let vault_signer =
                derive_vault_signer(&market_id, data.vault_signer_nonce, &serum_program);
            (
                data.bids,
                data.asks,
                data.event_queue,
                data.coin_vault,
                data.pc_vault,
                vault_signer,
            )
        } else {
            log_info!("   [WARN] Cannot parse Serum market — swap may fail");
            (
                Pubkey::default(),
                Pubkey::default(),
                Pubkey::default(),
                Pubkey::default(),
                Pubkey::default(),
                Pubkey::default(),
            )
        }
    };

    Some(PoolInfo {
        amm_program_id: amm_program,
        amm_id,
        amm_authority,
        base_mint: token_mint,
        base_token_program,
        quote_mint: wsol,
        lp_mint,
        base_vault: final_base_vault,
        quote_vault: final_quote_vault,
        amm_coin_vault: base_vault,
        amm_pc_vault: quote_vault,
        open_orders,
        market_id,
        target_orders,
        serum_program_id: serum_program,
        market_bids,
        market_asks,
        market_event_queue,
        market_coin_vault,
        market_pc_vault,
        market_vault_signer,
        pool_sol_amount,
        open_time,
        creator,
    })
}

/// Parse AMM V4 pool info from a Raydium pool creation transaction.
/// Requires 18+ accounts in the initialize2 instruction.
pub async fn get_pool_info(rpc_client: &RpcClient, signature: &str, amm_program: Pubkey) -> Option<PoolInfo> {
    let sig = Signature::from_str(signature).ok()?;

    let config = RpcTransactionConfig {
        encoding: Some(solana_transaction_status_client_types::UiTransactionEncoding::Base64),
        commitment: Some(CommitmentConfig::confirmed()),
        max_supported_transaction_version: Some(0),
    };

    for attempt in 0..10 {
        match rpc_client
            .get_transaction_with_config(&sig, config.clone())
            .await
        {
            Ok(tx) => {
                if let Some(decoded) = tx.transaction.transaction.decode() {
                    let message = &decoded.message;

                    let mut all_account_keys = message.static_account_keys().to_vec();
                    if let Some(meta) = &tx.transaction.meta {
                        use solana_transaction_status_client_types::option_serializer::OptionSerializer;
                        if let OptionSerializer::Some(loaded) = &meta.loaded_addresses {
                            for addr in &loaded.writable {
                                if let Ok(pubkey) = Pubkey::from_str(addr) {
                                    all_account_keys.push(pubkey);
                                }
                            }
                            for addr in &loaded.readonly {
                                if let Ok(pubkey) = Pubkey::from_str(addr) {
                                    all_account_keys.push(pubkey);
                                }
                            }
                        }
                    }

                    let instructions = match message {
                        solana_sdk::message::VersionedMessage::Legacy(m) => &m.instructions,
                        solana_sdk::message::VersionedMessage::V0(m) => &m.instructions,
                    };

                    let debug = std::env::var("DEBUG_POOL_PARSE").ok().as_deref() == Some("1");

                    for ix in instructions {
                        let program_id = all_account_keys[ix.program_id_index as usize];
                        if debug {
                            log_info!(
                                "   [DEBUG] top-level program_id={}, data_len={}, accounts={}",
                                program_id,
                                ix.data.len(),
                                ix.accounts.len()
                            );
                        }
                        if program_id != amm_program {
                            continue;
                        }
                        let mut accounts: Vec<Pubkey> = Vec::with_capacity(ix.accounts.len());
                        for idx in &ix.accounts {
                            let i = *idx as usize;
                            if i >= all_account_keys.len() {
                                accounts.clear();
                                break;
                            }
                            accounts.push(all_account_keys[i]);
                        }
                        if accounts.is_empty() {
                            continue;
                        }
                        if let Some(p) =
                            build_pool_info_from_accounts(rpc_client, &accounts, &ix.data, all_account_keys[0], amm_program).await
                        {
                            return Some(p);
                        }
                    }

                    // Some pool-create flows call Raydium initialize2 via CPI (inner instruction).
                    if let Some(meta) = &tx.transaction.meta {
                        use solana_transaction_status_client_types::option_serializer::OptionSerializer;
                        if let OptionSerializer::Some(inner) = &meta.inner_instructions {
                            for inner_ixs in inner {
                                for ui_ix in &inner_ixs.instructions {
                                    match ui_ix {
                                        UiInstruction::Compiled(c) => {
                                            let program_id =
                                                all_account_keys[c.program_id_index as usize];
                                            if debug {
                                                log_info!(
                                                    "   [DEBUG] inner compiled program_id={}, data_len={}, accounts={}",
                                                    program_id,
                                                    bs58::decode(&c.data).into_vec().map(|v| v.len()).unwrap_or(0),
                                                    c.accounts.len()
                                                );
                                            }
                                            if program_id != amm_program {
                                                continue;
                                            }
                                            let data = bs58::decode(&c.data)
                                                .into_vec()
                                                .unwrap_or_default();
                                            let mut accounts: Vec<Pubkey> =
                                                Vec::with_capacity(c.accounts.len());
                                            for idx in &c.accounts {
                                                let i = *idx as usize;
                                                if i >= all_account_keys.len() {
                                                    accounts.clear();
                                                    break;
                                                }
                                                accounts.push(all_account_keys[i]);
                                            }
                                            if accounts.is_empty() {
                                                continue;
                                            }
                                            if let Some(p) = build_pool_info_from_accounts(
                                                rpc_client, &accounts, &data, all_account_keys[0], amm_program
                                            ).await
                                            {
                                                return Some(p);
                                            }
                                        }
                                        UiInstruction::Parsed(
                                            UiParsedInstruction::PartiallyDecoded(p),
                                        ) => {
                                            let program_id =
                                                Pubkey::from_str(&p.program_id).ok()?;
                                            if debug {
                                                log_info!(
                                                    "   [DEBUG] inner partial program_id={}, data_len={}, accounts={}",
                                                    program_id,
                                                    bs58::decode(&p.data).into_vec().map(|v| v.len()).unwrap_or(0),
                                                    p.accounts.len()
                                                );
                                            }
                                            if program_id != amm_program {
                                                continue;
                                            }
                                            let data = bs58::decode(&p.data)
                                                .into_vec()
                                                .unwrap_or_default();
                                            let mut accounts: Vec<Pubkey> =
                                                Vec::with_capacity(p.accounts.len());
                                            for a in &p.accounts {
                                                let pk = Pubkey::from_str(a).ok()?;
                                                accounts.push(pk);
                                            }
                                            if let Some(p) = build_pool_info_from_accounts(
                                                rpc_client, &accounts, &data, all_account_keys[0], amm_program
                                            ).await
                                            {
                                                return Some(p);
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                    return None;
                } else {
                    log_info!(
                        "   [POOL] TX decode failed (attempt {}/10) — retrying...",
                        attempt + 1
                    );
                    sleep(Duration::from_millis(10)).await;
                    continue;
                }
            }
            Err(e) => {
                log_info!(
                    "   [POOL] get_transaction error (attempt {}/10): {}",
                    attempt + 1,
                    e
                );
                if attempt < 9 {
                    sleep(Duration::from_millis(10)).await;
                }
            }
        }
    }
    log_info!(
        "[POOL] ❌ Failed to parse pool info after 10 attempts for TX: {}",
        signature
    );
    None
}

/// Find a Raydium AMM V4 pool for a given mint.
/// It uses getProgramAccounts with Memcmp filters to find the AMM ID.
pub async fn find_pool_by_mint(rpc_client: &RpcClient, mint: &Pubkey, amm_program: Pubkey) -> Option<PoolInfo> {
    use solana_client::rpc_filter::{Memcmp, RpcFilterType};

    let wsol = Pubkey::from_str(crate::constants::WSOL_MINT).unwrap();

    log_info!("[POOL] Searching for pool for mint {}...", mint);

    // Filter: Size 752, and mint at offset 400 (base) and WSOL at 432 (quote)
    let filters = vec![
        RpcFilterType::DataSize(752),
        RpcFilterType::Memcmp(Memcmp::new(
            400,
            solana_client::rpc_filter::MemcmpEncodedBytes::Bytes(mint.to_bytes().to_vec()),
        )),
        RpcFilterType::Memcmp(Memcmp::new(
            432,
            solana_client::rpc_filter::MemcmpEncodedBytes::Bytes(wsol.to_bytes().to_vec()),
        )),
    ];

    let mut results = rpc_client
        .get_program_accounts_with_config(
            &amm_program,
            RpcProgramAccountsConfig {
                filters: Some(filters),
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    commitment: Some(CommitmentConfig::confirmed()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .ok()?;

    if results.is_empty() {
        // Try other direction: mint at offset 432 (quote) and WSOL at 400 (base)
        let filters = vec![
            RpcFilterType::DataSize(752),
            RpcFilterType::Memcmp(Memcmp::new(
                432,
                solana_client::rpc_filter::MemcmpEncodedBytes::Bytes(mint.to_bytes().to_vec()),
            )),
            RpcFilterType::Memcmp(Memcmp::new(
                400,
                solana_client::rpc_filter::MemcmpEncodedBytes::Bytes(wsol.to_bytes().to_vec()),
            )),
        ];
        results = rpc_client
            .get_program_accounts_with_config(
                &amm_program,
                RpcProgramAccountsConfig {
                    filters: Some(filters),
                    account_config: RpcAccountInfoConfig {
                        encoding: Some(UiAccountEncoding::Base64),
                        commitment: Some(CommitmentConfig::confirmed()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .await
            .ok()?;
    }

    if let Some((amm_id, _account)) = results.first() {
        log_info!(
            "[POOL] Found AMM ID: {}. Fetching pool history to reconstruct PoolInfo...",
            amm_id
        );
        // To get the full PoolInfo (market IDs, etc), we find the creation signature or any recent swap
        match rpc_client
            .get_signatures_for_address_with_config(
                amm_id,
                GetConfirmedSignaturesForAddress2Config {
                    limit: Some(50),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(sigs) => {
                for sig_info in sigs {
                    if let Some(pool) = get_pool_info(rpc_client, &sig_info.signature, amm_program).await {
                        return Some(pool);
                    }
                }
            }
            Err(e) => log_info!("[POOL] Failed to get signatures for {}: {}", amm_id, e),
        }
    }

    log_info!("[POOL] No Raydium AMM V4 pool found for mint {}", mint);
    None
}

/// Parse Raydium AMM V4 pool info **directly from the AMM account data** (752 bytes).
///
/// This is the preferred method when you already know the AMM pool address.
/// It is:
///   - Much faster than `get_pool_info` (1 RPC call vs scanning 30+ transactions)
///   - Works for any pool regardless of how old the creation TX is
///
/// Raydium AMM V4 state account layout (752 bytes):
///   Offset  8: nonce (u64)                ← bump seed for AMM authority
///   Offset 336: base_vault (Pubkey)       ← pool_coin_token_account
///   Offset 368: quote_vault (Pubkey)      ← pool_pc_token_account
///   Offset 400: base_mint (Pubkey)        ← coin_mint_address
///   Offset 432: quote_mint (Pubkey)       ← pc_mint_address (WSOL)
///   Offset 464: lp_mint (Pubkey)
///   Offset 496: open_orders (Pubkey)      ← amm_open_orders
///   Offset 528: market_id (Pubkey)        ← serum_market
///   Offset 560: serum_program (Pubkey)    ← serum_program_id
///   Offset 592: target_orders (Pubkey)    ← amm_target_orders
pub async fn get_pool_info_from_amm_account(
    rpc_client: &RpcClient,
    amm_id: &Pubkey,
    amm_program: Pubkey,
) -> Option<PoolInfo> {
    let account = match rpc_client.get_account(amm_id).await {
        Ok(acc) => acc,
        Err(e) => {
            log_info!("[POOL] Failed to fetch AMM account {}: {}", amm_id, e);
            return None;
        }
    };
    let data = &account.data;

    if data.len() < 752 {
        log_info!(
            "[POOL] AMM account data too small: {} bytes (need 752). Not a Raydium AMM V4 account.",
            data.len()
        );
        return None;
    }

    // Nonce at offset 8 — used as the bump seed to derive the AMM authority
    let nonce = data[8]; // take only the lowest byte

    // Parse all key Pubkeys at their fixed offsets
    let base_vault    = Pubkey::try_from(&data[336..368]).ok()?; // pool_coin_token_account
    let quote_vault   = Pubkey::try_from(&data[368..400]).ok()?; // pool_pc_token_account
    let base_mint     = Pubkey::try_from(&data[400..432]).ok()?; // coin_mint
    let quote_mint    = Pubkey::try_from(&data[432..464]).ok()?; // pc_mint (usually WSOL)
    let lp_mint       = Pubkey::try_from(&data[464..496]).ok()?;
    let open_orders   = Pubkey::try_from(&data[496..528]).ok()?;
    let market_id     = Pubkey::try_from(&data[528..560]).ok()?;
    let serum_program = Pubkey::try_from(&data[560..592]).ok()?;
    let target_orders = Pubkey::try_from(&data[592..624]).ok()?;

    log_info!("[POOL] Parsed AMM account: {}", amm_id);
    log_info!("[POOL]   base_mint   : {}", base_mint);
    log_info!("[POOL]   quote_mint  : {}", quote_mint);
    log_info!("[POOL]   base_vault  : {}", base_vault);
    log_info!("[POOL]   quote_vault : {}", quote_vault);
    log_info!("[POOL]   market_id   : {}", market_id);

    // Derive the AMM authority from the nonce
    let authority_seeds: &[&[u8]] = &[b"amm authority", &[nonce]];
    let amm_authority = match Pubkey::create_program_address(authority_seeds, &amm_program) {
        Ok(pk) => pk,
        Err(e) => {
            log_info!("[POOL] Failed to derive AMM authority (nonce={}): {}", nonce, e);
            return None;
        }
    };
    log_info!("[POOL]   amm_authority: {}", amm_authority);

    let wsol = Pubkey::from_str(WSOL_MINT).unwrap();
    if base_mint != wsol && quote_mint != wsol {
        log_info!("[POOL] Not a SOL/WSOL pair — skipping");
        return None;
    }

    let token_mint = if base_mint == wsol { quote_mint } else { base_mint };
    let base_token_program = mint_token_program_for_mint(rpc_client, &token_mint).await;
    log_info!("[POOL]   token_mint  : {} (program: {})", token_mint, base_token_program);

    // Vaults are stored as coin=base, pc=quote in the AMM state.
    // Our PoolInfo uses base=token, quote=WSOL convention.
    let (final_base_vault, final_quote_vault) = if base_mint == wsol {
        (quote_vault, base_vault)
    } else {
        (base_vault, quote_vault)
    };

    // Fetch serum market data for bids/asks/vaults
    let (market_bids, market_asks, market_event_queue, market_coin_vault, market_pc_vault, market_vault_signer) =
        if let Some(market_data) = parse_serum_market(rpc_client, &market_id).await {
            let vault_signer = derive_vault_signer(&market_id, market_data.vault_signer_nonce, &serum_program);
            (
                market_data.bids,
                market_data.asks,
                market_data.event_queue,
                market_data.coin_vault,
                market_data.pc_vault,
                vault_signer,
            )
        } else {
            log_info!("[POOL] Cannot parse Serum market {} — swap accounts unavailable", market_id);
            return None;
        };

    log_info!("[POOL] ✅ PoolInfo reconstructed from AMM account data!");

    Some(PoolInfo {
        amm_program_id: amm_program,
        amm_id: *amm_id,
        amm_authority,
        base_mint: token_mint,
        base_token_program,
        quote_mint: wsol,
        lp_mint,
        base_vault: final_base_vault,
        quote_vault: final_quote_vault,
        amm_coin_vault: base_vault,
        amm_pc_vault: quote_vault,
        open_orders,
        market_id,
        target_orders,
        serum_program_id: serum_program,
        market_bids,
        market_asks,
        market_event_queue,
        market_coin_vault,
        market_pc_vault,
        market_vault_signer,
        pool_sol_amount: 0,
        open_time: 0,
        creator: Pubkey::default(),
    })
}
