use solana_sdk::pubkey::Pubkey;

/// AMM V4 pool information (all fields needed for swap)
#[derive(Debug, Clone)]
pub struct PoolInfo {
    pub amm_program_id: Pubkey,
    pub amm_id: Pubkey,
    pub amm_authority: Pubkey,
    pub base_mint: Pubkey, // Token mint (from Slide.fun)
    /// SPL Token or Token-2022 — must match on-chain mint owner for ATAs + Raydium swap.
    pub base_token_program: Pubkey,
    pub quote_mint: Pubkey, // WSOL
    pub lp_mint: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub amm_coin_vault: Pubkey,
    pub amm_pc_vault: Pubkey,
    pub open_orders: Pubkey,
    pub market_id: Pubkey,
    pub target_orders: Pubkey,
    pub serum_program_id: Pubkey,
    pub market_bids: Pubkey,
    pub market_asks: Pubkey,
    pub market_event_queue: Pubkey,
    pub market_coin_vault: Pubkey,
    pub market_pc_vault: Pubkey,
    pub market_vault_signer: Pubkey,
    pub pool_sol_amount: u64,
    pub open_time: u64,
    pub creator: Pubkey,
}
