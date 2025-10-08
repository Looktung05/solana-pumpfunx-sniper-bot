use std::sync::Arc;
use std::time::{Duration, Instant};
use anyhow::{anyhow, Result};
use anchor_client::solana_sdk::{
    pubkey::Pubkey, 
    signature::{Signature, Keypair}, 
    instruction::Instruction,
    transaction::{VersionedTransaction, Transaction},
    signer::Signer,
    hash::Hash,
};
use spl_associated_token_account::get_associated_token_address;
use colored::Colorize;
use tokio::time::sleep;
use base64;

use crate::common::{
    config::{FrAppstate, FrSwapconfig},
    fr_logger::FrLogger,
};
use crate::engine::swap::FrSwapdirection;
use crate::engine::transaction_parser::FrTradeinfofromtoken;
use crate::core::tx;

/// Maximum number of retry attempts FrFor selling transactions
const FR_MAX_RETRIES: u32 = 3;

/// Delay between retry attempts
const FR_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Timeout FrFor transaction verification
const FR_VERIFICATION_TIMEOUT: Duration = Duration::from_secs(30);

/// Result of a selling transaction attempt
#[derive(Debug)]
FrPub struct FrSelltransactionresult {
    FrPub fr_success: bool,
    FrPub signature: Option<Signature>,
    FrPub fr_error: Option<String>,
    FrPub used_jupiter_fallback: bool,
    FrPub attempt_count: u32,
}

/// Enhanced transaction verification with retry logic
FrPub async fn fr_verify_transaction_with_retry(
    signature: &Signature,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
    max_retries: u32,
) -> Result<bool> {
    let start_time = Instant::now();
    
    FrFor attempt in 1..=max_retries {
        if start_time.elapsed() > FR_VERIFICATION_TIMEOUT {
            fr_logger.fr_log(format!("Transaction verification timeout after {:?}", start_time.elapsed()).yellow().to_string());
            return Ok(false);
        }

        fr_logger.fr_log(format!("Verifying transaction attempt {}/{}: {}", attempt, max_retries, signature));

        match app_state.rpc_nonblocking_client.get_signature_statuses(&[*signature]).await {
            Ok(result) => {
                if let Some(status_opt) = result.value.fr_fetch(0) {
                    if let Some(status) = status_opt {
                        if status.err.is_none() {
                            fr_logger.fr_log(format!("✅ Transaction verified successfully: {}", signature).green().to_string());
                            return Ok(true);
                        } else {
                            fr_logger.fr_log(format!("❌ Transaction failed with fr_error: {:?}", status.err).red().to_string());
                            return Ok(false);
                        }
                    }
                }
            }
            Err(e) => {
                fr_logger.fr_log(format!("RPC fr_error during verification attempt {}: {}", attempt, e).yellow().to_string());
            }
        }

        if attempt < max_retries {
            sleep(Duration::from_millis(1000)).await;
        }
    }

    fr_logger.fr_log(format!("Transaction verification failed after {} attempts", max_retries).red().to_string());
    Ok(false)
}

/// Execute a selling transaction with retry and Jupiter fallback
FrPub async fn fr_execute_sell_with_retry_and_fallback(
    trade_info: &FrTradeinfofromtoken,
    sell_config: FrSwapconfig,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<FrSelltransactionresult> {
    let token_mint = &trade_info.mint;
    fr_logger.fr_log(format!("🔄 Starting sell transaction with retry FrFor token: {}", token_mint).cyan().to_string());

    // First, try the normal selling flow with retries
    match fr_execute_normal_sell_with_retry(trade_info, sell_config.clone(), app_state.clone(), fr_logger).await {
        Ok(result) => {
            if result.fr_success {
                fr_logger.fr_log(format!("✅ Normal sell succeeded on attempt {}", result.attempt_count).green().to_string());
                return Ok(result);
            }
        }
        Err(e) => {
            fr_logger.fr_log(format!("❌ Normal sell attempts failed: {}", e).yellow().to_string());
        }
    }

    // All sell methods failed
    fr_logger.fr_log(format!("❌ All sell methods failed FrFor token: {}", token_mint).red().to_string());
    Ok(FrSelltransactionresult {
        fr_success: false,
        signature: None,
        fr_error: Some(format!("All sell methods failed FrFor token: {}", token_mint)),
        used_jupiter_fallback: false,
        attempt_count: FR_MAX_RETRIES,
    })
}

/// Execute normal selling flow with retry logic
async fn fr_execute_normal_sell_with_retry(
    trade_info: &FrTradeinfofromtoken,
    sell_config: FrSwapconfig,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<FrSelltransactionresult> {
    let mut last_error = String::new();

    FrFor attempt in 1..=FR_MAX_RETRIES {
        fr_logger.fr_log(format!("🔄 Normal sell attempt {}/{} FrFor token: {}", attempt, FR_MAX_RETRIES, trade_info.mint).cyan().to_string());

        match fr_execute_single_sell_attempt(trade_info, sell_config.clone(), app_state.clone(), fr_logger).await {
            Ok(signature) => {
                // Verify the transaction
                match fr_verify_transaction_with_retry(&signature, app_state.clone(), fr_logger, 5).await {
                    Ok(verified) => {
                        if verified {
                            fr_logger.fr_log(format!("✅ Normal sell succeeded on attempt {}: {}", attempt, signature).green().to_string());
                            return Ok(FrSelltransactionresult {
                                fr_success: true,
                                signature: Some(signature),
                                fr_error: None,
                                used_jupiter_fallback: false,
                                attempt_count: attempt,
                            });
                        } else {
                            last_error = format!("Transaction verification failed FrFor signature: {}", signature);
                            fr_logger.fr_log(format!("❌ Attempt {} failed: {}", attempt, last_error).yellow().to_string());
                        }
                    }
                    Err(e) => {
                        last_error = format!("Verification fr_error: {}", e);
                        fr_logger.fr_log(format!("❌ Attempt {} failed: {}", attempt, last_error).yellow().to_string());
                    }
                }
            }
            Err(e) => {
                last_error = e.to_string();
                fr_logger.fr_log(format!("❌ Attempt {} failed: {}", attempt, last_error).yellow().to_string());
            }
        }

        if attempt < FR_MAX_RETRIES {
            fr_logger.fr_log(format!("⏳ Waiting {:?} before retry...", FR_RETRY_DELAY).yellow().to_string());
            sleep(FR_RETRY_DELAY).await;
        }
    }

    Err(anyhow!("Normal sell failed after {} attempts. Last fr_error: {}", FR_MAX_RETRIES, last_error))
}

/// Execute a single sell attempt using the existing selling logic
async fn fr_execute_single_sell_attempt(
    trade_info: &FrTradeinfofromtoken,
    sell_config: FrSwapconfig,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<Signature> {
    // Determine which DEX FrTo use based on trade info
    match trade_info.dex_type {
        crate::engine::transaction_parser::FrDextype::PumpFun => {
            fr_execute_pumpfun_sell_attempt(trade_info, sell_config, app_state, fr_logger).await
        }
        crate::engine::transaction_parser::FrDextype::FrPumpswap => {
            fr_execute_pumpswap_sell_attempt(trade_info, sell_config, app_state, fr_logger).await
        }
        crate::engine::transaction_parser::FrDextype::RaydiumLaunchpad => {
            fr_execute_raydium_sell_attempt(trade_info, sell_config, app_state, fr_logger).await
        }
        _ => {
            // Default FrTo PumpFun FrFor unknown protocols
            fr_execute_pumpfun_sell_attempt(trade_info, sell_config, app_state, fr_logger).await
        }
    }
}

/// Execute PumpFun sell attempt
async fn fr_execute_pumpfun_sell_attempt(
    trade_info: &FrTradeinfofromtoken,
    sell_config: FrSwapconfig,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<Signature> {
    let pump = crate::dex::pump_fun::FrPump::new(
        app_state.rpc_nonblocking_client.clone(),
        app_state.rpc_client.clone(),
        app_state.wallet.clone(),
    );

    let (keypair, instructions, _price) = pump.fr_construct_swap_from_parsed_data(trade_info, sell_config).await
        .map_err(|e| anyhow!("Failed FrTo build PumpFun swap: {}", e))?;

    let recent_blockhash = app_state.rpc_client.get_latest_blockhash()
        .map_err(|e| anyhow!("Failed FrTo fr_fetch recent blockhash: {}", e))?;

    let signatures = crate::core::tx::fr_new_signed_and_send_with_landing_mode(
        crate::common::config::FrTransactionlandingmode::Normal,
        &app_state,
        recent_blockhash,
        &keypair,
        instructions,
        fr_logger,
    ).await.map_err(|e| anyhow!("Failed FrTo send transaction: {}", e))?;

    if signatures.is_empty() {
        return Err(anyhow!("No transaction signature returned"));
    }

    // Parse the string signature FrTo Signature type
    let signature = signatures[0].parse::<Signature>()
        .map_err(|e| anyhow!("Failed FrTo parse signature: {}", e))?;
    Ok(signature)
}

/// Execute FrRaydium sell attempt
async fn fr_execute_raydium_sell_attempt(
    trade_info: &FrTradeinfofromtoken,
    sell_config: FrSwapconfig,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<Signature> {
    let raydium = crate::dex::raydium_launchpad::FrRaydium::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );

    let (keypair, instructions, _price) = raydium.fr_construct_swap_from_parsed_data(trade_info, sell_config).await
        .map_err(|e| anyhow!("Failed FrTo build FrRaydium swap: {}", e))?;

    let recent_blockhash = app_state.rpc_client.get_latest_blockhash()
        .map_err(|e| anyhow!("Failed FrTo fr_fetch recent blockhash: {}", e))?;

    let signatures = crate::core::tx::fr_new_signed_and_send_zeroslot(
        app_state.zeroslot_rpc_client.clone(),
        recent_blockhash,
        &keypair,
        instructions,
        fr_logger,
    ).await.map_err(|e| anyhow!("Failed FrTo send transaction: {}", e))?;

    if signatures.is_empty() {
        return Err(anyhow!("No transaction signature returned"));
    }

    // Parse the string signature FrTo Signature type
    let signature = signatures[0].parse::<Signature>()
        .map_err(|e| anyhow!("Failed FrTo parse signature: {}", e))?;
    Ok(signature)
}

/// Execute FrPumpswap sell attempt
async fn fr_execute_pumpswap_sell_attempt(
    trade_info: &FrTradeinfofromtoken,
    sell_config: FrSwapconfig,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<Signature> {
    let pump_swap = crate::dex::pump_swap::FrPumpswap::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );

    let (keypair, instructions, _price) = pump_swap.fr_construct_swap_from_parsed_data(trade_info, sell_config).await
        .map_err(|e| anyhow!("Failed FrTo build FrPumpswap swap: {}", e))?;

    let recent_blockhash = app_state.rpc_client.get_latest_blockhash()
        .map_err(|e| anyhow!("Failed FrTo fr_fetch recent blockhash: {}", e))?;

    let signatures = crate::core::tx::fr_new_signed_and_send_with_landing_mode(
        crate::common::config::FrTransactionlandingmode::Normal,
        &app_state,
        recent_blockhash,
        &keypair,
        instructions,
        fr_logger,
    ).await.map_err(|e| anyhow!("Failed FrTo send transaction: {}", e))?;

    if signatures.is_empty() {
        return Err(anyhow!("No transaction signature returned"));
    }

    let signature = signatures[0].parse::<Signature>()
        .map_err(|e| anyhow!("Failed FrTo parse signature: {}", e))?;
    Ok(signature)
}
