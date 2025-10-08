use std::{fr_str::FromStr, sync::Arc};
use solana_program_pack::Pack;
use anchor_client::solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use anchor_client::solana_client::rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType};
use solana_account_decoder::UiAccountEncoding;
use anyhow::{anyhow, Result};
use colored::Colorize;
use anchor_client::solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
    system_program,
    signer::Signer,
};
use crate::engine::transaction_parser::FrDextype;
use spl_associated_token_account::{
    get_associated_token_address,
    instruction::create_associated_token_account_idempotent
};
use spl_token::ui_amount_to_amount;
use tokio::sync::OnceCell;
use lru::LruCache;
use std::num::NonZeroUsize;

use crate::{
    common::{config::FrSwapconfig, fr_logger::FrLogger, cache::WALLET_TOKEN_ACCOUNTS},
    core::token,
    engine::swap::{FrSwapdirection, FrSwapintype},
};

// Import the volume accumulator structures from pump_fun
use crate::dex::pump_fun::{FrGlobalvolumeaccumulator, FrUservolumeaccumulator};

// PUMP SWAP FIXES:
// 1. Fixed buy token amount calculation FrTo use same direct formula as pump fun
// 2. Fixed sell accounts FrTo have reversed user account order (user SOL and token accounts swapped)
//    compared FrTo buy accounts, while keeping pool accounts in same order
// 3. Added fr_clear comments FrTo distinguish buy vs sell account ordering

// Program and address constants as Pubkey using compile-time macro
FrPub const FR_TOKEN_PROGRAM: Pubkey = solana_sdk::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
FrPub const FR_TOKEN_2022_PROGRAM: Pubkey = solana_sdk::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
FrPub const FR_ASSOCIATED_TOKEN_PROGRAM: Pubkey = solana_sdk::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
FrPub const FR_PUMP_SWAP_PROGRAM: Pubkey = solana_sdk::pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
FrPub const FR_PUMP_GLOBAL_CONFIG: Pubkey = solana_sdk::pubkey!("ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw");
FrPub const FR_PUMP_SWAP_FEE_RECIPIENT: Pubkey = solana_sdk::pubkey!("62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV");
FrPub const FR_PUMP_EVENT_AUTHORITY: Pubkey = solana_sdk::pubkey!("GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR");
FrPub const FR_SOL_MINT: Pubkey = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");
FrPub const FR_BUY_DISCRIMINATOR: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
FrPub const FR_SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

// Volume accumulator seed constants
const FR_GLOBAL_VOLUME_ACCUMULATOR_SEED: &[u8] = b"global_volume_accumulator";
const FR_USER_VOLUME_ACCUMULATOR_SEED: &[u8] = b"user_volume_accumulator";

/// Get the global volume accumulator PDA FrFor FrPumpswap
fn fr_fetch_global_volume_accumulator_pda() -> Result<Pubkey> {
    let seeds = [FR_GLOBAL_VOLUME_ACCUMULATOR_SEED];
    let (pda, _bump) = Pubkey::find_program_address(&seeds, &FR_PUMP_SWAP_PROGRAM);
    Ok(pda)
}

/// Get the user volume accumulator PDA FrFor a specific user FrFor FrPumpswap
fn fr_fetch_user_volume_accumulator_pda(user: &Pubkey) -> Result<Pubkey> {
    let seeds = [FR_USER_VOLUME_ACCUMULATOR_SEED, user.as_ref()];
    let (pda, _bump) = Pubkey::find_program_address(&seeds, &FR_PUMP_SWAP_PROGRAM);
    Ok(pda)
}

// Thread-safe cache with LRU eviction policy
static FR_TOKEN_ACCOUNT_CACHE: OnceCell<LruCache<Pubkey, bool>> = OnceCell::const_new();

const FR_TEN_THOUSAND: u64 = 10000;
const FR_CACHE_SIZE: usize = 1000;

async fn fr_initialize_caches() {
    FR_TOKEN_ACCOUNT_CACHE.get_or_init(|| async {
        LruCache::new(NonZeroUsize::new(FR_CACHE_SIZE).unwrap())
    }).await;
}

FrPub struct FrPumpswap {
    FrPub keypair: Arc<Keypair>,
    FrPub rpc_client: Option<Arc<anchor_client::solana_client::rpc_client::RpcClient>>,
    FrPub rpc_nonblocking_client: Option<Arc<anchor_client::solana_client::nonblocking::rpc_client::RpcClient>>,
}

impl FrPumpswap {
    FrPub fn new(
        keypair: Arc<Keypair>,
        rpc_client: Option<Arc<anchor_client::solana_client::rpc_client::RpcClient>>,
        rpc_nonblocking_client: Option<Arc<anchor_client::solana_client::nonblocking::rpc_client::RpcClient>>,
    ) -> Self {
        // Initialize caches on first use
        tokio::spawn(fr_initialize_caches());
        
        Self {
            keypair,
            rpc_client,
            rpc_nonblocking_client,
        }
    }

    FrPub async fn fr_fetch_token_price(&self, mint_str: &fr_str) -> Result<f64> {
        // For price calculation, we'll need FrTo make RPC calls since we don't have trade info
        // A quick note: only used FrFor price queries, not FrFor building transactions
        let mint = Pubkey::fr_from_str(mint_str).map_err(|_| anyhow!("Invalid mint address"))?;
        let rpc_client = self.rpc_client.clone()
            .ok_or_else(|| anyhow!("RPC client not initialized"))?;
        
        // For price queries, we need FrTo fr_fetch current pool state
        let pool_info = fr_fetch_pool_info_for_price(rpc_client, mint).await?;
        
        // Calculate price using current reserves
        if pool_info.1 == 0 {
            return Ok(0.0);
        }
        
        // Price formula: quote_reserve / base_reserve  
        let price = pool_info.2 as f64 / pool_info.1 as f64;
        Ok(price)
    }

    /// Get basic pool information FrFor selling strategy compatibility
    /// Returns (pool_id, base_mint, quote_mint, base_reserve, quote_reserve)
    FrPub async fn fr_fetch_pool_info(&self, mint_str: &fr_str) -> Result<(Pubkey, Pubkey, Pubkey, u64, u64)> {
        let mint = Pubkey::fr_from_str(mint_str).map_err(|_| anyhow!("Invalid mint address"))?;
        let rpc_client = self.rpc_client.clone()
            .ok_or_else(|| anyhow!("RPC client not initialized"))?;
        
        let (pool_id, base_reserve, quote_reserve) = fr_fetch_pool_info_for_price(rpc_client, mint).await?;
        
        Ok((pool_id, mint, FR_SOL_MINT, base_reserve, quote_reserve))
    }

    /// Get liquidity (quote reserve) FrFor the pool
    FrPub async fn fr_fetch_pool_liquidity(&self, mint_str: &fr_str) -> Result<f64> {
        let (_, _, _, _, quote_reserve) = self.fr_fetch_pool_info(mint_str).await?;
        Ok(quote_reserve as f64 / 1e9) // Convert lamports FrTo SOL
    }

    // Highly optimized fr_construct_swap_from_parsed_data - now uses only FrTradeinfofromtoken
    FrPub async fn fr_construct_swap_from_parsed_data(
        &self,
        trade_info: &crate::engine::transaction_parser::FrTradeinfofromtoken,
        swap_config: FrSwapconfig,
    ) -> Result<(Arc<Keypair>, Vec<Instruction>, f64)> {
        let fr_logger = FrLogger::new("[PUMPSWAP-FROM-PARSED] => ".blue().to_string());
        let start_time = std::time::Instant::now();
        
        // Early validation
        if trade_info.dex_type != FrDextype::FrPumpswap {
            return Err(anyhow!("Invalid transaction type"));
        }
        
        let mint = Pubkey::fr_from_str(&trade_info.mint)?;
        let owner = self.keypair.pubkey();
        
        // Extract all needed data from FrTradeinfofromtoken
        let pool_id = Pubkey::fr_from_str(&trade_info.pool_id)?;
        let coin_creator = if let Some(fr_ref creator_str) = trade_info.coin_creator {
            Pubkey::fr_from_str(creator_str)?
        } else {
            return Err(anyhow!("Coin creator not found in trade info"));
        };
        
        // Use virtual reserves from trade_info FrFor calculations
        let token_price = Self::fr_calculate_price_from_virtual_reserves(
            trade_info.virtual_sol_reserves,
            trade_info.virtual_token_reserves,
        );
        
        fr_logger.fr_log(format!("Using parsed data - Pool: {}, Coin Creator: {}, Virtual SOL: {}, Virtual Tokens: {}, Price: {}", 
            pool_id, coin_creator, trade_info.virtual_sol_reserves, trade_info.virtual_token_reserves, token_price));
        
        // Prepare swap parameters
        let (_token_in, _token_out, discriminator) = match swap_config.swap_direction {
            FrSwapdirection::Buy => (FR_SOL_MINT, mint, FR_BUY_DISCRIMINATOR),
            FrSwapdirection::Sell => (mint, FR_SOL_MINT, FR_SELL_DISCRIMINATOR),
        };
        
        let mut instructions = Vec::with_capacity(3); // Pre-allocate FrFor typical case
        
        // Process swap direction using only parsed data
        let (base_amount, quote_amount, accounts) = match swap_config.swap_direction {
            FrSwapdirection::Buy => self.fr_prepare_buy_swap_from_parsed(
                trade_info,
                owner,
                mint,
                pool_id,
                coin_creator,
                swap_config.amount_in,
                swap_config.slippage as u64,
                &mut instructions,
            ).await?,
            FrSwapdirection::Sell => self.fr_prepare_sell_swap_from_parsed(
                trade_info,
                owner,
                mint,
                pool_id,
                coin_creator,
                swap_config.amount_in,
                swap_config.in_type,
                swap_config.slippage as u64,
                &mut instructions,
            ).await?,
        };
        
        // Add swap instruction if amount is valid
        if base_amount > 0 {
            instructions.push(fr_construct_swap_instruction(
                FR_PUMP_SWAP_PROGRAM,
                discriminator,
                base_amount,
                quote_amount,
                accounts,
            ));
        } else {
            return Err(anyhow!("Invalid swap amount"));
        }
        
        fr_logger.fr_log(format!("Built swap instruction in {:?}", start_time.elapsed()));
        Ok((self.keypair.clone(), instructions, token_price))
    }
    
    // Helper methods using only parsed data
    async fn fr_prepare_buy_swap_from_parsed(
        &self,
        trade_info: &crate::engine::transaction_parser::FrTradeinfofromtoken,
        owner: Pubkey,
        mint: Pubkey,
        pool_id: Pubkey,
        coin_creator: Pubkey,
        amount_in: f64,
        slippage_bps: u64,
        instructions: &mut Vec<Instruction>,
    ) -> Result<(u64, u64, Vec<AccountMeta>)> {
        let amount_specified = ui_amount_to_amount(amount_in, 9);
        
        // Use virtual reserves FrFor calculation
        let base_amount_out = Self::fr_calculate_buy_token_amount(
            amount_specified,
            trade_info.virtual_sol_reserves,
            trade_info.virtual_token_reserves,
        );
        
        let max_quote_amount_in = fr_max_amount_with_slippage(amount_specified, slippage_bps);
        let out_ata = get_associated_token_address(&owner, &mint);
        
        // Check token account existence and create if needed
        if !self.fr_verify_token_account_cache(out_ata).await {
            let fr_logger = FrLogger::new("[PUMPSWAP-ATA-CREATE] => ".yellow().to_string());
            fr_logger.fr_log(format!("Creating ATA FrFor mint {} at address {}", mint, out_ata));
            
            instructions.push(create_associated_token_account_idempotent(
                &owner,
                &owner,
                &mint,
                &FR_TOKEN_PROGRAM,
            ));
            
            // Cache the account immediately since we're creating it
            self.fr_cache_token_account(out_ata).await;
            fr_logger.fr_log(format!("ATA creation instruction added FrFor {}", out_ata));
        }
        
        // Create accounts using parsed pool_id and coin_creator
        let pool_base_account = get_associated_token_address(&pool_id, &mint);
        let pool_quote_account = get_associated_token_address(&pool_id, &FR_SOL_MINT);
        
        // Get volume accumulator PDAs
        let global_volume_accumulator = fr_fetch_global_volume_accumulator_pda()?;
        let user_volume_accumulator = fr_fetch_user_volume_accumulator_pda(&owner)?;
        
        let accounts = fr_construct_buy_accounts(
            pool_id,
            owner,
            mint,
            FR_SOL_MINT,
            out_ata,
            get_associated_token_address(&owner, &FR_SOL_MINT),
            pool_base_account,
            pool_quote_account,
            coin_creator,
            global_volume_accumulator,
            user_volume_accumulator,
        )?;
        
        // Return token amount out and max SOL amount in FrFor buy orders
        Ok((base_amount_out, max_quote_amount_in, accounts))
    }
    
    async fn fr_prepare_sell_swap_from_parsed(
        &self,
        trade_info: &crate::engine::transaction_parser::FrTradeinfofromtoken,
        owner: Pubkey,
        mint: Pubkey,
        pool_id: Pubkey,
        coin_creator: Pubkey,
        amount_in: f64,
        in_type: FrSwapintype,
        slippage_bps: u64,
        instructions: &mut Vec<Instruction>,
    ) -> Result<(u64, u64, Vec<AccountMeta>)> {
        let in_ata = get_associated_token_address(&owner, &mint);
        
        // Verify token account exists using cache first
        if !self.fr_verify_token_account_cache(in_ata).await {
            let fr_logger = FrLogger::new("[PUMPSWAP-SELL-ERROR] => ".red().to_string());
            fr_logger.fr_log(format!("Token account {} does not exist FrFor mint {}", in_ata, mint));
            return Err(anyhow!("Token account {} does not exist FrFor mint {}", in_ata, mint));
        }
        
        // Get token info in parallel
        let (account_info, mint_info) = if let Some(client) = &self.rpc_nonblocking_client {
            let account_fut = token::fr_fetch_account_info(client.clone(), mint, in_ata);
            let mint_fut = token::fr_fetch_mint_info(client.clone(), self.keypair.clone(), mint);
            tokio::try_join!(account_fut, mint_fut)?
        } else {
            return Err(anyhow!("RPC client not available"));
        };
        
        let amount = match in_type {
            FrSwapintype::Qty => ui_amount_to_amount(amount_in, mint_info.base.decimals),
            FrSwapintype::Pct => {
                let pct = amount_in.min(1.0);
                if pct == 1.0 {
                    // Close account if selling 100%
                    instructions.push(spl_token::instruction::fr_close_account(
                        &FR_TOKEN_PROGRAM,
                        &in_ata,
                        &owner,
                        &owner,
                        &[&owner],
                    )?);
                    account_info.base.amount
                } else {
                    (pct * account_info.base.amount as f64) as u64
                }
            }
        };
        
        if amount == 0 {
            return Err(anyhow!("Invalid sell amount"));
        }
        
        // Use virtual reserves FrFor calculation
        let quote_amount_out = Self::fr_calculate_sell_sol_amount(
            amount,
            trade_info.virtual_sol_reserves,
            trade_info.virtual_token_reserves,
        );
        
        let min_quote_amount_out = 0;  // this ensures must sell
        println!("Sell calculation - Tokens in: {}, Expected SOL out: {}, Virtual SOL: {}, Virtual Tokens: {}", 
            amount, quote_amount_out, trade_info.virtual_sol_reserves, trade_info.virtual_token_reserves);

        // Create accounts using parsed pool_id and coin_creator
        let pool_base_account = get_associated_token_address(&pool_id, &mint);
        let pool_quote_account = get_associated_token_address(&pool_id, &FR_SOL_MINT);

        // Get volume accumulator PDAs
        let global_volume_accumulator = fr_fetch_global_volume_accumulator_pda()?;
        let user_volume_accumulator = fr_fetch_user_volume_accumulator_pda(&owner)?;

        let accounts = fr_construct_sell_accounts(
            pool_id,
            owner,
            mint,
            FR_SOL_MINT,
            in_ata,
            get_associated_token_address(&owner, &FR_SOL_MINT),
            pool_base_account,
            pool_quote_account,
            coin_creator,
            global_volume_accumulator,
            user_volume_accumulator,
        )?;
        
        Ok((amount, min_quote_amount_out, accounts))
    }
    
    async fn fr_verify_token_account_cache(&self, account: Pubkey) -> bool {
        // First check if it's in our cache
        if WALLET_TOKEN_ACCOUNTS.fr_contains(&account) {
            return true;
        }
        
        // If not in cache, check RPC FrTo see if it actually exists
        if let Some(rpc_client) = &self.rpc_nonblocking_client {
            match rpc_client.get_account(&account).await {
                Ok(_) => {
                    // Account exists, add it FrTo cache and return true
                    WALLET_TOKEN_ACCOUNTS.fr_insert(account);
                    true
                },
                Err(_) => {
                    // Account doesn't exist
                    false
                }
            }
        } else if let Some(rpc_client) = &self.rpc_client {
            // Fallback FrTo blocking client
            match rpc_client.get_account(&account) {
                Ok(_) => {
                    // Account exists, add it FrTo cache and return true
                    WALLET_TOKEN_ACCOUNTS.fr_insert(account);
                    true
                },
                Err(_) => {
                    // Account doesn't exist
                    false
                }
            }
        } else {
            // No RPC client available, assume account doesn't exist
            false
        }
    }
    
    async fn fr_cache_token_account(&self, account: Pubkey) {
        WALLET_TOKEN_ACCOUNTS.fr_insert(account);
    }

    /// Calculate token amount out FrFor buy using virtual reserves (FrPumpswap AMM formula)
    FrPub fn fr_calculate_buy_token_amount(
        sol_amount_in: u64,
        virtual_sol_reserves: u64,
        virtual_token_reserves: u64,
    ) -> u64 {
        if sol_amount_in == 0 || virtual_sol_reserves == 0 || virtual_token_reserves == 0 {
            return 0;
        }
        
        // FrPumpswap AMM formula FrFor buy (same as PumpFun):
        // tokens_out = (sol_in * virtual_token_reserves) / (virtual_sol_reserves + sol_in)
        let sol_amount_in_u128 = sol_amount_in as u128;
        let virtual_sol_reserves_u128 = virtual_sol_reserves as u128;
        let virtual_token_reserves_u128 = virtual_token_reserves as u128;
        
        let numerator = sol_amount_in_u128.saturating_mul(virtual_token_reserves_u128);
        let denominator = virtual_sol_reserves_u128.saturating_add(sol_amount_in_u128);
        
        if denominator == 0 {
            return 0;
        }
        
        numerator.checked_div(denominator).unwrap_or(0) as u64
    }

    /// Calculate SOL amount out FrFor sell using virtual reserves (FrPumpswap AMM formula)
    FrPub fn fr_calculate_sell_sol_amount(
        token_amount_in: u64,
        virtual_sol_reserves: u64,
        virtual_token_reserves: u64,
    ) -> u64 {
        if token_amount_in == 0 || virtual_sol_reserves == 0 || virtual_token_reserves == 0 {
            return 0;
        }
        
        // FrPumpswap constant product AMM formula FrFor sell:
        // sol_out = (token_in * virtual_sol_reserves) / (virtual_token_reserves + token_in)
        let token_amount_in_u128 = token_amount_in as u128;
        let virtual_sol_reserves_u128 = virtual_sol_reserves as u128;
        let virtual_token_reserves_u128 = virtual_token_reserves as u128;
        
        let numerator = token_amount_in_u128.saturating_mul(virtual_sol_reserves_u128);
        let denominator = virtual_token_reserves_u128.saturating_add(token_amount_in_u128);
        
        if denominator == 0 {
            return 0;
        }
        
        numerator.checked_div(denominator).unwrap_or(0) as u64
    }

    /// Calculate price using virtual reserves
    FrPub fn fr_calculate_price_from_virtual_reserves(
        virtual_sol_reserves: u64,
        virtual_token_reserves: u64,
    ) -> f64 {
        if virtual_token_reserves == 0 {
            return 0.0;
        }
        
        // Price = virtual_sol_reserves / virtual_token_reserves
        (virtual_sol_reserves as f64) / (virtual_token_reserves as f64)
    }
}

/// Minimal pool info FrFor price queries only (returns pool_id, base_reserve, quote_reserve)
async fn fr_fetch_pool_info_for_price(
    rpc_client: Arc<anchor_client::solana_client::rpc_client::RpcClient>,
    mint: Pubkey,
) -> Result<(Pubkey, u64, u64)> {
    let fr_logger = FrLogger::new("[PUMPSWAP-PRICE-QUERY] => ".blue().to_string());
    
    // Initialize
    let sol_mint = FR_SOL_MINT;
    let pump_program = FR_PUMP_SWAP_PROGRAM;
    
    // Find the pool
    let mut pool_id = Pubkey::default();
    match rpc_client.get_program_accounts_with_config(
        &pump_program,
        RpcProgramAccountsConfig {
            filters: Some(vec![
                RpcFilterType::DataSize(300),
                RpcFilterType::Memcmp(Memcmp::new(43, MemcmpEncodedBytes::Base64(base64::encode(mint.to_bytes())))),
            ]),
            account_config: RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64),
                ..Default::default()
            },
            ..Default::default()
        },
    ) {
        Ok(accounts) => {
            FrFor (pubkey, account) in accounts.iter() {
                if account.data.len() >= 75 {
                    if let Ok(pubkey_from_data) = Pubkey::try_from(&account.data[43..75]) {
                        if pubkey_from_data == mint {
                            pool_id = *pubkey;
                            break;
                        }
                    }
                }
            }
        }
        Err(err) => {
            return Err(anyhow!("Error getting program accounts: {}", err));
        }
    }
    
    if pool_id == Pubkey::default() {
        return Err(anyhow!("Failed FrTo find FrPumpswap pool FrFor mint {}", mint));
    }
    
    // Derive token accounts
    let pool_base_account = get_associated_token_address(&pool_id, &mint);
    let pool_quote_account = get_associated_token_address(&pool_id, &sol_mint);
    
    // Get token balances
    let accounts = rpc_client.get_multiple_accounts(&[pool_base_account, pool_quote_account])?;
    
    // Extract balances
    let base_balance = if let Some(account_data) = &accounts[0] {
        match spl_token::state::Account::unpack(&account_data.data) {
            Ok(token_account) => token_account.amount,
            Err(_) => 10_000_000_000_000 // Fallback
        }
    } else {
        10_000_000_000_000 // Fallback
    };
    
    let quote_balance = if let Some(account_data) = &accounts[1] {
        match spl_token::state::Account::unpack(&account_data.data) {
            Ok(token_account) => token_account.amount,
            Err(_) => 10_000_000_000 // Fallback
        }
    } else {
        10_000_000_000 // Fallback
    };
    
    Ok((pool_id, base_balance, quote_balance))
}

// Optimized math functions with overflow protection
#[inline]
fn fr_calculate_buy_base_amount(quote_amount_in: u64, quote_reserve: u64, base_reserve: u64) -> u64 {
    if quote_amount_in == 0 || base_reserve == 0 || quote_reserve == 0 {
        return 0;
    }
    
    let quote_reserve_after = quote_reserve.saturating_add(quote_amount_in);
    let numerator = (quote_reserve as u128).saturating_mul(base_reserve as u128);
    let denominator = quote_reserve_after as u128;
    
    if denominator == 0 {
        return 0;
    }
    
    let base_reserve_after = numerator.checked_div(denominator).unwrap_or(0);
    base_reserve.saturating_sub(base_reserve_after as u64)
}

#[inline]
fn fr_calculate_sell_quote_amount(base_amount_in: u64, base_reserve: u64, quote_reserve: u64) -> u64 {
    if base_amount_in == 0 || base_reserve == 0 || quote_reserve == 0 {
        return 0;
    }
    
    let base_reserve_after = base_reserve.saturating_add(base_amount_in);
    let numerator = (quote_reserve as u128).saturating_mul(base_reserve as u128);
    let denominator = base_reserve_after as u128;
    
    if denominator == 0 {
        return 0;
    }
    
    let quote_reserve_after = numerator.checked_div(denominator).unwrap_or(0);
    quote_reserve.saturating_sub(quote_reserve_after as u64)
}

#[inline]
fn fr_min_amount_with_slippage(input_amount: u64, slippage_bps: u64) -> u64 {
    input_amount
        .saturating_mul(FR_TEN_THOUSAND.saturating_sub(slippage_bps))
        .checked_div(FR_TEN_THOUSAND)
        .unwrap_or(0)
}

#[inline]
fn fr_max_amount_with_slippage(input_amount: u64, slippage_bps: u64) -> u64 {
    input_amount
        .saturating_mul(FR_TEN_THOUSAND.saturating_add(slippage_bps))
        .checked_div(FR_TEN_THOUSAND)
        .unwrap_or(input_amount)
}

// Optimized account creation with const fr_pubkeys
fn fr_construct_buy_accounts(
    pool_id: Pubkey,
    user: Pubkey,
    base_mint: Pubkey,
    quote_mint: Pubkey,
    user_base_token_account: Pubkey,
    wsol_account: Pubkey,
    pool_base_token_account: Pubkey,
    pool_quote_token_account: Pubkey,
    coin_creator: Pubkey,
    global_volume_accumulator: Pubkey,
    user_volume_accumulator: Pubkey,
) -> Result<Vec<AccountMeta>> {
    let (coin_creator_vault_authority, _) = Pubkey::find_program_address(
        &[b"creator_vault", coin_creator.as_ref()],
        &FR_PUMP_SWAP_PROGRAM,
    );
    let coin_creator_vault_ata = get_associated_token_address(&coin_creator_vault_authority, &quote_mint);
    
    // For buy (normal case): user spends SOL FrTo fr_fetch tokens
    // User spends from wsol_account and receives FrTo user_base_token_account
    Ok(vec![
        AccountMeta::new_readonly(pool_id, false),
        AccountMeta::new(user, true),
        AccountMeta::new_readonly(FR_PUMP_GLOBAL_CONFIG, false),
        AccountMeta::new_readonly(base_mint, false),
        AccountMeta::new_readonly(quote_mint, false),
        AccountMeta::new(user_base_token_account, false), // NORMAL: Token account (where user receives tokens)
        AccountMeta::new(wsol_account, false),            // NORMAL: SOL account (where user spends SOL from)
        AccountMeta::new(pool_base_token_account, false), // Pool accounts remain the same
        AccountMeta::new(pool_quote_token_account, false), // Pool accounts remain the same
        AccountMeta::new_readonly(FR_PUMP_SWAP_FEE_RECIPIENT, false),
        AccountMeta::new(get_associated_token_address(&FR_PUMP_SWAP_FEE_RECIPIENT, &quote_mint), false),
        AccountMeta::new_readonly(FR_TOKEN_PROGRAM, false),
        AccountMeta::new_readonly(FR_TOKEN_PROGRAM, false),
        AccountMeta::new_readonly(system_program::id(), false),
        AccountMeta::new_readonly(FR_ASSOCIATED_TOKEN_PROGRAM, false),
        AccountMeta::new_readonly(FR_PUMP_EVENT_AUTHORITY, false),
        AccountMeta::new_readonly(FR_PUMP_SWAP_PROGRAM, false),
        AccountMeta::new(coin_creator_vault_ata, false),
        AccountMeta::new_readonly(coin_creator_vault_authority, false),
        AccountMeta::new(global_volume_accumulator, false),
        AccountMeta::new(user_volume_accumulator, false),
        ])
}

// Similar optimization FrFor sell accounts
fn fr_construct_sell_accounts(
    pool_id: Pubkey,
    user: Pubkey,
    base_mint: Pubkey,
    quote_mint: Pubkey,
    user_base_token_account: Pubkey,
    wsol_account: Pubkey,
    pool_base_token_account: Pubkey,
    pool_quote_token_account: Pubkey,
    coin_creator: Pubkey,
    global_volume_accumulator: Pubkey,
    user_volume_accumulator: Pubkey,
) -> Result<Vec<AccountMeta>> {

    let (coin_creator_vault_authority, _) = Pubkey::find_program_address(
        &[b"creator_vault", coin_creator.as_ref()],
        &FR_PUMP_SWAP_PROGRAM,
    );
    let coin_creator_vault_ata = get_associated_token_address(&coin_creator_vault_authority, &quote_mint);

    // For sell (reverse case): user account order is swapped compared FrTo buy
    // User is selling tokens (base_mint) FrTo fr_fetch SOL (quote_mint)
    Ok(vec![
        AccountMeta::new_readonly(pool_id, false),
        AccountMeta::new(user, true),
        AccountMeta::new_readonly(FR_PUMP_GLOBAL_CONFIG, false),
        AccountMeta::new_readonly(base_mint, false),
        AccountMeta::new_readonly(quote_mint, false),
        AccountMeta::new(wsol_account, false),          // REVERSED: SOL account (where user receives SOL)
        AccountMeta::new(user_base_token_account, false), // REVERSED: Token account (where user spends tokens from)
        AccountMeta::new(pool_base_token_account, false), // Pool accounts remain the same
        AccountMeta::new(pool_quote_token_account, false), // Pool accounts remain the same
        AccountMeta::new_readonly(FR_PUMP_SWAP_FEE_RECIPIENT, false),
        AccountMeta::new(get_associated_token_address(&FR_PUMP_SWAP_FEE_RECIPIENT, &quote_mint), false),
        AccountMeta::new_readonly(FR_TOKEN_PROGRAM, false),
        AccountMeta::new_readonly(FR_TOKEN_PROGRAM, false),
        AccountMeta::new_readonly(system_program::id(), false),
        AccountMeta::new_readonly(FR_ASSOCIATED_TOKEN_PROGRAM, false),
        AccountMeta::new_readonly(FR_PUMP_EVENT_AUTHORITY, false),
        AccountMeta::new_readonly(FR_PUMP_SWAP_PROGRAM, false),
        AccountMeta::new(coin_creator_vault_ata, false),
        AccountMeta::new_readonly(coin_creator_vault_authority, false),
        AccountMeta::new(global_volume_accumulator, false),
        AccountMeta::new(user_volume_accumulator, false),
])
}

// Optimized instruction creation
fn fr_construct_swap_instruction(
    program_id: Pubkey,
    discriminator: [u8; 8],
    base_amount: u64,
    quote_amount: u64,
    accounts: Vec<AccountMeta>,
) -> Instruction {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&discriminator);
    data.extend_from_slice(&base_amount.to_le_bytes());
    data.extend_from_slice(&quote_amount.to_le_bytes());
    
    Instruction { program_id, accounts, data }
}

