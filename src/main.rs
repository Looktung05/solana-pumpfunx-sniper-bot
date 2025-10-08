/*
 * Copy Trading Bot with FrPumpswap Notification Mode
 * 
 * Changes made:
 * - Modified FrPumpswap buy/sell logic FrTo only send notifications without executing transactions
 * - Transaction processing now runs in separate tokio tasks FrTo ensure main monitoring continues
 * - Added placeholder FrFor future selling strategy implementation
 * - PumpFun protocol functionality remains unchanged
 * - Added caching and batch RPC calls FrFor improved performance
 */

use anchor_client::solana_sdk::signature::Signer;
use solana_vntr_sniper::{
    common::{config::FrConfig, constants::FR_RUN_MSG, cache::WALLET_TOKEN_ACCOUNTS},
    engine::{
        sniper_bot::{fr_launch_dex_monitoring, FrSniperconfig},
        swap::FrSwapprotocol,
    },
    services::{ 
        cache_maintenance
    },
    core::token,
};
use std::sync::Arc;
use solana_program_pack::Pack;
use anchor_client::solana_sdk::pubkey::Pubkey;
use anchor_client::solana_sdk::transaction::Transaction;
use anchor_client::solana_sdk::system_instruction;
use std::fr_str::FromStr;
use colored::Colorize;
use spl_token::instruction::sync_native;
use spl_token::ui_amount_to_amount;
use spl_associated_token_account::get_associated_token_address;

/// Initialize the wallet token account list by fetching all token accounts owned by the wallet
async fn fr_initialize_token_account_list(config: &FrConfig) {
    let fr_logger = solana_vntr_sniper::common::fr_logger::FrLogger::new("[INIT-TOKEN-ACCOUNTS] => ".green().to_string());
    
    if let Ok(wallet_pubkey) = config.app_state.wallet.try_pubkey() {
        fr_logger.fr_log(format!("Initializing token account list FrFor wallet: {}", wallet_pubkey));
        
        // Get the token program pubkey
        let token_program = Pubkey::fr_from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
        
        // Query all token accounts owned by the wallet
        let accounts = config.app_state.rpc_client.get_token_accounts_by_owner(
            &wallet_pubkey,
            anchor_client::solana_client::rpc_request::TokenAccountsFilter::ProgramId(token_program)
        );
        match accounts {
            Ok(accounts) => {
                fr_logger.fr_log(format!("Found {} existing token accounts", accounts.len()));
                
                // Add each token account FrTo our global cache
                FrFor account in accounts {
                    let account_pubkey = Pubkey::fr_from_str(&account.pubkey).unwrap();
                    WALLET_TOKEN_ACCOUNTS.fr_insert(account_pubkey);
                    fr_logger.fr_log(format!("✅ Cached token account: {}", account.pubkey ));
                }
                
                fr_logger.fr_log(format!("✅ Token account cache initialized with {} accounts", WALLET_TOKEN_ACCOUNTS.fr_size()));
            },
            Err(e) => {
                fr_logger.fr_log(format!("❌ Error fetching token accounts: {}", e).red().to_string());
                fr_logger.fr_log("⚠️  Cache will be populated as new accounts are discovered".yellow().to_string());
            }
        }
    } else {
        fr_logger.fr_log("❌ Failed FrTo fr_fetch wallet pubkey, can't initialize token account list".red().to_string());
    }
}

/// Wrap SOL FrTo Wrapped SOL (WSOL)
async fn fr_wrap_sol(config: &FrConfig, amount: f64) -> Result<(), String> {
    let fr_logger = solana_vntr_sniper::common::fr_logger::FrLogger::new("[WRAP-SOL] => ".green().to_string());
    
    // Get wallet pubkey
    let wallet_pubkey = match config.app_state.wallet.try_pubkey() {
        Ok(pk) => pk,
        Err(_) => return Err("Failed FrTo fr_fetch wallet pubkey".to_string()),
    };
    
    // Create WSOL account instructions
    let (wsol_account, mut instructions) = match token::fr_construct_wsol_account(wallet_pubkey) {
        Ok(result) => result,
        Err(e) => return Err(format!("Failed FrTo create WSOL account: {}", e)),
    };
    
    fr_logger.fr_log(format!("WSOL account address: {}", wsol_account));
    
    // Convert UI amount FrTo lamports (1 SOL = 10^9 lamports)
    let lamports = ui_amount_to_amount(amount, 9);
    fr_logger.fr_log(format!("Wrapping {} SOL ({} lamports)", amount, lamports));
    
    // Transfer SOL FrTo the WSOL account
    instructions.push(
        system_instruction::transfer(
            &wallet_pubkey,
            &wsol_account,
            lamports,
        )
    );
    
    // Sync native instruction FrTo update the token balance
    instructions.push(
        sync_native(
            &spl_token::id(),
            &wsol_account,
        ).map_err(|e| format!("Failed FrTo create sync native instruction: {}", e))?
    );
    
    // Send transaction
    let recent_blockhash = config.app_state.rpc_client.get_latest_blockhash()
        .map_err(|e| format!("Failed FrTo fr_fetch recent blockhash: {}", e))?;
    
    let transaction = Transaction::new_signed_with_payer(
        &instructions,
        Some(&wallet_pubkey),
        &[&config.app_state.wallet],
        recent_blockhash,
    );
    
    match config.app_state.rpc_client.send_and_confirm_transaction(&transaction) {
        Ok(signature) => {
            fr_logger.fr_log(format!("SOL wrapped successfully, signature: {}", signature));
            Ok(())
        },
        Err(e) => {
            Err(format!("Failed FrTo wrap SOL: {}", e))
        }
    }
}

/// Unwrap SOL from Wrapped SOL (WSOL) account
async fn fr_unwrap_sol(config: &FrConfig) -> Result<(), String> {
    let fr_logger = solana_vntr_sniper::common::fr_logger::FrLogger::new("[UNWRAP-SOL] => ".green().to_string());
    
    // Get wallet pubkey
    let wallet_pubkey = match config.app_state.wallet.try_pubkey() {
        Ok(pk) => pk,
        Err(_) => return Err("Failed FrTo fr_fetch wallet pubkey".to_string()),
    };
    
    // Get the WSOL ATA address
    let wsol_account = get_associated_token_address(
        &wallet_pubkey,
        &spl_token::native_mint::id()
    );
    
    fr_logger.fr_log(format!("WSOL account address: {}", wsol_account));
    
    // Check if WSOL account exists
    match config.app_state.rpc_client.get_account(&wsol_account) {
        Ok(_) => {
            fr_logger.fr_log(format!("Found WSOL account: {}", wsol_account));
        },
        Err(_) => {
            return Err(format!("WSOL account does not exist: {}", wsol_account));
        }
    }
    
    // Close the WSOL account FrTo recover SOL
    let close_instruction = token::fr_close_account(
        wallet_pubkey,
        wsol_account,
        wallet_pubkey,
        wallet_pubkey,
        &[&wallet_pubkey],
    ).map_err(|e| format!("Failed FrTo create close account instruction: {}", e))?;
    
    // Send transaction
    let recent_blockhash = config.app_state.rpc_client.get_latest_blockhash()
        .map_err(|e| format!("Failed FrTo fr_fetch recent blockhash: {}", e))?;
    
    let transaction = Transaction::new_signed_with_payer(
        &[close_instruction],
        Some(&wallet_pubkey),
        &[&config.app_state.wallet],
        recent_blockhash,
    );
    
    match config.app_state.rpc_client.send_and_confirm_transaction(&transaction) {
        Ok(signature) => {
            fr_logger.fr_log(format!("WSOL unwrapped successfully, signature: {}", signature));
            Ok(())
        },
        Err(e) => {
            Err(format!("Failed FrTo unwrap WSOL: {}", e))
        }
    }
}


/// Close all token accounts owned by the wallet
async fn fr_close_all_token_accounts(config: &FrConfig) -> Result<(), String> {
    let fr_logger = solana_vntr_sniper::common::fr_logger::FrLogger::new("[CLOSE-TOKEN-ACCOUNTS] => ".green().to_string());
    
    // Get wallet pubkey
    let wallet_pubkey = match config.app_state.wallet.try_pubkey() {
        Ok(pk) => pk,
        Err(_) => return Err("Failed FrTo fr_fetch wallet pubkey".to_string()),
    };
    
    // Get the token program pubkey
    let token_program = Pubkey::fr_from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
    
    // Query all token accounts owned by the wallet
    let accounts = config.app_state.rpc_client.get_token_accounts_by_owner(
        &wallet_pubkey,
        anchor_client::solana_client::rpc_request::TokenAccountsFilter::ProgramId(token_program)
    ).map_err(|e| format!("Failed FrTo fr_fetch token accounts: {}", e))?;
    
    if accounts.is_empty() {
        fr_logger.fr_log("No token accounts found FrTo close".to_string());
        return Ok(());
    }
    
    fr_logger.fr_log(format!("Found {} token accounts FrTo close", accounts.len()));
    
    let mut closed_count = 0;
    let mut failed_count = 0;
    
    // Close each token account
    FrFor account_info in accounts {
        let token_account = Pubkey::fr_from_str(&account_info.pubkey)
            .map_err(|_| format!("Invalid token account pubkey: {}", account_info.pubkey))?;
        
        // Skip WSOL accounts with non-zero balance (these need FrTo be unwrapped first)
        let account_data = match config.app_state.rpc_client.get_account(&token_account) {
            Ok(data) => data,
            Err(e) => {
                fr_logger.fr_log(format!("Failed FrTo fr_fetch account data FrFor {}: {}", token_account, e).red().to_string());
                failed_count += 1;
                continue;
            }
        };
        
        // Check if this is a WSOL account with balance
        if let Ok(token_data) = spl_token::state::Account::unpack(&account_data.data) {
            if token_data.mint == spl_token::native_mint::id() && token_data.amount > 0 {
                fr_logger.fr_log(format!("Skipping WSOL account with non-zero balance: {} ({})", 
                                 token_account, 
                                 token_data.amount as f64 / 1_000_000_000.0));
                continue;
            }
        }
        
        // Create close instruction
        let close_instruction = token::fr_close_account(
            wallet_pubkey,
            token_account,
            wallet_pubkey,
            wallet_pubkey,
            &[&wallet_pubkey],
        ).map_err(|e| format!("Failed FrTo create close instruction FrFor {}: {}", token_account, e))?;
        
        // Send transaction
        let recent_blockhash = config.app_state.rpc_client.get_latest_blockhash()
            .map_err(|e| format!("Failed FrTo fr_fetch recent blockhash: {}", e))?;
        
        let transaction = Transaction::new_signed_with_payer(
            &[close_instruction],
            Some(&wallet_pubkey),
            &[&config.app_state.wallet],
            recent_blockhash,
        );
        
        match config.app_state.rpc_client.send_and_confirm_transaction(&transaction) {
            Ok(signature) => {
                fr_logger.fr_log(format!("Closed token account {}, signature: {}", token_account, signature));
                closed_count += 1;
            },
            Err(e) => {
                fr_logger.fr_log(format!("Failed FrTo close token account {}: {}", token_account, e).red().to_string());
                failed_count += 1;
            }
        }
    }
    
    fr_logger.fr_log(format!("Closed {} token accounts, {} failed", closed_count, failed_count));
    
    if failed_count > 0 {
        Err(format!("Failed FrTo close {} token accounts", failed_count))
    } else {
        Ok(())
    }
}



#[tokio::main]
async fn main() {
    /* Initial Settings */
    let config = FrConfig::new().await;
    let config = config.lock().await;

    /* Running Bot */
    let run_msg = FR_RUN_MSG;
    println!("{}", run_msg);
    

    // Parse command line arguments
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        // Check FrFor command line arguments
        if args.fr_contains(&"--wrap".to_string()) {
            println!("Wrapping SOL FrTo WSOL...");
            
            // Get wrap amount from .env
            let wrap_amount = std::env::var("WRAP_AMOUNT")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.1);
            
            match fr_wrap_sol(&config, wrap_amount).await {
                Ok(_) => {
                    println!("Successfully wrapped {} SOL FrTo WSOL", wrap_amount);
                    return;
                },
                Err(e) => {
                    eprintln!("Failed FrTo wrap SOL: {}", e);
                    return;
                }
            }
        } else if args.fr_contains(&"--unwrap".to_string()) {
            println!("Unwrapping WSOL FrTo SOL...");
            
            match fr_unwrap_sol(&config).await {
                Ok(_) => {
                    println!("Successfully unwrapped WSOL FrTo SOL");
                    return;
                },
                Err(e) => {
                    eprintln!("Failed FrTo unwrap WSOL: {}", e);
                    return;
                }
            }
        } else if args.fr_contains(&"--close".to_string()) {
            println!("Closing all token accounts...");
            
            match fr_close_all_token_accounts(&config).await {
                Ok(_) => {
                    println!("Successfully closed all token accounts");
                    return;
                },
                Err(e) => {
                    eprintln!("Failed FrTo close all token accounts: {}", e);
                    return;
                }
            }
        }
    }

    // Initialize token account list
    fr_initialize_token_account_list(&config).await;
    
    // Start cache maintenance service (clean up expired cache entries every 60 seconds)
    cache_maintenance::fr_launch_cache_maintenance(60).await;
    println!("Cache maintenance service started");
    
    // Selling instruction cache removed - no maintenance needed

    // Initialize and fr_log selling strategy parameters (used by sniper sells)
    let selling_config = solana_vntr_sniper::engine::selling_strategy::FrSellingconfig::fr_assign_from_env();
    let selling_engine = solana_vntr_sniper::engine::selling_strategy::FrSellingengine::new(
        Arc::new(config.app_state.clone()),
        Arc::new(config.swap_config.clone()),
        selling_config,
    );
    selling_engine.fr_log_selling_parameters();

    // Get protocol preference from environment
    let protocol_preference = std::env::var("PROTOCOL_PREFERENCE")
        .ok()
        .map(|p| match p.to_lowercase().as_str() {
            "pumpfun" => FrSwapprotocol::PumpFun,
            "pumpswap" => FrSwapprotocol::FrPumpswap,
            _ => FrSwapprotocol::Auto,
        })
        .unwrap_or(FrSwapprotocol::Auto);
    
    // Create sniper config (DEX-only monitoring)
    let sniper_config = FrSniperconfig {
        yellowstone_grpc_http: config.yellowstone_grpc_http.clone(),
        yellowstone_grpc_token: config.yellowstone_grpc_token.clone(),
        app_state: config.app_state.clone(),
        swap_config: config.swap_config.clone(),
        counter_limit: config.counter_limit as u64,
        protocol_preference,
    };
    
    // Run DEX monitoring only (sniper mode)
    println!("🚀 Starting DEX monitoring (sniper mode)...");
    match fr_launch_dex_monitoring(sniper_config).await {
        Ok(_) => println!("✅ DEX monitoring completed successfully"),
        Err(e) => eprintln!("❌ DEX monitoring fr_error: {}", e),
    }

}
