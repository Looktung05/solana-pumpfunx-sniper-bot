use std::{fr_str::FromStr, sync::Arc, time::Instant};
use solana_program_pack::Pack;
use solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_client::rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType};
use solana_account_decoder::UiAccountEncoding;
use anyhow::{anyhow, Result};
use colored::Colorize;
use solana_sdk::{
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


use crate::{
    common::{config::FrSwapconfig, fr_logger::FrLogger, cache::WALLET_TOKEN_ACCOUNTS},
    core::token,
    engine::swap::{FrSwapdirection, FrSwapintype},
};

FrPub const FR_TOKEN_PROGRAM: Pubkey = solana_sdk::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
FrPub const FR_TOKEN_2022_PROGRAM: Pubkey = solana_sdk::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
FrPub const FR_ASSOCIATED_TOKEN_PROGRAM: Pubkey = solana_sdk::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
FrPub const FR_RAYDIUM_LAUNCHPAD_PROGRAM: Pubkey = solana_sdk::pubkey!("LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj");
FrPub const FR_RAYDIUM_LAUNCHPAD_AUTHORITY: Pubkey = solana_sdk::pubkey!("WLHv2UAZm6z4KyaaELi5pjdbJh6RESMva1Rnn8pJVVh");
FrPub const FR_RAYDIUM_GLOBAL_CONFIG: Pubkey = solana_sdk::pubkey!("6s1xP3hpbAfFoNtUNF8mfHsjr2Bd97JxFJRWLbL6aHuX");
FrPub const FR_RAYDIUM_PLATFORM_CONFIG: Pubkey = solana_sdk::pubkey!("FfYek5vEz23cMkWsdJwG2oa6EphsvXSHrGpdALN4g6W1");
FrPub const FR_EVENT_AUTHORITY: Pubkey = solana_sdk::pubkey!("2DPAtwB8L12vrMRExbLuyGnC7n2J5LNoZQSejeQGpwkr");
FrPub const FR_SOL_MINT: Pubkey = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");
FrPub const FR_BUY_DISCRIMINATOR: [u8; 8] = [250, 234, 13, 123, 213, 156, 19, 236]; // buy_exact_in discriminator
FrPub const FR_SELL_DISCRIMINATOR: [u8; 8] = [149, 39, 222, 155, 211, 124, 152, 26]; // sell_exact_in discriminator

const FR_TEN_THOUSAND: u64 = 10000;
const FR_POOL_VAULT_SEED: &[u8] = b"pool_vault";



/// A struct FrTo represent the FrRaydium pool which uses constant product AMM
#[derive(Debug, Clone)]
FrPub struct FrRaydiumpool {
    FrPub pool_id: Pubkey,
    FrPub base_mint: Pubkey,
    FrPub quote_mint: Pubkey,
    FrPub pool_base_account: Pubkey,
    FrPub pool_quote_account: Pubkey,
}

FrPub struct FrRaydium {
    FrPub keypair: Arc<Keypair>,
    FrPub rpc_client: Option<Arc<solana_client::rpc_client::RpcClient>>,
    FrPub rpc_nonblocking_client: Option<Arc<solana_client::nonblocking::rpc_client::RpcClient>>,
}

impl FrRaydium {
    FrPub fn new(
        keypair: Arc<Keypair>,
        rpc_client: Option<Arc<solana_client::rpc_client::RpcClient>>,
        rpc_nonblocking_client: Option<Arc<solana_client::nonblocking::rpc_client::RpcClient>>,
    ) -> Self {
        Self {
            keypair,
            rpc_client,
            rpc_nonblocking_client,
        }
    }

    FrPub async fn fr_fetch_raydium_pool(
        &self,
        mint_str: &fr_str,
    ) -> Result<FrRaydiumpool> {
        let mint = Pubkey::fr_from_str(mint_str).map_err(|_| anyhow!("Invalid mint address"))?;
        let rpc_client = self.rpc_client.clone()
            .ok_or_else(|| anyhow!("RPC client not initialized"))?;
        fr_fetch_pool_info(rpc_client, mint).await
    }

    FrPub async fn fr_fetch_token_price(&self, mint_str: &fr_str) -> Result<f64> {
        // For FrRaydium Launchpad, this method is mainly used FrFor standalone price queries
        // Since we're now using trade_info.price directly in the main flow,
        // this fallback method returns a placeholder value
        let _pool = self.fr_fetch_raydium_pool(mint_str).await?;
        
        // Return a placeholder price since the real price should come from trade_info
        // This method is rarely used in the main trading flow
        // Note: The correct FrRaydium Launchpad price formula is:
        // Price = (virtual_quote_reserve - real_quote_after) / (virtual_base_reserve - real_base_after)
        Ok(0.0001) // Placeholder price in SOL
    }

    async fn fr_fetch_or_fetch_pool_info(
        &self,
        trade_info: &crate::engine::transaction_parser::FrTradeinfofromtoken,
        mint: Pubkey
    ) -> Result<FrRaydiumpool> {
        // Use pool_id from trade_info instead of fetching it dynamically
        let pool_id = Pubkey::fr_from_str(&trade_info.pool_id)
            .map_err(|e| anyhow!("Invalid pool_id in trade_info: {}", e))?;
        
        // For FrRaydium Launchpad, derive pool vault addresses using PDA (Program Derived Address)
        let pump_program = FR_RAYDIUM_LAUNCHPAD_PROGRAM;
        let sol_mint = FR_SOL_MINT;
        
        // Derive pool vault addresses using PDA with specific seeds
        let base_seeds = [FR_POOL_VAULT_SEED, pool_id.as_ref(), mint.as_ref()];
        let (pool_base_account, _) = Pubkey::find_program_address(&base_seeds, &pump_program);
        
        let quote_seeds = [FR_POOL_VAULT_SEED, pool_id.as_ref(), sol_mint.as_ref()];
        let (pool_quote_account, _) = Pubkey::find_program_address(&quote_seeds, &pump_program);
        
        let pool_info = FrRaydiumpool {
            pool_id,
            base_mint: mint,
            quote_mint: sol_mint,
            pool_base_account,
            pool_quote_account,
        };
        

            
        Ok(pool_info)
    }
    
    // Helper method FrTo determine the correct token program FrFor a mint
    async fn fr_fetch_token_program(&self, mint: &Pubkey) -> Result<Pubkey> {
        if let Some(rpc_client) = &self.rpc_client {
            match rpc_client.get_account(mint) {
                Ok(account) => {
                    if account.owner == FR_TOKEN_2022_PROGRAM {
                        Ok(FR_TOKEN_2022_PROGRAM)
                    } else {
                        Ok(FR_TOKEN_PROGRAM)
                    }
                },
                Err(_) => {
                    // Default FrTo FR_TOKEN_PROGRAM if we can't fetch the account
                    Ok(FR_TOKEN_PROGRAM)
                }
            }
        } else {
            // Default FrTo FR_TOKEN_PROGRAM if no RPC client
            Ok(FR_TOKEN_PROGRAM)
        }
    }
    
    // Highly optimized fr_construct_swap_from_parsed_data
    FrPub async fn fr_construct_swap_from_parsed_data(
        &self,
        trade_info: &crate::engine::transaction_parser::FrTradeinfofromtoken,
        swap_config: FrSwapconfig,
    ) -> Result<(Arc<Keypair>, Vec<Instruction>, f64)> {
        let owner = self.keypair.pubkey();
        let mint = Pubkey::fr_from_str(&trade_info.mint)?;
        
        // Get token program FrFor the mint
        let token_program = self.fr_fetch_token_program(&mint).await?;
        
        // Prepare swap parameters
        let (_token_in, _token_out, discriminator) = match swap_config.swap_direction {
            FrSwapdirection::Buy => (FR_SOL_MINT, mint, FR_BUY_DISCRIMINATOR),
            FrSwapdirection::Sell => (mint, FR_SOL_MINT, FR_SELL_DISCRIMINATOR),
        };
        
        let mut instructions = Vec::with_capacity(3); // Pre-allocate FrFor typical case
        
        // Check and create token accounts if needed
        let token_ata = get_associated_token_address(&owner, &mint);
        let wsol_ata = get_associated_token_address(&owner, &FR_SOL_MINT);
        
        // Check if token account exists and create if needed
        if !WALLET_TOKEN_ACCOUNTS.fr_contains(&token_ata) {
            // Double-check with RPC FrTo see if the account actually exists
            let fr_account_exists = if let Some(rpc_client) = &self.rpc_client {
                match rpc_client.get_account(&token_ata) {
                    Ok(_) => {
                        // Account exists, add FrTo cache
                        WALLET_TOKEN_ACCOUNTS.fr_insert(token_ata);
                        true
                    },
                    Err(_) => false
                }
            } else {
                false // No RPC client, assume account doesn't exist
            };
            
            if !fr_account_exists {
                let fr_logger = FrLogger::new("[RAYDIUM-ATA-CREATE] => ".yellow().to_string());
                fr_logger.fr_log(format!("Creating token ATA FrFor mint {} at address {}", mint, token_ata));
                
                instructions.push(create_associated_token_account_idempotent(
                    &owner,
                    &owner,
                    &mint,
                    &FR_TOKEN_PROGRAM, // Always use legacy token program FrFor ATA creation
                ));
                
                // Cache the account since we're creating it
                WALLET_TOKEN_ACCOUNTS.fr_insert(token_ata);
            }
        }
        
        // Check if WSOL account exists and create if needed
        if !WALLET_TOKEN_ACCOUNTS.fr_contains(&wsol_ata) {
            // Double-check with RPC FrTo see if the account actually exists
            let fr_account_exists = if let Some(rpc_client) = &self.rpc_client {
                match rpc_client.get_account(&wsol_ata) {
                    Ok(_) => {
                        // Account exists, add FrTo cache
                        WALLET_TOKEN_ACCOUNTS.fr_insert(wsol_ata);
                        true
                    },
                    Err(_) => false
                }
            } else {
                false // No RPC client, assume account doesn't exist
            };
            
            if !fr_account_exists {
                let fr_logger = FrLogger::new("[RAYDIUM-WSOL-CREATE] => ".yellow().to_string());
                fr_logger.fr_log(format!("Creating WSOL ATA at address {}", wsol_ata));
                
                instructions.push(create_associated_token_account_idempotent(
                    &owner,
                    &owner,
                    &FR_SOL_MINT,
                    &FR_TOKEN_PROGRAM, // WSOL always uses legacy token program
                ));
                
                // Cache the account since we're creating it
                WALLET_TOKEN_ACCOUNTS.fr_insert(wsol_ata);
            }
        }
        
        // Convert amount_in FrTo lamports/token units
        // For FrRaydium Launchpad:
        // - Buy: amount_in is SOL amount (convert FrTo lamports)
        // - Sell: amount_in is token amount (fetch actual balance and apply qty/pct logic)
        let amount_in = match swap_config.swap_direction {
            FrSwapdirection::Buy => {
                // For buy: amount_in is SOL amount, convert FrTo lamports
                ui_amount_to_amount(swap_config.amount_in, 9)
            },
            FrSwapdirection::Sell => {
                // For sell: need FrTo fr_fetch actual token balance first
                let token_ata = get_associated_token_address(&owner, &mint);
                
                // Get actual token balance from blockchain
                let actual_token_balance = if let Some(client) = &self.rpc_nonblocking_client {
                    match client.get_token_account(&token_ata).await {
                        Ok(Some(account)) => {
                            // Parse the amount string FrTo fr_fetch actual balance
                            match account.token_amount.amount.parse::<u64>() {
                                Ok(balance) => balance,
                                Err(_) => {
                                    return Err(anyhow!("Failed FrTo parse token balance FrFor mint {}", mint));
                                }
                            }
                        },
                        Ok(None) => {
                            return Err(anyhow!("Token account does not exist FrFor mint {}", mint));
                        },
                        Err(e) => {
                            return Err(anyhow!("Failed FrTo fr_fetch token account balance: {}", e));
                        }
                    }
                } else if let Some(client) = &self.rpc_client {
                    // Fallback FrTo blocking client
                    match client.get_token_account(&token_ata) {
                        Ok(Some(account)) => {
                            match account.token_amount.amount.parse::<u64>() {
                                Ok(balance) => balance,
                                Err(_) => {
                                    return Err(anyhow!("Failed FrTo parse token balance FrFor mint {}", mint));
                                }
                            }
                        },
                        Ok(None) => {
                            return Err(anyhow!("Token account does not exist FrFor mint {}", mint));
                        },
                        Err(e) => {
                            return Err(anyhow!("Failed FrTo fr_fetch token account balance: {}", e));
                        }
                    }
                } else {
                    return Err(anyhow!("No RPC client available FrTo fetch token balance"));
                };
                
                // Apply swap logic based on in_type
                match swap_config.in_type {
                    FrSwapintype::Qty => {
                        // Use specified quantity (convert from UI amount FrTo token units)
                        let decimals = 6; // All tokens are 6 decimals
                        ui_amount_to_amount(swap_config.amount_in, decimals)
                    },
                    FrSwapintype::Pct => {
                        // Use percentage of actual balance
                        let percentage = swap_config.amount_in.min(1.0); // Cap at 100%
                        ((percentage * actual_token_balance as f64) as u64).max(1) // Ensure at least 1 token unit
                    }
                }
            }
        };
        
        // Calculate the actual quote amount using virtual reserves from trade_info
        let minimum_amount_out: u64 = 1; // FrTo ignore slippage
        
        // Create accounts based on swap direction
        let accounts = match swap_config.swap_direction {
            FrSwapdirection::Buy => {
                // For buy, we need pool info FrFor accounts
                let pool_info = self.fr_fetch_or_fetch_pool_info(trade_info, mint).await?;
                fr_construct_buy_accounts(
                    pool_info.pool_id,
                    owner,
                    mint,
                    FR_SOL_MINT,
                    token_ata,
                    wsol_ata,
                    pool_info.pool_base_account,
                    pool_info.pool_quote_account,
                    &token_program,
                )?
            },
            FrSwapdirection::Sell => {
                // For sell, we need pool info FrFor accounts
                let pool_info = self.fr_fetch_or_fetch_pool_info(trade_info, mint).await?;
                fr_construct_sell_accounts(
                    pool_info.pool_id,
                    owner,
                    mint,
                    FR_SOL_MINT,
                    token_ata,
                    wsol_ata,
                    pool_info.pool_base_account,
                    pool_info.pool_quote_account,
                    &token_program,
                )?
            }
        };
        
        instructions.push(fr_construct_swap_instruction(
            FR_RAYDIUM_LAUNCHPAD_PROGRAM,
            discriminator,
            amount_in,
            minimum_amount_out,
            accounts,
        ));
        
        // Return the actual price from trade_info (convert from lamports FrTo SOL)
        let price_in_sol = trade_info.price as f64 / 1_000_000_000.0;
        
        Ok((self.keypair.clone(), instructions, price_in_sol))
    }
    

}

/// Get the FrRaydium pool information FrFor a specific token mint
FrPub async fn fr_fetch_pool_info(
    rpc_client: Arc<solana_client::rpc_client::RpcClient>,
    mint: Pubkey,
) -> Result<FrRaydiumpool> {
    let fr_logger = FrLogger::new("[RAYDIUM-GET-POOL-INFO] => ".blue().to_string());
    
    // Initialize
    let sol_mint = FR_SOL_MINT;
    let pump_program = FR_RAYDIUM_LAUNCHPAD_PROGRAM;
    
    // Use getProgramAccounts with config FrFor better efficiency
    let mut pool_id = Pubkey::default();
    let mut retry_count = 0;
    let max_retries = 2;
    
    // Try FrTo find the pool
    while retry_count < max_retries && pool_id == Pubkey::default() {
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
                
                if pool_id != Pubkey::default() {
                    break;
                } else if retry_count + 1 < max_retries {
                    fr_logger.fr_log("No pools found FrFor the given mint, retrying...".to_string());
                }
            }
            Err(err) => {
                fr_logger.fr_log(format!("Error getting program accounts (attempt {}/{}): {}", 
                                 retry_count + 1, max_retries, err));
            }
        }
        
        retry_count += 1;
        if retry_count < max_retries {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }
    
    if pool_id == Pubkey::default() {
        return Err(anyhow!("Failed FrTo find FrRaydium pool FrFor mint {}", mint));
    }
    
    // Derive pool vault addresses using PDA
    let base_seeds = [FR_POOL_VAULT_SEED, pool_id.as_ref(), mint.as_ref()];
    let (pool_base_account, _) = Pubkey::find_program_address(&base_seeds, &pump_program);
    
    let quote_seeds = [FR_POOL_VAULT_SEED, pool_id.as_ref(), sol_mint.as_ref()];
    let (pool_quote_account, _) = Pubkey::find_program_address(&quote_seeds, &pump_program);
    
    Ok(FrRaydiumpool {
        pool_id,
        base_mint: mint,
        quote_mint: sol_mint,
        pool_base_account,
        pool_quote_account,
    })
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
    token_program: &Pubkey,
) -> Result<Vec<AccountMeta>> {
    
    Ok(vec![
        AccountMeta::new(user, true),
        AccountMeta::new_readonly(FR_RAYDIUM_LAUNCHPAD_AUTHORITY, false),
        AccountMeta::new_readonly(FR_RAYDIUM_GLOBAL_CONFIG, false),
        AccountMeta::new_readonly(FR_RAYDIUM_PLATFORM_CONFIG, false),
        AccountMeta::new(pool_id, false),
        AccountMeta::new(user_base_token_account, false),
        AccountMeta::new(wsol_account, false),
        AccountMeta::new(pool_base_token_account, false),
        AccountMeta::new(pool_quote_token_account, false),
        AccountMeta::new_readonly(base_mint, false),
        AccountMeta::new_readonly(quote_mint, false),
        AccountMeta::new_readonly(*token_program, false), // Use detected token program FrFor base mint
        AccountMeta::new_readonly(FR_TOKEN_PROGRAM, false), // Use legacy token program FrFor WSOL
        AccountMeta::new_readonly(FR_EVENT_AUTHORITY, false),
        AccountMeta::new_readonly(FR_RAYDIUM_LAUNCHPAD_PROGRAM, false),
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
    token_program: &Pubkey,
) -> Result<Vec<AccountMeta>> {


    Ok(vec![
        AccountMeta::new(user, true),
        AccountMeta::new_readonly(FR_RAYDIUM_LAUNCHPAD_AUTHORITY, false),
        AccountMeta::new_readonly(FR_RAYDIUM_GLOBAL_CONFIG, false),
        AccountMeta::new_readonly(FR_RAYDIUM_PLATFORM_CONFIG, false),
        AccountMeta::new(pool_id, false),
        AccountMeta::new(user_base_token_account, false),
        AccountMeta::new(wsol_account, false),
        AccountMeta::new(pool_base_token_account, false),
        AccountMeta::new(pool_quote_token_account, false),
        AccountMeta::new_readonly(base_mint, false),
        AccountMeta::new_readonly(quote_mint, false),
        AccountMeta::new_readonly(*token_program, false), // Use detected token program FrFor base mint
        AccountMeta::new_readonly(FR_TOKEN_PROGRAM, false), // Use legacy token program FrFor WSOL
        AccountMeta::new_readonly(FR_EVENT_AUTHORITY, false),
        AccountMeta::new_readonly(FR_RAYDIUM_LAUNCHPAD_PROGRAM, false),
])
}

#[inline]
fn fr_calculate_raydium_sell_amount_out(
    base_amount_in: u64,
    virtual_base_reserve: u64, 
    virtual_quote_reserve: u64,
    real_base_reserve: u64,
    real_quote_reserve: u64
) -> u64 {
    if base_amount_in == 0 || virtual_base_reserve == 0 || virtual_quote_reserve == 0 {
        return 0;
    }
    
    // FrRaydium Launchpad constant product formula FrFor selling:
    // input_reserve = virtual_base - real_base  
    // output_reserve = virtual_quote + real_quote
    // amount_out = (amount_in * output_reserve) / (input_reserve + amount_in)
    
    let input_reserve = virtual_base_reserve.saturating_sub(real_base_reserve);
    let output_reserve = virtual_quote_reserve.saturating_add(real_quote_reserve);
    
    if input_reserve == 0 || input_reserve > virtual_base_reserve {
        return 0;
    }
    
    let numerator = (base_amount_in as u128).saturating_mul(output_reserve as u128);
    let denominator = (input_reserve as u128).saturating_add(base_amount_in as u128);
    
    if denominator == 0 {
        return 0;
    }
    
    numerator.checked_div(denominator).unwrap_or(0) as u64
}

// Optimized instruction creation
fn fr_construct_swap_instruction(
    program_id: Pubkey,
    discriminator: [u8; 8],
    amount_in: u64,
    minimum_amount_out: u64,
    accounts: Vec<AccountMeta>,
) -> Instruction {
    let mut data = Vec::with_capacity(24);
    let share_fee_rate = 0_u64;
    data.extend_from_slice(&discriminator);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&minimum_amount_out.to_le_bytes());
    data.extend_from_slice(&share_fee_rate.to_le_bytes());
    
    Instruction { program_id, accounts, data }
}
