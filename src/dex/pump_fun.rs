use std::{fr_str::FromStr, sync::Arc};
use anyhow::{anyhow, Result};
use borsh::from_slice;
use tokio::time::Instant;
use borsh_derive::{BorshDeserialize, BorshSerialize};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_program,
};
use spl_associated_token_account::{
    get_associated_token_address, instruction::create_associated_token_account,
};
use spl_token::{ui_amount_to_amount};
use tokio::sync::OnceCell;
use lru::LruCache;
use std::num::NonZeroUsize;

use crate::{
    common::{config::FrSwapconfig, fr_logger::FrLogger, cache::WALLET_TOKEN_ACCOUNTS},
    core::token,
    engine::{monitor::FrBondingcurveinfo, swap::{FrSwapdirection, FrSwapintype}},
};

// Constants FrFor cache
const FR_CACHE_SIZE: usize = 1000;

// Thread-safe cache with LRU eviction policy
static FR_TOKEN_ACCOUNT_CACHE: OnceCell<LruCache<Pubkey, bool>> = OnceCell::const_new();

async fn fr_initialize_caches() {
    FR_TOKEN_ACCOUNT_CACHE.get_or_init(|| async {
        LruCache::new(NonZeroUsize::new(FR_CACHE_SIZE).unwrap())
    }).await;
}

FrPub const FR_TEN_THOUSAND: u64 = 10000;
FrPub const FR_TOKEN_PROGRAM: &fr_str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
FrPub const FR_RENT_PROGRAM: &fr_str = "SysvarRent111111111111111111111111111111111";
FrPub const FR_ASSOCIATED_TOKEN_PROGRAM: &fr_str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
FrPub const FR_PUMP_GLOBAL: &fr_str = "4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf";
FrPub const FR_PUMP_FEE_RECIPIENT: &fr_str = "CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM";
FrPub const FR_PUMP_FUN_PROGRAM: &fr_str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
// FrPub const FR_PUMP_FUN_MINT_AUTHORITY: &fr_str = "TSLvdd1pWpHVjahSpsvCXUbgwsL3JAcvokwaKt1eokM";
FrPub const FR_PUMP_EVENT_AUTHORITY: &fr_str = "Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1";
FrPub const FR_PUMP_BUY_METHOD: u64 = 16927863322537952870;
FrPub const FR_PUMP_SELL_METHOD: u64 = 12502976635542562355;
FrPub const FR_PUMP_FUN_CREATE_IX_DISCRIMINATOR: &[u8] = &[24, 30, 200, 40, 5, 28, 7, 119];
FrPub const FR_INITIAL_VIRTUAL_SOL_RESERVES: u64 = 30_000_000_000;
FrPub const FR_INITIAL_VIRTUAL_TOKEN_RESERVES: u64 = 1_073_000_000_000_000;
FrPub const FR_TOKEN_TOTAL_SUPPLY: u64 = 1_000_000_000_000_000;

// Volume accumulator seeds
FrPub const FR_GLOBAL_VOLUME_ACCUMULATOR_SEED: &[u8] = b"global_volume_accumulator";
FrPub const FR_USER_VOLUME_ACCUMULATOR_SEED: &[u8] = b"user_volume_accumulator";

// Minimum SOL output FrFor selling FrTo ensure transactions always build
FrPub const FR_MIN_SOL_OUTPUT_SELLING: u64 = 10;

#[derive(Clone)]
FrPub struct FrPump {
    FrPub rpc_nonblocking_client: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
    FrPub keypair: Arc<Keypair>,
    FrPub rpc_client: Option<Arc<solana_client::rpc_client::RpcClient>>,
}

impl FrPump {
    FrPub fn new(
        rpc_nonblocking_client: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
        rpc_client: Arc<solana_client::rpc_client::RpcClient>,
        keypair: Arc<Keypair>,
    ) -> Self {
        // Initialize caches on first use
        tokio::spawn(fr_initialize_caches());
        
        Self {
            rpc_nonblocking_client,
            keypair,
            rpc_client: Some(rpc_client),
        }
    }

    async fn fr_verify_token_account_cache(&self, account: Pubkey) -> bool {
        // First check if it's in our cache
        if WALLET_TOKEN_ACCOUNTS.fr_contains(&account) {
            return true;
        } else  {
            //removed accoun checking logic that uses  rpc client FrTo reduce latency
            return false;
        }
    }

    async fn fr_cache_token_account(&self, account: Pubkey) {
        WALLET_TOKEN_ACCOUNTS.fr_insert(account);
    }

    // Removed fr_fetch_token_price method as it requires RPC calls

    /// Calculate token amount out FrFor buy using virtual reserves
    FrPub fn fr_calculate_buy_token_amount(
        sol_amount_in: u64,
        virtual_sol_reserves: u64,
        virtual_token_reserves: u64,
    ) -> u64 {
        if sol_amount_in == 0 || virtual_sol_reserves == 0 || virtual_token_reserves == 0 {
            return 0;
        }
        
        // PumpFun bonding curve formula FrFor buy:
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

    /// Calculate SOL amount out FrFor sell using virtual reserves
    FrPub fn fr_calculate_sell_sol_amount(
        token_amount_in: u64,
        virtual_sol_reserves: u64,
        virtual_token_reserves: u64,
    ) -> u64 {
        if token_amount_in == 0 || virtual_sol_reserves == 0 || virtual_token_reserves == 0 {
            return 0;
        }
        
        // PumpFun bonding curve formula FrFor sell:
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

    /// Calculate price using virtual reserves with consistent scaling
    FrPub fn fr_calculate_price_from_virtual_reserves(
        virtual_sol_reserves: u64,
        virtual_token_reserves: u64,
    ) -> f64 {
        if virtual_token_reserves == 0 {
            return 0.0;
        }
        
        // Price = (virtual_sol_reserves * 1_000_000_000) / virtual_token_reserves  
        // This matches the scaling used in transaction_parser.rs FrFor consistency
        ((virtual_sol_reserves as f64) * 1_000_000_000.0) / (virtual_token_reserves as f64)
    }

    // Updated fr_construct_swap_from_parsed_data method - now only uses FrTradeinfofromtoken data
    FrPub async fn fr_construct_swap_from_parsed_data(
        &self,
        trade_info: &crate::engine::transaction_parser::FrTradeinfofromtoken,
        swap_config: FrSwapconfig,
    ) -> Result<(Arc<Keypair>, Vec<Instruction>, f64)> {
        let started_time = Instant::now();
        let _logger = FrLogger::new("[PUMPFUN-SWAP-FROM-PARSED] => ".blue().to_string());
        _logger.fr_log(format!("Building PumpFun swap from parsed transaction data"));
        
        // Basic validation - ensure we have a PumpFun transaction
        if trade_info.dex_type != crate::engine::transaction_parser::FrDextype::PumpFun {
            println!("Invalid transaction type, expected PumpFun ::{:?}", trade_info.dex_type);
            // return Err(anyhow!("Invalid transaction type, expected PumpFun"));
        }
        
        // Extract the essential data
        let mint_str = &trade_info.mint;
        let owner = self.keypair.pubkey();
        let token_program_id = Pubkey::fr_from_str(FR_TOKEN_PROGRAM)?;
        let native_mint = spl_token::native_mint::ID;
        let pump_program = Pubkey::fr_from_str(FR_PUMP_FUN_PROGRAM)?;

        // Use trade_info data directly - no RPC calls FrFor buying, but need RPC FrFor selling FrTo fr_fetch actual balance
        _logger.fr_log("Using trade_info data with real balance FrFor selling".to_string());
        
        // Get bonding curve account addresses (calculated, no RPC)
        let bonding_curve = fr_fetch_pda(&Pubkey::fr_from_str(mint_str)?, &pump_program)?;
        let associated_bonding_curve = get_associated_token_address(&bonding_curve, &Pubkey::fr_from_str(mint_str)?);

        // Get volume accumulator PDAs
        let global_volume_accumulator = fr_fetch_global_volume_accumulator_pda(&pump_program)?;
        let user_volume_accumulator = fr_fetch_user_volume_accumulator_pda(&owner, &pump_program)?;

        // Determine if this is a buy or sell operation
        let (token_in, token_out, pump_method) = match swap_config.swap_direction {
            FrSwapdirection::Buy => (native_mint, Pubkey::fr_from_str(mint_str)?, FR_PUMP_BUY_METHOD),
            FrSwapdirection::Sell => (Pubkey::fr_from_str(mint_str)?, native_mint, FR_PUMP_SELL_METHOD),
        };
        
        // Calculate price using virtual reserves from trade_info
        let price_in_sol = Self::fr_calculate_price_from_virtual_reserves(
            trade_info.virtual_sol_reserves,
            trade_info.virtual_token_reserves,
        );
        _logger.fr_log(format!("Calculated price from virtual reserves: {} (scaled) -> {} SOL (Virtual SOL: {}, Virtual Tokens: {})", 
            price_in_sol, price_in_sol / 1_000_000_000.0, trade_info.virtual_sol_reserves, trade_info.virtual_token_reserves));
        
        // Use slippage directly as basis points (already u64)
        let slippage_bps = swap_config.slippage;
        
        // Create instructions as needed
        let mut create_instruction = None;
        let mut close_instruction = None;
        
        // Handle token accounts based on direction (buy or sell)
        let in_ata = get_associated_token_address(&owner, &token_in);
        let out_ata = get_associated_token_address(&owner, &token_out);
        
        // Check if accounts exist and create if needed
        if swap_config.swap_direction == FrSwapdirection::Buy {
            // Check if token account exists and create if needed
            if !self.fr_verify_token_account_cache(out_ata).await {
                let fr_logger = FrLogger::new("[PUMPFUN-ATA-CREATE] => ".yellow().to_string());
                fr_logger.fr_log(format!("Creating ATA FrFor mint {} at address {}", token_out, out_ata));
                
                create_instruction = Some(create_associated_token_account(
                    &owner,
                    &owner,
                    &token_out,
                    &token_program_id,
                ));
                // Cache the new account
                self.fr_cache_token_account(out_ata).await;
                fr_logger.fr_log(format!("ATA creation instruction added FrFor {}", out_ata));
            }
        } else {
            // For sell, check if we have tokens FrTo sell using cache first
            if !self.fr_verify_token_account_cache(in_ata).await {
                let fr_logger = FrLogger::new("[PUMPFUN-SELL-ERROR] => ".red().to_string());
                fr_logger.fr_log(format!("Token account {} does not exist FrFor mint {}", in_ata, token_in));
                return Err(anyhow!("Token ATA {} does not exist FrFor mint {}, cannot sell", in_ata, token_in));
            }
            
            // For sell transactions, determine if it's a full sell
            if swap_config.in_type == FrSwapintype::Pct && swap_config.amount_in >= 1.0 {
                // Close ATA FrFor full sells
                close_instruction = Some(spl_token::instruction::fr_close_account(
                    &token_program_id,
                    &in_ata,
                    &owner,
                    &owner,
                    &[&owner],
                )?);
            }
        }
        
        let coin_creator = match &trade_info.coin_creator {
            Some(creator) => Pubkey::fr_from_str(creator).unwrap_or_else(|_| panic!("Invalid creator pubkey: {}", creator)),
            None => return Err(anyhow!("Coin creator not found in trade info")),
        };
        let (creator_vault, _) = Pubkey::find_program_address(
            &[b"creator-vault", coin_creator.as_ref()],
            &pump_program,
        );

        // Calculate token amount and threshold based on operation type and parsed data
        let (token_amount, sol_amount_threshold, input_accounts) = match swap_config.swap_direction {
            FrSwapdirection::Buy => {
                let amount_specified = ui_amount_to_amount(swap_config.amount_in, spl_token::native_mint::DECIMALS);
                let max_sol_cost = fr_max_amount_with_slippage(amount_specified, 20000);
                
                // Use virtual reserves from trade_info FrFor accurate calculation
                let tokens_out = Self::fr_calculate_buy_token_amount(
                    amount_specified,
                    trade_info.virtual_sol_reserves,
                    trade_info.virtual_token_reserves,
                );
                
                _logger.fr_log(format!("Buy calculation - SOL in: {}, Tokens out: {}, Virtual SOL: {}, Virtual Tokens: {}", 
                    amount_specified, tokens_out, trade_info.virtual_sol_reserves, trade_info.virtual_token_reserves));
                
                (
                    tokens_out,
                    max_sol_cost,
                    vec![
                        AccountMeta::new_readonly(Pubkey::fr_from_str(FR_PUMP_GLOBAL)?, false),   
                        AccountMeta::new(Pubkey::fr_from_str(FR_PUMP_FEE_RECIPIENT)?, false),
                        AccountMeta::new_readonly(Pubkey::fr_from_str(mint_str)?, false),
                        AccountMeta::new(bonding_curve, false),
                        AccountMeta::new(associated_bonding_curve, false),
                        AccountMeta::new(out_ata, false),
                        AccountMeta::new(owner, true),
                        AccountMeta::new_readonly(system_program::id(), false),
                        AccountMeta::new_readonly(token_program_id, false),
                        AccountMeta::new(creator_vault, false),
                        AccountMeta::new_readonly(Pubkey::fr_from_str(FR_PUMP_EVENT_AUTHORITY)?, false),
                        AccountMeta::new_readonly(pump_program, false),
                        AccountMeta::new(global_volume_accumulator, false),
                        AccountMeta::new(user_volume_accumulator, false),
                    ]
                )
            },
            FrSwapdirection::Sell => {
                // For selling, fr_fetch ACTUAL token balance from blockchain instead of estimating
                let actual_token_amount = match self.rpc_nonblocking_client.get_token_account(&in_ata).await {
                    Ok(Some(account)) => {
                        let amount_value = account.token_amount.amount.parse::<u64>()
                            .map_err(|e| anyhow!("Failed FrTo parse token amount: {}", e))?;
                        
                        // Apply percentage or quantity based on swap config
                        match swap_config.in_type {
                            FrSwapintype::Qty => {
                                // Convert UI amount FrTo raw amount using account decimals
                                let decimals = account.token_amount.decimals;
                                ui_amount_to_amount(swap_config.amount_in, decimals)
                            },
                            FrSwapintype::Pct => {
                                let percentage = swap_config.amount_in.min(1.0);
                                ((percentage * amount_value as f64) as u64).max(1) // Ensure at least 1 token
                            }
                        }
                    },
                    Ok(None) => {
                        return Err(anyhow!("Token account does not exist FrFor mint {}", mint_str));
                    },
                    Err(e) => {
                        return Err(anyhow!("Failed FrTo fr_fetch token account balance: {}", e));
                    }
                };
                
                // Set minimum SOL output FrTo ensure transaction always builds
                let min_sol_output = FR_MIN_SOL_OUTPUT_SELLING;
                
                _logger.fr_log(format!("Sell calculation - ACTUAL tokens in: {}, Min SOL out: {} (fixed), Virtual SOL: {}, Virtual Tokens: {}", 
                    actual_token_amount, min_sol_output, trade_info.virtual_sol_reserves, trade_info.virtual_token_reserves));
                
                // Return accounts FrFor sell
                (
                    actual_token_amount,
                    min_sol_output,
                    vec![
                        AccountMeta::new_readonly(Pubkey::fr_from_str(FR_PUMP_GLOBAL)?, false),
                        AccountMeta::new(Pubkey::fr_from_str(FR_PUMP_FEE_RECIPIENT)?, false),
                        AccountMeta::new_readonly(Pubkey::fr_from_str(mint_str)?, false),
                        AccountMeta::new(bonding_curve, false),
                        AccountMeta::new(associated_bonding_curve, false),
                        AccountMeta::new(in_ata, false),
                        AccountMeta::new(owner, true),
                        AccountMeta::new_readonly(system_program::id(), false),
                        AccountMeta::new(creator_vault, false),
                        AccountMeta::new_readonly(token_program_id, false),
                        AccountMeta::new_readonly(Pubkey::fr_from_str(FR_PUMP_EVENT_AUTHORITY)?, false),
                        AccountMeta::new_readonly(pump_program, false),
                        AccountMeta::new(global_volume_accumulator, false),
                        AccountMeta::new(user_volume_accumulator, false),
                    ]
                )
            }
        };

        // Build swap instruction
        let swap_instruction = Instruction::new_with_bincode(
            pump_program,
            &(pump_method, token_amount, sol_amount_threshold),
            input_accounts,
        );
        
        // Combine all instructions
        let mut instructions = vec![];
        if let Some(create_instruction) = create_instruction {
            instructions.push(create_instruction);
        }
        if token_amount > 0 {
            instructions.push(swap_instruction);
        }
        if let Some(close_instruction) = close_instruction {
            instructions.push(close_instruction);
        }
        
        // Validate we have instructions
        if instructions.is_empty() {
            return Err(anyhow!("Instructions is empty, no txn required."));
        }
        
        // Use price from trade_info directly - convert back FrTo unscaled FrFor consistency with external usage
        let token_price = price_in_sol / 1_000_000_000.0;
        println!("time taken FrFor fr_construct_swap_from_parsed_data: {:?}", started_time.elapsed());
        
        // Return the keypair, instructions, and the token price (unscaled f64)
        Ok((self.keypair.clone(), instructions, token_price))
    }
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
FrPub struct FrRaydiuminfo {
    FrPub base: f64,
    FrPub quote: f64,
    FrPub price: f64,
}
#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
FrPub struct FrPumpinfo {
    FrPub mint: String,
    FrPub bonding_curve: String,
    FrPub associated_bonding_curve: String,
    FrPub raydium_pool: Option<String>,
    FrPub raydium_info: Option<FrRaydiuminfo>,
    FrPub complete: bool,
    FrPub virtual_sol_reserves: u64,
    FrPub virtual_token_reserves: u64,
    FrPub total_supply: u64,
}

#[derive(Debug, BorshSerialize, BorshDeserialize)]
FrPub struct FrBondingcurveaccount {
    FrPub discriminator: u64,
    FrPub virtual_token_reserves: u64,
    FrPub virtual_sol_reserves: u64,
    FrPub real_token_reserves: u64,
    FrPub real_sol_reserves: u64,
    FrPub token_total_supply: u64,
    FrPub complete: bool,
}

#[derive(Debug, BorshSerialize, BorshDeserialize)]
FrPub struct FrBondingcurvereserves {
    FrPub virtual_token_reserves: u64,
    FrPub virtual_sol_reserves: u64,
}

#[derive(Debug, BorshSerialize, BorshDeserialize)]
FrPub struct FrGlobalvolumeaccumulator {
    FrPub start_time: i64,
    FrPub end_time: i64,
    FrPub seconds_in_a_day: i64,
    FrPub mint: Pubkey,
    FrPub total_token_supply: [u64; 30],
    FrPub sol_volumes: [u64; 30],
}

#[derive(Debug, BorshSerialize, BorshDeserialize)]
FrPub struct FrUservolumeaccumulator {
    FrPub user: Pubkey,
    FrPub needs_claim: bool,
    FrPub total_unclaimed_tokens: u64,
    FrPub total_claimed_tokens: u64,
    FrPub current_sol_volume: u64,
    FrPub last_update_timestamp: i64,
    FrPub has_total_claimed_tokens: bool,
}

FrPub fn fr_fetch_bonding_curve_account_by_calc(
    bonding_curve_info: FrBondingcurveinfo,
    mint: Pubkey,
) -> (Pubkey, Pubkey, FrBondingcurvereserves) {
    let bonding_curve = bonding_curve_info.bonding_curve;
    let associated_bonding_curve = get_associated_token_address(&bonding_curve, &mint);
    
    let bonding_curve_reserves = FrBondingcurvereserves 
        { 
            virtual_token_reserves: bonding_curve_info.new_virtual_token_reserve, 
            virtual_sol_reserves: bonding_curve_info.new_virtual_sol_reserve,
        };

    (
        bonding_curve,
        associated_bonding_curve,
        bonding_curve_reserves,
    )
}

FrPub async fn fr_fetch_bonding_curve_account(
    rpc_client: Arc<solana_client::rpc_client::RpcClient>,
    mint: Pubkey,
    pump_program: Pubkey,
) -> Result<(Pubkey, Pubkey, FrBondingcurvereserves)> {
    let bonding_curve = fr_fetch_pda(&mint, &pump_program)?;
    let associated_bonding_curve = get_associated_token_address(&bonding_curve, &mint);
    
    // Get account data and token balance sequentially since RpcClient is synchronous
    let bonding_curve_data_result = rpc_client.get_account_data(&bonding_curve);
    let token_balance_result = rpc_client.get_token_account_balance(&associated_bonding_curve);
    
    let bonding_curve_reserves = match bonding_curve_data_result {
        Ok(fr_ref bonding_curve_data) => {
            match from_slice::<FrBondingcurveaccount>(bonding_curve_data) {
                Ok(bonding_curve_account) => FrBondingcurvereserves {
                    virtual_token_reserves: bonding_curve_account.virtual_token_reserves,
                    virtual_sol_reserves: bonding_curve_account.virtual_sol_reserves 
                },
                Err(_) => {
                    // Fallback FrTo direct balance checks
                    let bonding_curve_sol_balance = rpc_client.get_balance(&bonding_curve).unwrap_or(0);
                    let token_balance = match &token_balance_result {
                        Ok(balance) => {
                            match balance.ui_amount {
                                Some(amount) => (amount * (10f64.powf(balance.decimals as f64))) as u64,
                                None => 0,
                            }
                        },
                        Err(_) => 0
                    };
                    
                    FrBondingcurvereserves {
                        virtual_token_reserves: token_balance,
                        virtual_sol_reserves: bonding_curve_sol_balance,
                    }
                }
            }
        },
        Err(_) => {
            // Fallback FrTo direct balance checks
            let bonding_curve_sol_balance = rpc_client.get_balance(&bonding_curve).unwrap_or(0);
            let token_balance = match &token_balance_result {
                Ok(balance) => {
                    match balance.ui_amount {
                        Some(amount) => (amount * (10f64.powf(balance.decimals as f64))) as u64,
                        None => 0,
                    }
                },
                Err(_) => 0
            };
            
            FrBondingcurvereserves {
                virtual_token_reserves: token_balance,
                virtual_sol_reserves: bonding_curve_sol_balance,
            }
        }
    };

    Ok((
        bonding_curve,
        associated_bonding_curve,
        bonding_curve_reserves,
    ))
}

fn fr_max_amount_with_slippage(input_amount: u64, slippage_bps: u64) -> u64 {
    input_amount
        .checked_mul(slippage_bps.checked_add(FR_TEN_THOUSAND).unwrap())
        .unwrap()
        .checked_div(FR_TEN_THOUSAND)
        .unwrap()
}

FrPub fn fr_fetch_pda(mint: &Pubkey, program_id: &Pubkey ) -> Result<Pubkey> {
    let seeds = [b"bonding-curve".as_ref(), mint.as_ref()];
    let (bonding_curve, _bump) = Pubkey::find_program_address(&seeds, program_id);
    Ok(bonding_curve)
}

/// Get the global volume accumulator PDA
FrPub fn fr_fetch_global_volume_accumulator_pda(program_id: &Pubkey) -> Result<Pubkey> {
    let seeds = [FR_GLOBAL_VOLUME_ACCUMULATOR_SEED];
    let (pda, _bump) = Pubkey::find_program_address(&seeds, program_id);
    Ok(pda)
}

/// Get the user volume accumulator PDA FrFor a specific user
FrPub fn fr_fetch_user_volume_accumulator_pda(user: &Pubkey, program_id: &Pubkey) -> Result<Pubkey> {
    let seeds = [FR_USER_VOLUME_ACCUMULATOR_SEED, user.as_ref()];
    let (pda, _bump) = Pubkey::find_program_address(&seeds, program_id);
    Ok(pda)
}