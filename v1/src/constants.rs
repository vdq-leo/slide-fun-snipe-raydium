// ==================== SLIDE-FUN RAYDIUM SNIPER - CONSTANTS ====================
use lazy_static::lazy_static;
use std::env;

// Slide.fun program ID
pub const SLIDEFUN_PROGRAM: &str = "GkF6F9GNPjzkC18Xa3a88xwEc5vwyQDA1iXvFkKBqNDC";
pub const SLIDEFUN_PROGRAM_DEVNET: &str = "6t4ZUwYAdpeB61y8jsMo4Kq1PZYaZvkfnaZUyXaJZeng"; // Devnet default

lazy_static! {
    static ref SLIDEFUN_PROGRAM_RUNTIME: String = env::var("SLIDEFUN_PROGRAM")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| SLIDEFUN_PROGRAM.to_string());
}

pub fn slidefun_program() -> &'static str {
    SLIDEFUN_PROGRAM_RUNTIME.as_str()
}

pub fn slidefun_program_devnet() -> &'static str {
    SLIDEFUN_PROGRAM_DEVNET
}

// Slide.fun migrate instruction discriminator
// Anchor: sha256("global:migrate")[0..8]
pub const SLIDEFUN_MIGRATE_DISCRIMINATOR: [u8; 8] = [155, 234, 231, 146, 236, 158, 162, 30];

// Slide.fun buy instruction discriminator (from IDL: buy discriminator = [102, 6, 61, 18, 1, 218, 235, 234])
pub const SLIDEFUN_BUY_DISCRIMINATOR: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];

// Slide.fun sell instruction discriminator (from IDL: sell discriminator = [51, 230, 133, 164, 1, 48, 99, 66])
pub const SLIDEFUN_SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 48, 99, 66];

// Slide.fun create_bonding_curve discriminator (from IDL: [94, 139, 158, 50, 69, 95, 8, 45])
pub const SLIDEFUN_CREATE_BONDING_CURVE_DISCRIMINATOR: [u8; 8] = [94, 139, 158, 50, 69, 95, 8, 45];

// Slide.fun config PDA seed = b"config"
pub const SLIDEFUN_CONFIG_SEED: &[u8] = b"config";
// Slide.fun bonding_curve PDA seed = b"bonding_curve"
pub const SLIDEFUN_BONDING_CURVE_SEED: &[u8] = b"bonding_curve";

// Raydium AMM V4
pub const RAYDIUM_AMM_PROGRAM: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
pub const RAYDIUM_AMM_PROGRAM_DEVNET: &str = "DRaycpLY18LhpbydsBWbVJtxpNv9oXPgjRSfpF2bWpYb";
pub const RAYDIUM_AUTHORITY: &str = "5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1";

lazy_static! {
    static ref RAYDIUM_AMM_PROGRAM_RUNTIME: String =
        env::var("RAYDIUM_AMM_PROGRAM").unwrap_or_else(|_| RAYDIUM_AMM_PROGRAM.to_string());
    static ref RAYDIUM_AUTHORITY_RUNTIME: String =
        env::var("RAYDIUM_AUTHORITY").unwrap_or_else(|_| RAYDIUM_AUTHORITY.to_string());
}

pub fn raydium_amm_program() -> &'static str {
    RAYDIUM_AMM_PROGRAM_RUNTIME.as_str()
}
pub fn raydium_authority() -> &'static str {
    RAYDIUM_AUTHORITY_RUNTIME.as_str()
}

// Token programs
pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

// Jito MEV configuration
const JITO_TIP_ADDRESS: &str = "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5";
const JITO_BUNDLE_URLS: &[&str] = &[
    "https://london.mainnet.block-engine.jito.wtf/api/v1/bundles",
    "https://singapore.mainnet.block-engine.jito.wtf/api/v1/bundles",
    "https://ny.mainnet.block-engine.jito.wtf/api/v1/bundles",
    "https://amsterdam.mainnet.block-engine.jito.wtf/api/v1/bundles",
    "https://tokyo.mainnet.block-engine.jito.wtf/api/v1/bundles",
    "https://slc.mainnet.block-engine.jito.wtf/api/v1/bundles",
];

lazy_static! {
    static ref JITO_TIP_ADDRESS_RUNTIME: String =
        env::var("JITO_TIP_ADDRESS").unwrap_or_else(|_| JITO_TIP_ADDRESS.to_string());
    static ref JITO_BUNDLE_URLS_RUNTIME: Vec<String> = env::var("JITO_BUNDLE_URLS")
        .map(|v| v.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_else(|_| JITO_BUNDLE_URLS.iter().map(|&s| s.to_string()).collect());
}

pub fn jito_tip_address() -> &'static str {
    JITO_TIP_ADDRESS_RUNTIME.as_str()
}
pub fn jito_bundle_urls() -> Vec<String> {
    JITO_BUNDLE_URLS_RUNTIME.clone()
}

// Instruction discriminators
pub const SWAP_BASE_IN: u8 = 9; // AMM v4 swapBaseIn

// File paths
