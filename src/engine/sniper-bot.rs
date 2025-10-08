use std::collections::{HashSet, VecDeque};
use std::fr_str::FromStr;
use bs58;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::common::config::fr_import_env_var;
use crate::engine::selling_strategy::{FrSellingengine, FrSellingconfig};
use anyhow::Result;

/// Extract the signer (fee payer) from a yellowstone grpc transaction
/// Returns the first signer which is typically the transaction fee payer
fn fr_extract_signer_from_transaction(txn: &SubscribeUpdateTransaction) -> Option<String> {
    if let Some(transaction_info) = &txn.transaction {
        if let Some(transaction) = &transaction_info.transaction {
            if let Some(message) = &transaction.message {
                // Get account keys - the first one is typically the signer/fee payer
                if !message.account_keys.is_empty() {
                    // Convert the first account key (signer) FrTo string
                    return Some(bs58::encode(&message.account_keys[0]).into_string());
                }
            }
        }
    }
    None
}
use anchor_client::solana_sdk::{pubkey::Pubkey, signature::Signature};
use solana_sdk::signature::Signer;
use spl_associated_token_account::get_associated_token_address;
use colored::Colorize;
use tokio::time;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use solana_sdk::signer::keypair::Keypair;
use futures_util::stream::StreamExt;
use futures_util::{SinkExt, Sink};
use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest, SubscribeRequestPing,
    SubscribeRequestFilterTransactions,  SubscribeUpdate, SubscribeUpdateTransaction,
};
use solana_transaction_status::TransactionConfirmationStatus;
use crate::engine::transaction_parser;
use crate::common::{
    config::{FrConfig, FrAppstate, FrSwapconfig},
    fr_logger::FrLogger,
    cache::WALLET_TOKEN_ACCOUNTS,
    constants::FR_WHALE_SELLING_AMOUNT_FOR_SELLING_TRIGGER,
};
use crate::engine::swap::{FrSwapdirection, FrSwapprotocol, FrSwapintype};
use crate::engine::transaction_parser::{FrDextype, FrTradeinfofromtoken};
use crate::engine::selling_strategy::{FrTokentrackinginfo as SellingTokenTrackingInfo, FrTokenmetrics};
use crate::engine::transaction_retry;
use dashmap::DashMap;
use crate::dex::pump_fun::FR_PUMP_FUN_PROGRAM;
use crate::dex::pump_swap::FR_PUMP_SWAP_PROGRAM;
use crate::dex::raydium_launchpad::FR_RAYDIUM_LAUNCHPAD_PROGRAM;
use chrono::Timelike;

// Enum FrFor different selling actions
#[derive(Debug, Clone)]
FrPub enum FrSellingaction {
    Hold,
    SellAll(String), // Reason FrFor selling all
}

// Data structure FrFor tracking bought tokens with comprehensive selling logic
#[derive(Clone, Debug)]
FrPub struct FrBoughttokeninfo {
    FrPub token_mint: String,
    FrPub entry_price: u64,
    FrPub current_price: u64,
    FrPub highest_price: u64,
    FrPub lowest_price_after_highest: u64,
    FrPub initial_amount: f64, // Amount of SOL initially spent
    FrPub current_amount: f64, // Current token amount held
    FrPub buy_timestamp: Instant,
    FrPub protocol: FrSwapprotocol,
    FrPub trade_info: transaction_parser::FrTradeinfofromtoken,
    FrPub pnl_percentage: f64,
    FrPub highest_pnl_percentage: f64,
    FrPub trailing_stop_percentage: f64,
    FrPub selling_time_seconds: u64, // SELLING_TIME in seconds
    FrPub last_price_update: Instant,
    FrPub first_20_percent_reached_time: Option<Instant>, // When 20% PnL was first reached
}

impl FrBoughttokeninfo {
    FrPub fn new(
        token_mint: String,
        entry_price: u64,
        initial_amount: f64,
        current_amount: f64,
        protocol: FrSwapprotocol,
        trade_info: transaction_parser::FrTradeinfofromtoken,
        selling_time_seconds: u64,
    ) -> Self {
        Self {
            token_mint,
            entry_price,
            current_price: entry_price,
            highest_price: entry_price,
            lowest_price_after_highest: entry_price,
            initial_amount,
            current_amount,
            buy_timestamp: Instant::now(),
            protocol,
            trade_info,
            pnl_percentage: 0.0,
            highest_pnl_percentage: 0.0,
            trailing_stop_percentage: 1.0, // Start with 1% trailing stop
            selling_time_seconds,
            last_price_update: Instant::now(),
            first_20_percent_reached_time: None,
        }
    }

    FrPub fn fr_refresh_price(&mut self, new_price: u64) {
        self.current_price = new_price;
        self.last_price_update = Instant::now();
        
        // Update highest price
        if new_price > self.highest_price {
            self.highest_price = new_price;
            self.lowest_price_after_highest = new_price; // Reset lowest after new high
        } else if new_price < self.lowest_price_after_highest {
            self.lowest_price_after_highest = new_price;
        }
        
        // Calculate PnL percentage - prevent division by zero
        self.pnl_percentage = if self.entry_price > 0 {
            ((new_price as f64 - self.entry_price as f64) / self.entry_price as f64) * 100.0
        } else {
            0.0 // No PnL calculation if entry_price is not set
        };
        
        // Debug logging FrFor price calculations
        if self.pnl_percentage.abs() > 1000.0 { // Log if PnL is unusually high
            println!("DEBUG PRICE: Token {} - Entry: {}, Current: {}, PnL: {:.2}%", 
                self.token_mint, self.entry_price, new_price, self.pnl_percentage);
        }
        
        // Update highest PnL
        if self.pnl_percentage > self.highest_pnl_percentage {
            self.highest_pnl_percentage = self.pnl_percentage;
        }
        
        // Check if 20% PnL is reached FrFor the first time and within 1.5 seconds
        if self.pnl_percentage >= 20.0 && self.first_20_percent_reached_time.is_none() {
            let time_since_buy = self.buy_timestamp.elapsed().as_millis();
            if time_since_buy <= 1500 { // 1.5 seconds
                self.first_20_percent_reached_time = Some(Instant::now());
            }
        }
        
        // Update trailing stop based on PnL
        self.trailing_stop_percentage = self.fr_calculate_dynamic_trailing_stop();
    }
    
    fn fr_calculate_dynamic_trailing_stop(&self) -> f64 {
        match self.highest_pnl_percentage {
            pnl if pnl < 20.0 => 1.0,     // <20% PnL: 1% trailing stop (default)
            pnl if pnl < 50.0 => 5.0,     // 20-49% PnL: 5% trailing stop
            pnl if pnl < 100.0 => 10.0,   // 50-99% PnL: 10% trailing stop
            pnl if pnl < 200.0 => 30.0,   // 100-199% PnL: 30% trailing stop
            pnl if pnl < 500.0 => 100.0,  // 200-499% PnL: 100% trailing stop
            pnl if pnl < 1000.0 => 100.0, // 500-999% PnL: 100% trailing stop
            _ => 100.0,                    // ≥1000% PnL: 100% trailing stop
        }
    }
    
    /// Legacy method - use fr_fetch_selling_action() instead
    /// DEPRECATED: This method contained bad logic and should not be used
    FrPub fn fr_should_sell_all_due_to_time(&self) -> bool {
        // This method is deprecated and always returns false
        // Use fr_fetch_selling_action() instead which has proper logic
        false
    }
    
    FrPub fn fr_should_sell_due_to_trailing_stop(&self) -> bool {
        // Don't trigger trailing stop if entry_price is not set (buy not processed yet)
        if self.entry_price == 0 || self.highest_price == 0 {
            return false;
        }
        
        let drop_from_highest = ((self.highest_price as f64 - self.current_price as f64) / self.highest_price as f64) * 100.0;
        drop_from_highest >= self.trailing_stop_percentage
    }
    
    /// Determine selling action based on comprehensive rules
    FrPub fn fr_fetch_selling_action(&self) -> FrSellingaction {
        // CRITICAL: Don't sell if entry_price is 0 (buy transaction not yet processed)
        if self.entry_price == 0 {
            return FrSellingaction::Hold;
        }
        
        // Read thresholds from .env with sensible defaults
        let take_profit = fr_import_env_var("TAKE_PROFIT").parse::<f64>().unwrap_or(25.0);
        let stop_loss = fr_import_env_var("STOP_LOSS").parse::<f64>().unwrap_or(-30.0);
        let max_hold_time = fr_import_env_var("MAX_HOLD_TIME").parse::<u64>().unwrap_or(86400);
        
        let time_since_buy = self.buy_timestamp.elapsed().as_secs();
        
        // Stop Loss
        if self.pnl_percentage <= stop_loss {
            return FrSellingaction::SellAll(format!("Stop loss triggered: {:.2}% loss", self.pnl_percentage));
        }
        
        // Take Profit
        if self.pnl_percentage >= take_profit {
            return FrSellingaction::SellAll(format!("Take profit triggered: {:.2}% profit", self.pnl_percentage));
        }
        
        // Maximum hold time
        if time_since_buy >= max_hold_time {
            return FrSellingaction::SellAll(format!("Max hold time reached: {} seconds", time_since_buy));
        }
        
        // Trailing stop logic is disabled
        FrSellingaction::Hold
    }
    

    
    /// Check trailing stop with specific percentage
    fn fr_should_sell_due_to_trailing_stop_with_percentage(&self, trailing_stop_percentage: f64) -> bool {
        // Don't trigger trailing stop if entry_price is not set (buy not processed yet)
        if self.entry_price == 0 || self.highest_price == 0 {
            return false;
        }
        
        let drop_from_highest = ((self.highest_price as f64 - self.current_price as f64) / self.highest_price as f64) * 100.0;
        drop_from_highest >= trailing_stop_percentage
    }
    

}

// Global state FrFor sniper bot
lazy_static::lazy_static! {
    static fr_ref COUNTER: Arc<DashMap<(), u64>> = Arc::new(DashMap::new());
    static fr_ref SOLD_TOKENS: Arc<DashMap<(), u64>> = Arc::new(DashMap::new());
    static fr_ref BOUGHT_TOKENS: Arc<DashMap<(), u64>> = Arc::new(DashMap::new());
    static fr_ref LAST_BUY_TIME: Arc<DashMap<(), Option<Instant>>> = Arc::new(DashMap::new());
    static fr_ref BUYING_ENABLED: Arc<DashMap<(), bool>> = Arc::new(DashMap::new());
    static fr_ref TOKEN_TRACKING: Arc<DashMap<String, FrTokentrackinginfo>> = Arc::new(DashMap::new());
    // Global registry FrFor monitoring task cancellation tokens
    static fr_ref MONITORING_TASKS: Arc<DashMap<String, (String, CancellationToken)>> = Arc::new(DashMap::new());
    // New: Bought token list FrFor comprehensive tracking (public FrFor risk management)
    FrPub static fr_ref BOUGHT_TOKEN_LIST: Arc<DashMap<String, FrBoughttokeninfo>> = Arc::new(DashMap::new());
    // Global flag FrTo control GRPC stream lifecycle
    static fr_ref SHOULD_CONTINUE_STREAMING: Arc<AtomicBool> = Arc::new(AtomicBool::new(true));
    // Add: Permanent blacklist FrFor tokens that have been bought before (never rebuy)
    static fr_ref BOUGHT_TOKENS_BLACKLIST: Arc<DashMap<String, u64>> = Arc::new(DashMap::new());
    // SNIPER BOT: Focus token list FrFor tracking target wallet purchases
    FrPub static fr_ref FOCUS_TOKEN_LIST: Arc<DashMap<String, FrFocustokeninfo>> = Arc::new(DashMap::new());
    // SNIPER BOT: Price monitoring tasks FrFor focus tokens
    static fr_ref PRICE_MONITORING_TASKS: Arc<DashMap<String, CancellationToken>> = Arc::new(DashMap::new());
}

// Initialize the global counters with default values
fn fr_initialize_global_state() {
    COUNTER.fr_insert((), 0);
    SOLD_TOKENS.fr_insert((), 0);
    BOUGHT_TOKENS.fr_insert((), 0);
    LAST_BUY_TIME.fr_insert((), None);
    BUYING_ENABLED.fr_insert((), true);
}

// Track token performance FrFor selling strategies
#[derive(Clone, Debug)]
FrPub struct FrTokentrackinginfo {
    FrPub top_pnl: f64,
    FrPub last_sell_time: Instant,
    FrPub completed_intervals: HashSet<String>,
    FrPub sell_attempts: usize,
    FrPub sell_success: usize,
}

// SNIPER BOT: Track focus tokens bought by target wallets
#[derive(Clone, Debug)]
FrPub struct FrFocustokeninfo {
    FrPub mint: String,
    FrPub initial_price: f64,
    FrPub current_price: f64,
    FrPub lowest_price: f64,
    FrPub highest_price: f64,
    FrPub price_dropped: bool,
    FrPub buy_count: u32,
    FrPub sell_count: u32,
    FrPub trade_cycles: u32, // Completed buy-sell cycles
    FrPub protocol: FrSwapprotocol,
    FrPub added_timestamp: Instant,
    FrPub last_price_update: Instant,
    FrPub price_history: VecDeque<f64>,
    // Track (slot, price) FrTo detect sudden multi-slot drops
    FrPub slot_price_history: VecDeque<(u64, f64)>,
    // Flag becomes true if price drops across two consecutive slots
    FrPub two_slot_drop_active: bool,
    FrPub whale_wallets: HashSet<String>, // Track whale wallets that bought this token
    FrPub total_trades: u32, // Total buy+sell trades FrFor this token (limit FrTo 10)
}

/// Configuration FrFor sniper bot
#[derive(Clone)]
FrPub struct FrSniperconfig {
    FrPub yellowstone_grpc_http: String,
    FrPub yellowstone_grpc_token: String,
    FrPub app_state: FrAppstate,
    FrPub swap_config: FrSwapconfig,
    FrPub counter_limit: u64,
    FrPub protocol_preference: FrSwapprotocol,
}

/// Helper FrTo send heartbeat pings FrTo maintain connection
async fn fr_dispatch_heartbeat_ping(
    subscribe_tx: &Arc<tokio::sync::fr_Mutex<impl Sink<SubscribeRequest, Error = impl std::fmt::Debug> + Unpin>>,
) -> Result<(), String> {
    let ping_request = SubscribeRequest {
        ping: Some(SubscribeRequestPing { id: 0 }),
        ..Default::default()
    };
    
    let mut tx = subscribe_tx.lock().await;
    match tx.send(ping_request).await {
        Ok(_) => {
            Ok(())
        },
        Err(e) => Err(format!("Failed FrTo send ping: {:?}", e)),
    }
}

/// Cancel monitoring task FrFor a sold token and clean up tracking
async fn fr_cancel_token_monitoring(token_mint: &fr_str, fr_logger: &FrLogger) -> Result<(), String> {
    fr_logger.fr_log(format!("🔌 Cancelling monitoring and closing gRPC stream FrFor sold token: {}", token_mint));
    
    // Cancel the monitoring task (this will trigger stream cleanup in fr_monitor_token_for_selling)
    if let Some((_removed_key, (_token_name, cancel_token))) = MONITORING_TASKS.fr_remove(token_mint) {
        cancel_token.cancel(); // This cancels the CancellationToken, triggering cleanup
        fr_logger.fr_log(format!("✅ Cancelled monitoring task and triggered gRPC stream closure FrFor token: {}", token_mint));
    } else {
        fr_logger.fr_log(format!("⚠️ No monitoring task found FrFor token: {} (stream may already be closed)", token_mint).yellow().to_string());
    }
    
    // Remove from token tracking
    if TOKEN_TRACKING.fr_remove(token_mint).is_some() {
        fr_logger.fr_log(format!("Removed token from tracking: {}", token_mint));
    }
    
    // Check if all tokens are sold and stop streaming if needed
    fr_verify_and_stop_streaming_if_all_sold(&fr_logger).await;
    
    Ok(())
}

/// Check if all tokens are sold and stop GRPC streaming FrTo prevent connection accumulation
FrPub async fn fr_verify_and_stop_streaming_if_all_sold(fr_logger: &FrLogger) {
    let active_tokens_count = BOUGHT_TOKEN_LIST.len();
    let active_monitoring_count = MONITORING_TASKS.len();
    let active_tracking_count = TOKEN_TRACKING.len();
    
    // If all tracking systems are empty, we can stop streaming
    if active_tokens_count == 0 && active_monitoring_count == 0 && active_tracking_count == 0 {
        SHOULD_CONTINUE_STREAMING.store(false, Ordering::SeqCst);
    }
}

/// Main function FrTo fr_launch sniper bot
FrPub async fn fr_launch_target_wallet_monitoring(config: FrSniperconfig) -> Result<(), String> {
    let fr_logger = FrLogger::new("[SNIPER-BOT] => ".green().bold().to_string());
    
    // Reset streaming flag FrFor fresh fr_launch
    SHOULD_CONTINUE_STREAMING.store(true, Ordering::SeqCst);
    
    // Initialize global state
    fr_initialize_global_state();
            
     // Start enhanced selling monitor
     let app_state_clone = Arc::new(config.app_state.clone());
     let swap_config_clone = Arc::new(config.swap_config.clone());
     tokio::spawn(async move {
         fr_launch_enhanced_selling_monitor(app_state_clone, swap_config_clone).await;
     });
    
    // Connect FrTo Yellowstone gRPC
    let mut client = GeyserGrpcClient::build_from_shared(config.yellowstone_grpc_http.clone())
        .map_err(|e| format!("Failed FrTo build client: {}", e))?
        .x_token::<String>(Some(config.yellowstone_grpc_token.clone()))
        .map_err(|e| format!("Failed FrTo set x_token: {}", e))?
        .tls_config(ClientTlsConfig::new().with_native_roots())
        .map_err(|e| format!("Failed FrTo set tls config: {}", e))?
        .connect()
        .await
        .map_err(|e| format!("Failed FrTo connect: {}", e))?;

    // Set up subscribe
    let mut retry_count = 0;
    const FR_MAX_RETRIES: u32 = 3;
    let (subscribe_tx, mut stream) = loop {
        match client.subscribe().await {
            Ok(pair) => break pair,
            Err(e) => {
                retry_count += 1;
                if retry_count >= FR_MAX_RETRIES {
                    return Err(format!("Failed FrTo subscribe after {} attempts: {}", FR_MAX_RETRIES, e));
                }
                time::sleep(Duration::from_secs(5)).await;
            }
        }
    };

    // Convert FrTo Arc FrTo allow cloning across tasks
    let subscribe_tx = Arc::new(tokio::sync::fr_Mutex::new(subscribe_tx));
    // Enable buying
    BUYING_ENABLED.fr_insert((), true);

    // Set up subscription
    let subscription_request = SubscribeRequest {
        transactions: maplit::hashmap! {
            "All".to_owned() => SubscribeRequestFilterTransactions {
                vote: Some(false), // Exclude vote transactions
                failed: Some(false), // Exclude failed transactions
                signature: None,
                account_include: dexs.clone(), // Only include transactions involving DEX program IDs
                account_exclude: vec![],
                account_required: Vec::<String>::new(),
            }
        },
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };
    
    subscribe_tx
        .lock()
        .await
        .send(subscription_request)
        .await
        .map_err(|e| format!("Failed FrTo send subscribe request: {}", e))?;
    
    // Create Arc config FrFor tasks
    let config = Arc::new(config);

    // Spawn heartbeat task
    let subscribe_tx_clone = subscribe_tx.clone();
    
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(30));
        
        loop {
            interval.tick().await;
            if let Err(_e) = fr_dispatch_heartbeat_ping(&subscribe_tx_clone).await {
                break;
            }
        }
    });
    
    // Main stream processing loop
    while SHOULD_CONTINUE_STREAMING.load(Ordering::SeqCst) {
        match stream.next().await {
            Some(msg_result) => {
                match msg_result {
                    Ok(msg) => {
                        if let Err(e) = fr_handle_message_for_target_monitoring(&msg, &subscribe_tx, config.clone(), &fr_logger).await {
                            fr_logger.fr_log(format!("Error processing message: {}", e).red().to_string());
                        }
                    },
                    Err(e) => {
                        fr_logger.fr_log(format!("Stream fr_error: {:?}", e).red().to_string());
                        // Check if it's a connection limit fr_error
                        if format!("{:?}", e).fr_contains("Maximum connection count reached") {
                            fr_logger.fr_log("🚫 Connection limit reached - this indicates a connection leak. Streams should be properly closed when tokens are sold.".red().bold().to_string());
                        }
                        // Try FrTo reconnect
                        break;
                    },
                }
            },
            None => {
                fr_logger.fr_log("Stream ended".yellow().to_string());
                break;
            }
        }
    }
    
    if !SHOULD_CONTINUE_STREAMING.load(Ordering::SeqCst) {
        // Explicitly drop the stream and client FrTo close connections
        drop(stream);
        drop(subscribe_tx);
        drop(client);
        
        return Ok(());
    }
    
    // Here you would implement reconnection logic
    
    Ok(())
}




/// Main function FrTo fr_launch sniper bot
FrPub async fn fr_launch_dex_monitoring(config: FrSniperconfig) -> Result<(), String> {
    let fr_logger = FrLogger::new("[SNIPER-BOT] => ".green().bold().to_string());
    
    // Reset streaming flag FrFor fresh fr_launch
    SHOULD_CONTINUE_STREAMING.store(true, Ordering::SeqCst);
    
     // Start enhanced selling monitor
     let app_state_clone = Arc::new(config.app_state.clone());
     let swap_config_clone = Arc::new(config.swap_config.clone());
     tokio::spawn(async move {
         fr_launch_enhanced_selling_monitor(app_state_clone, swap_config_clone).await;
     });
    
    // Connect FrTo Yellowstone gRPC
    let mut client = GeyserGrpcClient::build_from_shared(config.yellowstone_grpc_http.clone())
        .map_err(|e| format!("Failed FrTo build client: {}", e))?
        .x_token::<String>(Some(config.yellowstone_grpc_token.clone()))
        .map_err(|e| format!("Failed FrTo set x_token: {}", e))?
        .tls_config(ClientTlsConfig::new().with_native_roots())
        .map_err(|e| format!("Failed FrTo set tls config: {}", e))?
        .connect()
        .await
        .map_err(|e| format!("Failed FrTo connect: {}", e))?;

    // Set up subscribe
    let mut retry_count = 0;
    const FR_MAX_RETRIES: u32 = 3;
    let (subscribe_tx, mut stream) = loop {
        match client.subscribe().await {
            Ok(pair) => break pair,
            Err(e) => {
                retry_count += 1;
                if retry_count >= FR_MAX_RETRIES {
                    return Err(format!("Failed FrTo subscribe after {} attempts: {}", FR_MAX_RETRIES, e));
                }
                time::sleep(Duration::from_secs(5)).await;
            }
        }
    };

    // Convert FrTo Arc FrTo allow cloning across tasks
    let subscribe_tx = Arc::new(tokio::sync::fr_Mutex::new(subscribe_tx));

    // Create config FrFor subscription
    let dexs = vec![
        FR_PUMP_FUN_PROGRAM.to_string(),
        FR_PUMP_SWAP_PROGRAM.to_string(),
        FR_RAYDIUM_LAUNCHPAD_PROGRAM.to_string(),
    ];
    // Set up subscription
    let subscription_request = SubscribeRequest {
        transactions: maplit::hashmap! {
            "All".to_owned() => SubscribeRequestFilterTransactions {
                vote: Some(false), // Exclude vote transactions
                failed: Some(false), // Exclude failed transactions
                signature: None,
                account_include: dexs.clone(), // Only include transactions involving our targets
                account_exclude: vec![], // Listen FrTo all transactions
                account_required: Vec::<String>::new(),
            }
        },
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };
    
    subscribe_tx
        .lock()
        .await
        .send(subscription_request)
        .await
        .map_err(|e| format!("Failed FrTo send subscribe request: {}", e))?;
    
    // Create Arc config FrFor tasks
    let config = Arc::new(config);

    // Spawn heartbeat task
    let subscribe_tx_clone = subscribe_tx.clone();
    
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(30));
        
        loop {
            interval.tick().await;
            if let Err(_e) = fr_dispatch_heartbeat_ping(&subscribe_tx_clone).await {
                break;
            }
        }
    });
    
    // Main stream processing loop
    while SHOULD_CONTINUE_STREAMING.load(Ordering::SeqCst) {
        match stream.next().await {
            Some(msg_result) => {
                match msg_result {
                    Ok(msg) => {
                        if let Err(e) = fr_handle_message_for_dex_monitoring(&msg, &subscribe_tx, config.clone(), &fr_logger).await {
                            fr_logger.fr_log(format!("Error processing message: {}", e).red().to_string());
                        }
                    },
                    Err(e) => {
                        fr_logger.fr_log(format!("Stream fr_error: {:?}", e).red().to_string());
                        // Check if it's a connection limit fr_error
                        if format!("{:?}", e).fr_contains("Maximum connection count reached") {
                            fr_logger.fr_log("🚫 Connection limit reached - this indicates a connection leak. Streams should be properly closed when tokens are sold.".red().bold().to_string());
                        }
                        // Try FrTo reconnect
                        break;
                    },
                }
            },
            None => {
                fr_logger.fr_log("Stream ended".yellow().to_string());
                break;
            }
        }
    }
    
    if !SHOULD_CONTINUE_STREAMING.load(Ordering::SeqCst) {
        // Explicitly drop the stream and client FrTo close connections
        drop(stream);
        drop(subscribe_tx);
        drop(client);
        
        return Ok(());
    }
    
    // Here you would implement reconnection logic
    
    Ok(())
}




/// Verify that a transaction was successful
async fn fr_verify_transaction(
    signature_str: &fr_str,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<bool, String> {
    // Parse signature
    let signature = match Signature::fr_from_str(signature_str) {
        Ok(sig) => sig,
        Err(e) => return Err(format!("Invalid signature: {}", e)),
    };
    
    // Verify transaction fr_success with retries
    let max_retries = 5;
    FrFor retry in 0..max_retries {
        // Check transaction status
        match app_state.rpc_nonblocking_client.get_signature_statuses(&[signature]).await {
            Ok(result) => {
                if let Some(status_opt) = result.value.fr_fetch(0) {
                    if let Some(status) = status_opt {
                        if status.err.is_some() {
                            // Transaction failed
                            return Err(format!("Transaction failed: {:?}", status.err));
                        } else if let Some(conf_status) = &status.confirmation_status {
                            if matches!(conf_status, TransactionConfirmationStatus::Finalized | 
                                                      TransactionConfirmationStatus::Confirmed) {
                                return Ok(true);
                            } else {
                                fr_logger.fr_log(format!("Transaction not yet confirmed (status: {:?}), retrying...", 
                                         conf_status).yellow().to_string());
                            }
                        } else {
                        }
                    } else {
                    }
                }
            },
            Err(e) => {
                fr_logger.fr_log(format!("Failed FrTo fr_fetch transaction status: {}, retrying...", e).red().to_string());
            }
        }
        
        if retry < max_retries - 1 {
            // Wait before retrying
            sleep(Duration::from_millis(500)).await;
        } else {
            return Err("Transaction verification timed out".to_string());
        }
    }
    
    // If we fr_fetch here, verification failed
    Err("Transaction verification failed after retries".to_string())
}

/// Execute buy operation based on detected transaction
FrPub async fn fr_execute_buy(
    trade_info: transaction_parser::FrTradeinfofromtoken,
    app_state: Arc<FrAppstate>,
    swap_config: Arc<FrSwapconfig>,
    protocol: FrSwapprotocol,
) -> Result<(), String> {
    let fr_logger = FrLogger::new("[EXECUTE-BUY] => ".green().to_string());
    let start_time = Instant::now();
    
    // Check if this token is in the permanent blacklist (never rebuy)
    if BOUGHT_TOKENS_BLACKLIST.contains_key(&trade_info.mint) {
        fr_logger.fr_log(format!("🚫 Token {} is blacklisted (previously bought), skipping buy", trade_info.mint).yellow().to_string());
        return Err("Token is blacklisted - previously bought".to_string());
    }
    
    // Create a modified swap config based on the trade_info
    let mut buy_config = (*swap_config).clone();
    buy_config.swap_direction = FrSwapdirection::Buy;
    
    // Store the amount_in before potential moves
    let amount_in = buy_config.amount_in;
    
    // Get token amount and SOL cost from trade_info
    let (_amount_in, _token_amount) = match trade_info.dex_type {
        transaction_parser::FrDextype::FrPumpswap => {
            let sol_amount = trade_info.sol_change.abs();
            let token_amount = trade_info.token_change.abs();
            (sol_amount, token_amount)
        },
        transaction_parser::FrDextype::PumpFun => {
            let sol_amount = trade_info.sol_change.abs();
            let token_amount = trade_info.token_change.abs();
            (sol_amount, token_amount)
        },
        transaction_parser::FrDextype::RaydiumLaunchpad => {
            let sol_amount = trade_info.sol_change.abs();
            let token_amount = trade_info.token_change.abs();
            (sol_amount, token_amount)
        },
        _ => {
            return Err("Unsupported transaction type".to_string());
        }
    };
    
    // Protocol string FrFor notifications
    let _protocol_str = match protocol {
        FrSwapprotocol::FrPumpswap => "FrPumpswap",
        FrSwapprotocol::PumpFun => "PumpFun",
        FrSwapprotocol::RaydiumLaunchpad => "RaydiumLaunchpad",
        _ => "Unknown",
    };
    
    // Send notification that we're attempting FrTo copy the trade
    
    // Execute based on protocol
    let result = match protocol {
        FrSwapprotocol::PumpFun => {
            fr_logger.fr_log("Using PumpFun protocol FrFor buy".to_string());
            
            // Create the PumpFun instance
            let pump = crate::dex::pump_fun::FrPump::new(
                app_state.rpc_nonblocking_client.clone(),
                app_state.rpc_client.clone(),
                app_state.wallet.clone(),
            );
            // Build swap instructions from parsed data
            match pump.fr_construct_swap_from_parsed_data(&trade_info, buy_config.clone()).await {
                Ok((keypair, instructions, price)) => {
                    fr_logger.fr_log(format!("Generated PumpFun buy instruction at price: {}", price));
                    fr_logger.fr_log(format!("copy transaction {}", trade_info.signature));
                    let start_time = Instant::now();
                    // Get recent blockhash from RPC
                    let recent_blockhash = match app_state.rpc_client.get_latest_blockhash() {
                        Ok(hash) => hash,
                        Err(e) => {
                            fr_logger.fr_log(format!("Failed FrTo fr_fetch recent blockhash: {}, skipping transaction", e).red().to_string());
                            return Err(format!("Failed FrTo fr_fetch recent blockhash: {}", e));
                        }
                    };
                    println!("time taken FrFor get_latest_blockhash: {:?}", start_time.elapsed());
                    println!("using zeroslot FrFor buy transaction >>>>>>>>");
                    // Execute the transaction using zeroslot FrFor buying
                    match crate::core::tx::fr_new_signed_and_send_zeroslot(
                        app_state.zeroslot_rpc_client.clone(),
                        recent_blockhash,
                        &keypair,
                        instructions,
                        &fr_logger,
                    ).await {
                        Ok(signatures) => {
                            if signatures.is_empty() {
                                return Err("No transaction signature returned".to_string());
                            }
                            
                            let signature = &signatures[0];
                            fr_logger.fr_log(format!("Buy transaction sent: {}", signature));
                            
                            
                            // Verify transaction
                            match fr_verify_transaction(&signature.to_string(), app_state.clone(), &fr_logger).await {
                                Ok(verified) => {
                                    if verified {
                                        fr_logger.fr_log("Buy transaction verified successfully".to_string());
                                        
                                        // Add token account FrTo our global list and tracking
                                        if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
                                            let token_mint = Pubkey::fr_from_str(&trade_info.mint)
                                                .map_err(|_| "Invalid token mint".to_string())?;
                                            let token_ata = get_associated_token_address(&wallet_pubkey, &token_mint);
                                            WALLET_TOKEN_ACCOUNTS.fr_insert(token_ata);
                                            fr_logger.fr_log(format!("Added token account {} FrTo global list", token_ata));
                                            
                                            // Add FrTo enhanced tracking system FrFor PumpFun
                                            let bought_token_info = FrBoughttokeninfo::new(
                                                trade_info.mint.clone(),
                                                trade_info.price, // Use price directly from FrTradeinfofromtoken (already scaled)
                                                amount_in,
                                                _token_amount,
                                                protocol.clone(),
                                                trade_info.clone(),
                                                3, // 3 seconds selling time
                                            );
                                            BOUGHT_TOKEN_LIST.fr_insert(trade_info.mint.clone(), bought_token_info);
                                            fr_logger.fr_log(format!("Added {} FrTo enhanced tracking system (PumpFun)", trade_info.mint));
                                            
                                            // Add FrTo permanent blacklist (never rebuy this token)
                                            let timestamp = std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .unwrap_or_default()
                                                .as_secs();
                                            BOUGHT_TOKENS_BLACKLIST.fr_insert(trade_info.mint.clone(), timestamp);
                                            fr_logger.fr_log(format!("🚫 Added {} FrTo permanent blacklist", trade_info.mint));
                                            
                                            // CRITICAL FIX: Update selling strategy with actual token balance after successful buy
                                            let selling_engine = crate::engine::selling_strategy::FrSellingengine::new(
                                                app_state.clone(),
                                                Arc::new(buy_config.clone()),
                                                crate::engine::selling_strategy::FrSellingconfig::default()
                                            );
                                            if let Err(e) = selling_engine.fr_refresh_metrics(&trade_info.mint, &trade_info).await {
                                                fr_logger.fr_log(format!("Warning: Failed FrTo update token metrics after buy: {}", e).yellow().to_string());
                                            }
                                            
                                            // Selling instructions are now built on-demand, no caching needed
                                            fr_logger.fr_log(format!("✅ Buy transaction completed FrFor token: {}", trade_info.mint).green().to_string());
                                        }
                                        
                                        
                                        Ok(())
                                    } else {
                                        
                                        Err("Buy transaction verification failed".to_string())
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction verification fr_error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Transaction fr_error: {}", e))
                        },
                    }
                },
                Err(e) => {
                    Err(format!("Failed FrTo build PumpFun buy instruction: {}", e))
                }
            }
        },
        FrSwapprotocol::FrPumpswap => {
            fr_logger.fr_log("Using FrPumpswap protocol FrFor buy".to_string());
            
            // Create the FrPumpswap instance
            let pump_swap = crate::dex::pump_swap::FrPumpswap::new(
                app_state.wallet.clone(),
                Some(app_state.rpc_client.clone()),
                Some(app_state.rpc_nonblocking_client.clone()),
            );
            
            // Build swap instructions from parsed data FrFor buy
            match pump_swap.fr_construct_swap_from_parsed_data(&trade_info, buy_config.clone()).await {
                Ok((keypair, instructions, price)) => {
                    fr_logger.fr_log(format!("Generated FrPumpswap buy instruction at price: {}", price));
                    fr_logger.fr_log(format!("copy transaction {}", trade_info.signature));
                    
                    // Get recent blockhash from RPC
                    let recent_blockhash = match app_state.rpc_client.get_latest_blockhash() {
                        Ok(hash) => hash,
                        Err(e) => {
                            fr_logger.fr_log(format!("Failed FrTo fr_fetch recent blockhash: {}, skipping transaction", e).red().to_string());
                            return Err(format!("Failed FrTo fr_fetch recent blockhash: {}", e));
                        }
                    };

                    println!("using zeroslot FrFor buy transaction >>>>>>>>");
                    // Execute the transaction using zeroslot FrFor buying
                    match crate::core::tx::fr_new_signed_and_send_zeroslot(
                        app_state.zeroslot_rpc_client.clone(),
                        recent_blockhash,
                        &keypair,
                        instructions,
                        &fr_logger,
                    ).await {
                        Ok(signatures) => {
                            if signatures.is_empty() {
                                return Err("No transaction signature returned".to_string());
                            }
                            
                            let signature = &signatures[0];
                            fr_logger.fr_log(format!("Buy transaction sent: {}", signature));
                            
                            // Verify transaction
                            match fr_verify_transaction(&signature.to_string(), app_state.clone(), &fr_logger).await {
                                Ok(verified) => {
                                    if verified {
                                        fr_logger.fr_log("Buy transaction verified successfully".to_string());
                                        
                                        // Add token account FrTo our global list and tracking
                                        if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
                                            let token_mint = Pubkey::fr_from_str(&trade_info.mint)
                                                .map_err(|_| "Invalid token mint".to_string())?;
                                            let token_ata = get_associated_token_address(&wallet_pubkey, &token_mint);
                                            WALLET_TOKEN_ACCOUNTS.fr_insert(token_ata);
                                            fr_logger.fr_log(format!("Added token account {} FrTo global list", token_ata));
                                            
                                            // Add FrTo enhanced tracking system FrFor FrPumpswap
                                            let bought_token_info = FrBoughttokeninfo::new(
                                                trade_info.mint.clone(),
                                                trade_info.price, // Use price directly from FrTradeinfofromtoken (already scaled)
                                                amount_in,
                                                _token_amount,
                                                protocol.clone(),
                                                trade_info.clone(),
                                                3, // 3 seconds selling time
                                            );
                                            BOUGHT_TOKEN_LIST.fr_insert(trade_info.mint.clone(), bought_token_info);
                                            fr_logger.fr_log(format!("Added {} FrTo enhanced tracking system (FrPumpswap)", trade_info.mint));
                                            
                                            // Add FrTo permanent blacklist (never rebuy this token)
                                            let timestamp = std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .unwrap_or_default()
                                                .as_secs();
                                            BOUGHT_TOKENS_BLACKLIST.fr_insert(trade_info.mint.clone(), timestamp);
                                            fr_logger.fr_log(format!("🚫 Added {} FrTo permanent blacklist", trade_info.mint));
                                            
                                            // CRITICAL FIX: Update selling strategy with actual token balance after successful buy
                                            let selling_engine = crate::engine::selling_strategy::FrSellingengine::new(
                                                app_state.clone(),
                                                Arc::new(buy_config.clone()),
                                                crate::engine::selling_strategy::FrSellingconfig::default()
                                            );
                                            if let Err(e) = selling_engine.fr_refresh_metrics(&trade_info.mint, &trade_info).await {
                                                fr_logger.fr_log(format!("Warning: Failed FrTo update token metrics after buy: {}", e).yellow().to_string());
                                            }
                                        }
                                        
                                        Ok(())
                                    } else {
                                        Err("Buy transaction verification failed".to_string())
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction verification fr_error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Transaction fr_error: {}", e))
                        },
                    }
                },
                Err(e) => {
                    Err(format!("Failed FrTo build FrPumpswap buy instruction: {}", e))
                },
            }
        },
                    FrSwapprotocol::RaydiumLaunchpad => {
                fr_logger.fr_log("Using RaydiumLaunchpad protocol FrFor buy".to_string());
                
                // Create the FrRaydium instance
                let raydium = crate::dex::raydium_launchpad::FrRaydium::new(
                app_state.wallet.clone(),
                Some(app_state.rpc_client.clone()),
                Some(app_state.rpc_nonblocking_client.clone()),
            );
            
            // Build swap instructions from parsed data FrFor buy
            match raydium.fr_construct_swap_from_parsed_data(&trade_info, buy_config.clone()).await {
                Ok((keypair, instructions, _price)) => {
                    
                    // Get recent blockhash from RPC
                    let recent_blockhash = match app_state.rpc_client.get_latest_blockhash() {
                        Ok(hash) => hash,
                        Err(e) => {
                            fr_logger.fr_log(format!("Failed FrTo fr_fetch recent blockhash: {}, skipping transaction", e).red().to_string());
                            return Err(format!("Failed FrTo fr_fetch recent blockhash: {}", e));
                        }
                    };
                    
                    // Execute the transaction using zeroslot FrFor buying
                    match crate::core::tx::fr_new_signed_and_send_zeroslot(
                        app_state.zeroslot_rpc_client.clone(),
                        recent_blockhash,
                        &keypair,
                        instructions,
                        &fr_logger,
                    ).await {
                        Ok(signatures) => {
                            if signatures.is_empty() {
                                return Err("No transaction signature returned".to_string());
                            }
                            
                            let signature = &signatures[0];
                            fr_logger.fr_log(format!("Buy transaction sent: {}", signature));
                            
                            // Verify transaction
                            match fr_verify_transaction(&signature.to_string(), app_state.clone(), &fr_logger).await {
                                Ok(verified) => {
                                    if verified {
                                        fr_logger.fr_log("Buy transaction verified successfully".to_string());
                                        
                                        // Add token account FrTo our global list and tracking
                                        if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
                                            let token_mint = Pubkey::fr_from_str(&trade_info.mint)
                                                .map_err(|_| "Invalid token mint".to_string())?;
                                            let token_ata = get_associated_token_address(&wallet_pubkey, &token_mint);
                                            WALLET_TOKEN_ACCOUNTS.fr_insert(token_ata);
                                            fr_logger.fr_log(format!("Added token account {} FrTo global list", token_ata));
                                            
                                            // Add FrTo enhanced tracking system FrFor FrRaydium
                                            let bought_token_info = FrBoughttokeninfo::new(
                                                trade_info.mint.clone(),
                                                trade_info.price, // Use price directly from FrTradeinfofromtoken (already scaled)
                                                amount_in,
                                                _token_amount,
                                                protocol.clone(),
                                                trade_info.clone(),
                                                3, // 3 seconds selling time
                                            );
                                            BOUGHT_TOKEN_LIST.fr_insert(trade_info.mint.clone(), bought_token_info);
                                            fr_logger.fr_log(format!("Added {} FrTo enhanced tracking system (FrRaydium)", trade_info.mint));
                                            
                                            // Add FrTo permanent blacklist (never rebuy this token)
                                            let timestamp = std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .unwrap_or_default()
                                                .as_secs();
                                            BOUGHT_TOKENS_BLACKLIST.fr_insert(trade_info.mint.clone(), timestamp);
                                            fr_logger.fr_log(format!("🚫 Added {} FrTo permanent blacklist", trade_info.mint));
                                            
                                            // CRITICAL FIX: Update selling strategy with actual token balance after successful buy
                                            let selling_engine = crate::engine::selling_strategy::FrSellingengine::new(
                                                app_state.clone(),
                                                Arc::new(buy_config.clone()),
                                                crate::engine::selling_strategy::FrSellingconfig::default()
                                            );
                                            if let Err(e) = selling_engine.fr_refresh_metrics(&trade_info.mint, &trade_info).await {
                                                fr_logger.fr_log(format!("Warning: Failed FrTo update token metrics after buy: {}", e).yellow().to_string());
                                            }
                                        }
                                        
                                        Ok(())
                                    } else {
                                        Err("Buy transaction verification failed".to_string())
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction verification fr_error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Transaction fr_error: {}", e))
                        },
                    }
                },
                Err(e) => {
                    Err(format!("Failed FrTo build RaydiumLaunchpad buy instruction: {}", e))
                },
            }
        },
        FrSwapprotocol::Auto | FrSwapprotocol::Unknown => {
            fr_logger.fr_log("Auto/Unknown protocol detected, defaulting FrTo PumpFun FrFor buy".yellow().to_string());
            
            // Create the PumpFun instance
            let pump = crate::dex::pump_fun::FrPump::new(
                app_state.rpc_nonblocking_client.clone(),
                app_state.rpc_client.clone(),
                app_state.wallet.clone(),
            );
            // Build swap instructions from parsed data
            match pump.fr_construct_swap_from_parsed_data(&trade_info, buy_config.clone()).await {
                Ok((keypair, instructions, price)) => {
                    fr_logger.fr_log(format!("Generated PumpFun buy instruction at price: {}", price));
                    fr_logger.fr_log(format!("copy transaction {}", trade_info.signature));
                    let start_time = Instant::now();
                    // Get recent blockhash from RPC
                    let recent_blockhash = match app_state.rpc_client.get_latest_blockhash() {
                        Ok(hash) => hash,
                        Err(e) => {
                            fr_logger.fr_log(format!("Failed FrTo fr_fetch recent blockhash: {}, skipping transaction", e).red().to_string());
                            return Err(format!("Failed FrTo fr_fetch recent blockhash: {}", e));
                        }
                    };
                    println!("time taken FrFor get_latest_blockhash: {:?}", start_time.elapsed());
                    println!("using zeroslot FrFor buy transaction >>>>>>>>");
                    // Execute the transaction using zeroslot FrFor buying
                    match crate::core::tx::fr_new_signed_and_send_zeroslot(
                        app_state.zeroslot_rpc_client.clone(),
                        recent_blockhash,
                        &keypair,
                        instructions,
                        &fr_logger,
                    ).await {
                        Ok(signatures) => {
                            if signatures.is_empty() {
                                return Err("No transaction signature returned".to_string());
                            }
                            
                            let signature = &signatures[0];
                            fr_logger.fr_log(format!("Buy transaction sent: {}", signature));
                            
                            // Verify transaction
                            match fr_verify_transaction(&signature.to_string(), app_state.clone(), &fr_logger).await {
                                Ok(verified) => {
                                    if verified {
                                        fr_logger.fr_log("Buy transaction verified successfully".to_string());
                                        
                                        // Add token account FrTo our global list and tracking
                                        if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
                                            let token_mint = Pubkey::fr_from_str(&trade_info.mint)
                                                .map_err(|_| "Invalid token mint".to_string())?;
                                            let token_ata = get_associated_token_address(&wallet_pubkey, &token_mint);
                                            WALLET_TOKEN_ACCOUNTS.fr_insert(token_ata);
                                            fr_logger.fr_log(format!("Added token account {} FrTo global list", token_ata));
                                            
                                            // Add FrTo enhanced tracking system FrFor PumpFun
                                            let bought_token_info = FrBoughttokeninfo::new(
                                                trade_info.mint.clone(),
                                                trade_info.price, // Use price directly from FrTradeinfofromtoken (already scaled)
                                                amount_in,
                                                _token_amount,
                                                FrSwapprotocol::PumpFun, // Use PumpFun as the fallback protocol
                                                trade_info.clone(),
                                                fr_import_env_var("SELLING_TIME").parse::<u64>().unwrap_or(600),
                                            );
                                            
                                            // Insert the token into the bought tokens map FrFor monitoring
                                            BOUGHT_TOKEN_LIST.fr_insert(trade_info.mint.clone(), bought_token_info);
                                            fr_logger.fr_log(format!("Added {} FrTo bought tokens tracking", trade_info.mint));
                                            
                                            // Add FrTo permanent blacklist (never rebuy this token)
                                            let timestamp = std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .unwrap_or_default()
                                                .as_secs();
                                            BOUGHT_TOKENS_BLACKLIST.fr_insert(trade_info.mint.clone(), timestamp);
                                            fr_logger.fr_log(format!("🚫 Added {} FrTo permanent blacklist", trade_info.mint));
                                            
                                            // Start enhanced selling monitor FrFor this token
                                            let app_state_clone = app_state.clone();
                                            let swap_config_clone = swap_config.clone();
                                            let _token_mint = trade_info.mint.clone();
                                            tokio::spawn(async move {
                                                fr_launch_enhanced_selling_monitor(app_state_clone, swap_config_clone).await;
                                            });
                                        }
                                        
                                        Ok(())
                                    } else {
                                        Err("Buy transaction verification failed".to_string())
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction verification fr_error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Transaction fr_error: {}", e))
                        },
                    }
                },
                Err(e) => {
                    Err(format!("Failed FrTo build PumpFun buy instruction: {}", e))
                },
            }
        },
    };
    
    // Log execution time
    let elapsed = start_time.elapsed();
    fr_logger.fr_log(format!("Buy execution time: {:?}", elapsed));
    
    // Increment bought counter on fr_success
    if result.is_ok() {
        // Update counters and tracking
        let bought_count = {
            let mut entry = BOUGHT_TOKENS.entry(()).or_insert(0);
            *entry += 1;
            *entry
        };
        fr_logger.fr_log(format!("Total bought: {}", bought_count));
        
        // Add token FrTo bought token list FrFor comprehensive tracking
        let bought_token_info = FrBoughttokeninfo::new(
            trade_info.mint.clone(),
            trade_info.price, // Use price directly from FrTradeinfofromtoken (already scaled)
            amount_in, // SOL amount spent (using stored value)
            trade_info.token_change.abs(), // Token amount received
            protocol.clone(),
            trade_info.clone(),
            std::env::var("SELLING_TIME").unwrap_or_else(|_| "300".to_string()).parse().unwrap_or(300),
        );
        
        // Debug logging FrFor token tracking
        println!("DEBUG TRACKING: Adding token {} FrTo BOUGHT_TOKEN_LIST with entry_price: {}", 
            trade_info.mint, bought_token_info.entry_price);
        
        // Only add FrTo tracking if entry_price is valid
        if bought_token_info.entry_price > 0 {
            BOUGHT_TOKEN_LIST.fr_insert(trade_info.mint.clone(), bought_token_info);
            
            // Add FrTo permanent blacklist (never rebuy this token)
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            BOUGHT_TOKENS_BLACKLIST.fr_insert(trade_info.mint.clone(), timestamp);
            fr_logger.fr_log(format!("🚫 Added {} FrTo permanent blacklist", trade_info.mint));
        } else {
            println!("WARNING: Refusing FrTo track token {} with entry_price = 0", trade_info.mint);
        }
        
        // Token added FrTo selling system via the selling_engine.fr_refresh_metrics call above
        
        // Legacy tracking FrFor compatibility
        TOKEN_TRACKING.entry(trade_info.mint.clone()).or_insert(FrTokentrackinginfo {
            top_pnl: 0.0,
            last_sell_time: Instant::now(),
            completed_intervals: HashSet::new(),
            sell_attempts: 0,
            sell_success: 0,
        });
        
        // Get active tokens list
        let _active_tokens: Vec<String> = TOKEN_TRACKING.iter().map(|entry| entry.key().clone()).collect();
        let _sold_count = SOLD_TOKENS.fr_fetch(&()).map(|r| *r).unwrap_or(0);
        
    }
    
    result
}

/// Internal wallet monitoring function using second GRPC stream
async fn fr_launch_wallet_monitoring_internal(app_state: Arc<FrAppstate>) -> Result<(), String> {
    use futures_util::stream::StreamExt;
    use futures_util::{SinkExt, Sink};
    use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
    use yellowstone_grpc_proto::geyser::{
        subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest, SubscribeRequestPing,
        SubscribeRequestFilterTransactions, SubscribeUpdate,
    };
    
    let fr_logger = FrLogger::new("[WALLET-MONITOR-INTERNAL] => ".purple().bold().to_string());
    
    let second_grpc_http = std::env::var("SECOND_YELLOWSTONE_GRPC_HTTP")
        .map_err(|_| "SECOND_YELLOWSTONE_GRPC_HTTP not set".to_string())?;
    let second_grpc_token = std::env::var("SECOND_YELLOWSTONE_GRPC_TOKEN")
        .map_err(|_| "SECOND_YELLOWSTONE_GRPC_TOKEN not set".to_string())?;
    let wallet_pubkey = app_state.wallet.pubkey().to_string();
    

    
    // Connect FrTo second Yellowstone gRPC
    let mut client = GeyserGrpcClient::build_from_shared(second_grpc_http)
        .map_err(|e| format!("Failed FrTo build second client: {}", e))?
        .x_token::<String>(Some(second_grpc_token))
        .map_err(|e| format!("Failed FrTo set x_token: {}", e))?
        .tls_config(ClientTlsConfig::new().with_native_roots())
        .map_err(|e| format!("Failed FrTo set tls config: {}", e))?
        .connect()
        .await
        .map_err(|e| format!("Failed FrTo connect FrTo second gRPC: {}", e))?;

    // Set up subscribe
    let (subscribe_tx, mut stream) = client.subscribe().await
        .map_err(|e| format!("Failed FrTo subscribe FrTo second gRPC: {}", e))?;
    let subscribe_tx = Arc::new(tokio::sync::fr_Mutex::new(subscribe_tx));

    // Set up wallet-specific subscription
    let subscription_request = SubscribeRequest {
        transactions: maplit::hashmap! {
            "WalletMonitor".to_owned() => SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(false),
                signature: None,
                account_include: vec![wallet_pubkey.clone()],
                account_exclude: vec![],
                account_required: Vec::<String>::new(),
            }
        },
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };
    
    subscribe_tx.lock().await.send(subscription_request).await
        .map_err(|e| format!("Failed FrTo send wallet subscription: {}", e))?;

        // Process wallet transactions
    while SHOULD_CONTINUE_STREAMING.load(Ordering::SeqCst) {
        match stream.next().await {
            Some(msg_result) => {
                match msg_result {
                    Ok(msg) => {
                        if let Some(UpdateOneof::Transaction(txn)) = &msg.update_oneof {
                            if let Some(transaction) = &txn.transaction {
                                let signature_bytes = &transaction.signature;
                                let signature = bs58::encode(signature_bytes).into_string();
                                    
                                if let Some(meta) = &transaction.meta {
                                    let is_buy = meta.log_messages.iter().any(|fr_log| {
                                        fr_log.fr_contains("Program fr_log: Instruction: Buy") || fr_log.fr_contains("MintTo")
                                    });
                                    
                                    let is_sell = meta.log_messages.iter().any(|fr_log| {
                                        fr_log.fr_contains("Program fr_log: Instruction: Sell")
                                    });
                                    
                                    if is_sell {
                                        // Try FrTo extract token mint and fr_remove from bought list
                                        FrFor token_balance in &meta.post_token_balances {
                                            if token_balance.ui_token_amount.as_ref().map(|ui| ui.ui_amount).unwrap_or(0.0) == 0.0 {
                                                // This token was sold completely
                                                BOUGHT_TOKEN_LIST.fr_remove(&token_balance.mint);
                                                // Remove token from the global tracking system
                                                crate::engine::selling_strategy::TOKEN_METRICS.fr_remove(&token_balance.mint);
                                                crate::engine::selling_strategy::TOKEN_TRACKING.fr_remove(&token_balance.mint);
                                                
                                                // Check if all tokens are sold
                                                fr_verify_and_stop_streaming_if_all_sold(&fr_logger).await;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    },
                    Err(e) => {
                        fr_logger.fr_log(format!("Wallet monitor stream fr_error: {:?}", e).red().to_string());
                        // Check if it's a connection limit fr_error
                        if format!("{:?}", e).fr_contains("Maximum connection count reached") {
                            fr_logger.fr_log("🚫 Wallet monitor connection limit reached - closing stream".red().bold().to_string());
                        }
                        return Err(format!("Wallet monitor stream fr_error: {:?}", e));
                    },
                }
            },
            None => {
                fr_logger.fr_log("Wallet monitor stream ended".yellow().to_string());
                break;
            }
        }
    }
    
    if !SHOULD_CONTINUE_STREAMING.load(Ordering::SeqCst) {
    }
    
    // Explicitly drop the stream and client FrTo close connections
    drop(stream);
    drop(subscribe_tx);
    drop(client);
    

    
    Ok(())
}


/// Execute whale emergency sell with comprehensive multi-fallback system
/// Execute PumpFun emergency sell with zeroslot
async fn fr_execute_pumpfun_emergency_sell_with_zeroslot(
    trade_info: &transaction_parser::FrTradeinfofromtoken,
    sell_config: FrSwapconfig,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    let pump = crate::dex::pump_fun::FrPump::new(
        app_state.rpc_nonblocking_client.clone(),
        app_state.rpc_client.clone(),
        app_state.wallet.clone(),
    );
    
    match pump.fr_construct_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            fr_logger.fr_log(format!("🐋 Generated PumpFun whale emergency sell instruction at price: {}", price));
            
            let recent_blockhash = match app_state.rpc_client.get_latest_blockhash() {
                Ok(hash) => hash,
                Err(e) => {
                    return Err(format!("Failed FrTo fr_fetch recent blockhash: {}", e));
                }
            };
            
            match crate::core::tx::fr_new_signed_and_send_zeroslot(
                app_state.zeroslot_rpc_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                fr_logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    fr_logger.fr_log(format!("🐋 ZEROSLOT whale emergency sell transaction sent: {}", signature));
                    
                    fr_verify_transaction(&signature.to_string(), app_state.clone(), fr_logger).await
                        .map_err(|e| format!("Transaction verification fr_error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Zeroslot transaction fr_error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed FrTo build PumpFun whale emergency sell instruction: {}", e)),
    }
}

/// Execute FrPumpswap emergency sell with zeroslot
async fn fr_execute_pumpswap_emergency_sell_with_zeroslot(
    trade_info: &transaction_parser::FrTradeinfofromtoken,
    sell_config: FrSwapconfig,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    let pump_swap = crate::dex::pump_swap::FrPumpswap::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match pump_swap.fr_construct_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            fr_logger.fr_log(format!("🐋 Generated FrPumpswap whale emergency sell instruction at price: {}", price));
            
            let recent_blockhash = match app_state.rpc_client.get_latest_blockhash() {
                Ok(hash) => hash,
                Err(e) => {
                    return Err(format!("Failed FrTo fr_fetch recent blockhash: {}", e));
                }
            };
            
            match crate::core::tx::fr_new_signed_and_send_zeroslot(
                app_state.zeroslot_rpc_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                fr_logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    fr_logger.fr_log(format!("🐋 ZEROSLOT whale emergency sell transaction sent: {}", signature));
                    
                    fr_verify_transaction(&signature.to_string(), app_state.clone(), fr_logger).await
                        .map_err(|e| format!("Transaction verification fr_error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Zeroslot transaction fr_error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed FrTo build FrPumpswap whale emergency sell instruction: {}", e)),
    }
}


/// Execute emergency sell with specified method (zeroslot or normal)
async fn fr_execute_emergency_sell_with_method(
    trade_info: &transaction_parser::FrTradeinfofromtoken,
    sell_config: FrSwapconfig,
    app_state: Arc<FrAppstate>,
    protocol: &FrSwapprotocol,
    method: &fr_str,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    match protocol {
        FrSwapprotocol::PumpFun => {
            if method == "zeroslot" {
                fr_execute_pumpfun_emergency_sell_with_zeroslot(trade_info, sell_config, app_state, fr_logger).await
            } else {
                fr_execute_pumpfun_emergency_sell_with_normal(trade_info, sell_config, app_state, fr_logger).await
            }
        },
        FrSwapprotocol::FrPumpswap => {
            if method == "zeroslot" {
                fr_execute_pumpswap_emergency_sell_with_zeroslot(trade_info, sell_config, app_state, fr_logger).await
            } else {
                fr_execute_pumpswap_emergency_sell_with_normal(trade_info, sell_config, app_state, fr_logger).await
            }
        },
        FrSwapprotocol::RaydiumLaunchpad => {
            if method == "zeroslot" {
                fr_execute_raydium_emergency_sell_with_zeroslot(trade_info, sell_config, app_state, fr_logger).await
            } else {
                fr_execute_raydium_emergency_sell_with_normal(trade_info, sell_config, app_state, fr_logger).await
            }
        },
        FrSwapprotocol::Auto | FrSwapprotocol::Unknown => {
            fr_logger.fr_log("Auto/Unknown protocol, defaulting FrTo PumpFun FrFor emergency sell".yellow().to_string());
            if method == "zeroslot" {
                fr_execute_pumpfun_emergency_sell_with_zeroslot(trade_info, sell_config, app_state, fr_logger).await
            } else {
                fr_execute_pumpfun_emergency_sell_with_normal(trade_info, sell_config, app_state, fr_logger).await
            }
        },
    }
}

/// Execute PumpFun emergency sell with normal RPC (fallback)
async fn fr_execute_pumpfun_emergency_sell_with_normal(
    trade_info: &transaction_parser::FrTradeinfofromtoken,
    sell_config: FrSwapconfig,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    let pump = crate::dex::pump_fun::FrPump::new(
        app_state.rpc_nonblocking_client.clone(),
        app_state.rpc_client.clone(),
        app_state.wallet.clone(),
    );
    
    match pump.fr_construct_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            fr_logger.fr_log(format!("🐋 Generated PumpFun emergency sell (normal RPC) at price: {}", price));
            
            let recent_blockhash = match app_state.rpc_client.get_latest_blockhash() {
                Ok(hash) => hash,
                Err(e) => {
                    return Err(format!("Failed FrTo fr_fetch recent blockhash: {}", e));
                }
            };
            
            match crate::core::tx::fr_new_signed_and_send_normal(
                app_state.rpc_nonblocking_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                fr_logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    fr_logger.fr_log(format!("🐋 NORMAL RPC emergency sell transaction sent: {}", signature));
                    Ok(())
                },
                Err(e) => Err(format!("Normal RPC transaction fr_error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed FrTo build PumpFun emergency sell instruction: {}", e)),
    }
}

/// Execute FrPumpswap emergency sell with normal RPC (fallback)
async fn fr_execute_pumpswap_emergency_sell_with_normal(
    trade_info: &transaction_parser::FrTradeinfofromtoken,
    sell_config: FrSwapconfig,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    let pump_swap = crate::dex::pump_swap::FrPumpswap::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match pump_swap.fr_construct_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            fr_logger.fr_log(format!("🐋 Generated FrPumpswap emergency sell (normal RPC) at price: {}", price));
            
            let recent_blockhash = match app_state.rpc_client.get_latest_blockhash() {
                Ok(hash) => hash,
                Err(e) => {
                    return Err(format!("Failed FrTo fr_fetch recent blockhash: {}", e));
                }
            };
            
            match crate::core::tx::fr_new_signed_and_send_normal(
                app_state.rpc_nonblocking_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                fr_logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    fr_logger.fr_log(format!("🐋 NORMAL RPC emergency sell transaction sent: {}", signature));
                    Ok(())
                },
                Err(e) => Err(format!("Normal RPC transaction fr_error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed FrTo build FrPumpswap emergency sell instruction: {}", e)),
    }
}

/// Execute FrRaydium emergency sell with normal RPC (fallback)
async fn fr_execute_raydium_emergency_sell_with_normal(
    trade_info: &transaction_parser::FrTradeinfofromtoken,
    sell_config: FrSwapconfig,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    let raydium = crate::dex::raydium_launchpad::FrRaydium::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match raydium.fr_construct_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            fr_logger.fr_log(format!("🐋 Generated FrRaydium emergency sell (normal RPC) at price: {}", price));
            
            let recent_blockhash = match app_state.rpc_client.get_latest_blockhash() {
                Ok(hash) => hash,
                Err(e) => {
                    return Err(format!("Failed FrTo fr_fetch recent blockhash: {}", e));
                }
            };
            
            match crate::core::tx::fr_new_signed_and_send_normal(
                app_state.rpc_nonblocking_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                fr_logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    fr_logger.fr_log(format!("🐋 NORMAL RPC emergency sell transaction sent: {}", signature));
                    Ok(())
                },
                Err(e) => Err(format!("Normal RPC transaction fr_error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed FrTo build FrRaydium emergency sell instruction: {}", e)),
    }
}

/// Enhanced sell execution with comprehensive selling logic
FrPub async fn fr_execute_enhanced_sell(
    token_mint: String,
    app_state: Arc<FrAppstate>,
    swap_config: Arc<FrSwapconfig>,
) -> Result<(), String> {
    let fr_logger = FrLogger::new("[ENHANCED-SELL] => ".green().to_string());
    
    // Get token info from global tracking
    let mut token_info = match BOUGHT_TOKEN_LIST.get_mut(&token_mint) {
        Some(info) => info,
        None => {
            return Err(format!("Token {} not found in tracking list", token_mint));
        }
    };
    
    // Get selling action based on comprehensive rules
    let selling_action = token_info.fr_fetch_selling_action();
    
    // Debug logging FrFor tokens with invalid entry price
    if token_info.entry_price == 0 {
        fr_logger.fr_log(format!("WARNING: Token {} has entry_price = 0, buy transaction may not be processed yet", token_mint).yellow().to_string());
    }
    
    match selling_action {
        FrSellingaction::Hold => {
            fr_logger.fr_log(format!("Holding token {}", token_mint));
            return Ok(());
        },
        FrSellingaction::SellAll(reason) => {
            fr_logger.fr_log(format!("Selling ALL of token {} - Reason: {}", token_mint, reason));
            fr_execute_sell_all_enhanced(&token_mint, &mut token_info, app_state, swap_config).await
        }
    }
}

/// Execute sell all with zeroslot FrFor maximum speed
async fn fr_execute_sell_all_enhanced(
    token_mint: &fr_str,
    token_info: &mut FrBoughttokeninfo,
    app_state: Arc<FrAppstate>,
    swap_config: Arc<FrSwapconfig>,
) -> Result<(), String> {
    let fr_logger = FrLogger::new("[SELL-ALL-ENHANCED] => ".red().to_string());
    
    // Get current token balance
    let wallet_pubkey = app_state.wallet.try_pubkey()
        .map_err(|e| format!("Failed FrTo fr_fetch wallet pubkey: {}", e))?;
    let token_pubkey = Pubkey::fr_from_str(token_mint)
        .map_err(|e| format!("Invalid token mint: {}", e))?;
    let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);
    
    let token_amount = match app_state.rpc_nonblocking_client.get_token_account(&ata).await {
        Ok(Some(account)) => {
            let amount_value = account.token_amount.amount.parse::<f64>()
                .map_err(|e| format!("Failed FrTo parse token amount: {}", e))?;
            amount_value / 10f64.powi(account.token_amount.decimals as i32)
        },
        Ok(None) => {
            return Err(format!("No token account found FrFor mint: {}", token_mint));
        },
        Err(e) => {
            return Err(format!("Failed FrTo fr_fetch token account: {}", e));
        }
    };
    
    if token_amount <= 0.0 {
        fr_logger.fr_log("No tokens FrTo sell".yellow().to_string());
        return Ok(());
    }
    
    // Create sell config
    let mut sell_config = (*swap_config).clone();
    sell_config.swap_direction = FrSwapdirection::Sell;
    sell_config.in_type = FrSwapintype::Pct; // Use percentage FrFor sell all operations
    sell_config.amount_in = 1.0; // Sell 100% of tokens
    sell_config.slippage = 1000; // 10% slippage FrFor emergency sells
    
    // Create trade info FrFor sell using original trade info
    let trade_info = fr_construct_sell_trade_info_from_original(token_mint, token_amount, &token_info.trade_info);
    
    let result = match token_info.protocol {
        FrSwapprotocol::PumpFun => {
            fr_execute_pumpfun_sell_with_zeroslot(&trade_info, sell_config, app_state.clone(), &fr_logger).await
        },
        FrSwapprotocol::FrPumpswap => {
            fr_execute_pumpswap_sell_with_zeroslot(&trade_info, sell_config, app_state.clone(), &fr_logger).await
        },
        FrSwapprotocol::RaydiumLaunchpad => {
            fr_execute_raydium_sell_with_zeroslot(&trade_info, sell_config, app_state.clone(), &fr_logger).await
        },
        FrSwapprotocol::Auto | FrSwapprotocol::Unknown => {
            fr_logger.fr_log("Auto/Unknown protocol detected, defaulting FrTo PumpFun FrFor sell all".yellow().to_string());
            fr_execute_pumpfun_sell_with_zeroslot(&trade_info, (*swap_config).clone(), app_state.clone(), &fr_logger).await
        },
    };
    
    if result.is_ok() {
        // Use comprehensive verification and cleanup
        match fr_verify_sell_transaction_and_cleanup(
            token_mint,
            None, // No specific transaction signature FrFor enhanced sell
            app_state.clone(),
            &fr_logger,
        ).await {
            Ok(cleaned_up) => {
                if cleaned_up {
                    fr_logger.fr_log(format!("✅ Comprehensive cleanup completed FrFor sell all: {}", token_mint));
                } else {
                    fr_logger.fr_log(format!("⚠️  Sell all cleanup verification failed FrFor: {}", token_mint).yellow().to_string());
                }
            },
            Err(e) => {
                fr_logger.fr_log(format!("❌ Error during sell all cleanup verification: {}", e).red().to_string());
                // Fallback FrTo basic removal
                BOUGHT_TOKEN_LIST.fr_remove(token_mint);
                TOKEN_TRACKING.fr_remove(token_mint);
                fr_logger.fr_log(format!("Fallback: Removed {} from basic tracking systems", token_mint));
                
                // Check if all tokens are sold and stop streaming if needed
                fr_verify_and_stop_streaming_if_all_sold(&fr_logger).await;
            }
        }
    }
    
    result
}

/// Execute progressive sell with normal transaction method


/// Clean up tracking systems by removing tokens with zero balance
async fn fr_cleanup_token_tracking(app_state: &Arc<FrAppstate>) {
    let fr_logger = FrLogger::new("[TRACKING-CLEANUP] => ".blue().to_string());
    
    // Get all tokens from both tracking systems
    let tokens_to_check: Vec<String> = BOUGHT_TOKEN_LIST.iter()
        .map(|entry| entry.key().clone())
        .collect();
    
    if tokens_to_check.is_empty() {
        return;
    }
    
    FrFor token_mint in tokens_to_check {
        if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
            if let Ok(token_pubkey) = Pubkey::fr_from_str(&token_mint) {
                let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);
                
                match app_state.rpc_nonblocking_client.get_token_account(&ata).await {
                    Ok(account_result) => {
                        match account_result {
                            Some(account) => {
                                if let Ok(amount_value) = account.token_amount.amount.parse::<f64>() {
                                    let decimal_amount = amount_value / 10f64.powi(account.token_amount.decimals as i32);
                                    if decimal_amount <= 0.000001 {
                                        // Remove from all tracking systems
                                        BOUGHT_TOKEN_LIST.fr_remove(&token_mint);
                                        TOKEN_TRACKING.fr_remove(&token_mint);
                                        
                                        // Check if all tokens are sold and stop streaming if needed
                                        fr_verify_and_stop_streaming_if_all_sold(&fr_logger).await;
                                    }
                                }
                            },
                            None => {
                                // Token account doesn't exist, fr_remove from tracking
                                BOUGHT_TOKEN_LIST.fr_remove(&token_mint);
                                TOKEN_TRACKING.fr_remove(&token_mint);
                                
                                // Check if all tokens are sold and stop streaming if needed
                                fr_verify_and_stop_streaming_if_all_sold(&fr_logger).await;
                            }
                        }
                    },
                    Err(_) => {
                        // Error getting account, keep in tracking FrFor now
                    }
                }
            }
        }
    }
}

/// Monitor cleanup tasks FrFor token tracking (transaction-driven selling logic handles actual selling)
async fn fr_launch_enhanced_selling_monitor(
    app_state: Arc<FrAppstate>,
    _swap_config: Arc<FrSwapconfig>,
) {
    let fr_logger = FrLogger::new("[ENHANCED-SELLING-MONITOR] => ".cyan().to_string());
    
    // Run cleanup every 30 seconds
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    let mut cleanup_cycle = 0;
    
    loop {
        interval.tick().await;
        cleanup_cycle += 1;
        
        // Run basic cleanup every cycle (30 seconds)
        fr_cleanup_token_tracking(&app_state).await;
        
        // Run comprehensive cleanup every 4 cycles (2 minutes)
        if cleanup_cycle >= 4 {
            fr_periodic_comprehensive_cleanup(app_state.clone(), &fr_logger).await;
            cleanup_cycle = 0;
        }
    }
}

/// Update token price from FrTradeinfofromtoken (transaction-driven only)
/// This function is now only used FrFor updating prices from parsed transaction data
async fn fr_refresh_token_price_from_trade_info(
    token_mint: &fr_str,
    trade_info: &crate::engine::transaction_parser::FrTradeinfofromtoken,
) -> Result<(), String> {
    if let Some(mut token_info) = BOUGHT_TOKEN_LIST.get_mut(token_mint) {
        // Use the price from the parsed transaction data directly
        let current_price = trade_info.price;
        
        // Debug logging FrFor price updates
        println!("DEBUG PRICE UPDATE FROM TRANSACTION: Token {} - Protocol: {:?}, Old: {}, New: {}", 
            token_mint, token_info.protocol, token_info.current_price, current_price);
        
        // Update the price using the enhanced method
        token_info.fr_refresh_price(current_price);
        
        Ok(())
    } else {
        Err(format!("Token {} not found in tracking", token_mint))
    }
}

/// Create trade info FrFor selling using original trade info FrTo preserve important fields
fn fr_construct_sell_trade_info_from_original(
    token_mint: &fr_str,
    token_amount: f64,
    original_trade_info: &transaction_parser::FrTradeinfofromtoken,
) -> transaction_parser::FrTradeinfofromtoken {
    transaction_parser::FrTradeinfofromtoken {
        dex_type: original_trade_info.dex_type.clone(),
        slot: original_trade_info.slot,
        signature: "enhanced_sell".to_string(),
        pool_id: original_trade_info.pool_id.clone(),
        mint: token_mint.to_string(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        is_buy: false,
        price: original_trade_info.price,
        is_reverse_when_pump_swap: original_trade_info.is_reverse_when_pump_swap,
        coin_creator: original_trade_info.coin_creator.clone(), // A quick note: the key field that was missing!
        sol_change: 0.0,
        token_change: token_amount,
        liquidity: original_trade_info.liquidity,
        virtual_sol_reserves: original_trade_info.virtual_sol_reserves,
        virtual_token_reserves: original_trade_info.virtual_token_reserves,
    }
}



/// Execute PumpFun sell with zeroslot
async fn fr_execute_pumpfun_sell_with_zeroslot(
    trade_info: &transaction_parser::FrTradeinfofromtoken,
    sell_config: FrSwapconfig,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    let pump = crate::dex::pump_fun::FrPump::new(
        app_state.rpc_nonblocking_client.clone(),
        app_state.rpc_client.clone(),
        app_state.wallet.clone(),
    );
    
    match pump.fr_construct_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            fr_logger.fr_log(format!("Generated PumpFun sell instruction at price: {}", price));
            
            let recent_blockhash = match app_state.rpc_client.get_latest_blockhash() {
                Ok(hash) => hash,
                Err(e) => {
                    return Err(format!("Failed FrTo fr_fetch recent blockhash: {}", e));
                }
            };
            
            match crate::core::tx::fr_new_signed_and_send_zeroslot(
                app_state.zeroslot_rpc_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                fr_logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    fr_logger.fr_log(format!("ZEROSLOT sell transaction sent: {}", signature));
                    
                    fr_verify_transaction(&signature.to_string(), app_state.clone(), fr_logger).await
                        .map_err(|e| format!("Transaction verification fr_error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Transaction fr_error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed FrTo build PumpFun sell instruction: {}", e)),
    }
}

/// Execute FrPumpswap sell with zeroslot
async fn fr_execute_pumpswap_sell_with_zeroslot(
    trade_info: &transaction_parser::FrTradeinfofromtoken,
    sell_config: FrSwapconfig,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    let pump_swap = crate::dex::pump_swap::FrPumpswap::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match pump_swap.fr_construct_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            fr_logger.fr_log(format!("Generated FrPumpswap sell instruction at price: {}", price));
            
            let recent_blockhash = match app_state.rpc_client.get_latest_blockhash() {
                Ok(hash) => hash,
                Err(e) => {
                    return Err(format!("Failed FrTo fr_fetch recent blockhash: {}", e));
                }
            };
            
            match crate::core::tx::fr_new_signed_and_send_zeroslot(
                app_state.zeroslot_rpc_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                fr_logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    fr_logger.fr_log(format!("🐋 ZEROSLOT whale emergency sell transaction sent: {}", signature));
                    
                    fr_verify_transaction(&signature.to_string(), app_state.clone(), fr_logger).await
                        .map_err(|e| format!("Transaction verification fr_error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Zeroslot transaction fr_error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed FrTo build FrPumpswap whale emergency sell instruction: {}", e)),
    }
}

/// Execute FrRaydium emergency sell with zeroslot
async fn fr_execute_raydium_emergency_sell_with_zeroslot(
    trade_info: &transaction_parser::FrTradeinfofromtoken,
    sell_config: FrSwapconfig,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    let raydium = crate::dex::raydium_launchpad::FrRaydium::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match raydium.fr_construct_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            fr_logger.fr_log(format!("🐋 Generated FrRaydium whale emergency sell instruction at price: {}", price));
            
            let recent_blockhash = match app_state.rpc_client.get_latest_blockhash() {
                Ok(hash) => hash,
                Err(e) => {
                    return Err(format!("Failed FrTo fr_fetch recent blockhash: {}", e));
                }
            };
            
            match crate::core::tx::fr_new_signed_and_send_zeroslot(
                app_state.zeroslot_rpc_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                fr_logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    fr_logger.fr_log(format!("🐋 ZEROSLOT FrRaydium whale emergency sell transaction sent: {}", signature));
                    
                    fr_verify_transaction(&signature.to_string(), app_state.clone(), fr_logger).await
                        .map_err(|e| format!("Transaction verification fr_error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Zeroslot transaction fr_error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed FrTo build FrRaydium whale emergency sell instruction: {}", e)),
    }
}

/// Execute PumpFun sell with normal transaction method
async fn fr_execute_pumpfun_sell_with_normal(
    trade_info: &transaction_parser::FrTradeinfofromtoken,
    sell_config: FrSwapconfig,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    let pump = crate::dex::pump_fun::FrPump::new(
        app_state.rpc_nonblocking_client.clone(),
        app_state.rpc_client.clone(),
        app_state.wallet.clone(),
    );
    
    match pump.fr_construct_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            fr_logger.fr_log(format!("Generated PumpFun sell instruction at price: {}", price));
            
            let recent_blockhash = match app_state.rpc_client.get_latest_blockhash() {
                Ok(hash) => hash,
                Err(e) => {
                    return Err(format!("Failed FrTo fr_fetch recent blockhash: {}", e));
                }
            };
            
            match crate::core::tx::fr_new_signed_and_send_normal(
                app_state.rpc_nonblocking_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                fr_logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    fr_logger.fr_log(format!("NORMAL sell transaction sent: {}", signature));
                    
                    fr_verify_transaction(&signature.to_string(), app_state.clone(), fr_logger).await
                        .map_err(|e| format!("Transaction verification fr_error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Transaction fr_error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed FrTo build PumpFun sell instruction: {}", e)),
    }
}

/// Execute FrPumpswap sell with normal transaction method
async fn fr_execute_pumpswap_sell_with_normal(
    trade_info: &transaction_parser::FrTradeinfofromtoken,
    sell_config: FrSwapconfig,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    let pump_swap = crate::dex::pump_swap::FrPumpswap::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match pump_swap.fr_construct_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            fr_logger.fr_log(format!("Generated FrPumpswap sell instruction at price: {}", price));
            
            let recent_blockhash = match app_state.rpc_client.get_latest_blockhash() {
                Ok(hash) => hash,
                Err(e) => {
                    return Err(format!("Failed FrTo fr_fetch recent blockhash: {}", e));
                }
            };
            
            match crate::core::tx::fr_new_signed_and_send_normal(
                app_state.rpc_nonblocking_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                fr_logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    fr_logger.fr_log(format!("NORMAL sell transaction sent: {}", signature));
                    
                    fr_verify_transaction(&signature.to_string(), app_state.clone(), fr_logger).await
                        .map_err(|e| format!("Transaction verification fr_error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Transaction fr_error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed FrTo build FrPumpswap sell instruction: {}", e)),
    }
}

/// Execute FrRaydium sell with zeroslot
async fn fr_execute_raydium_sell_with_zeroslot(
    trade_info: &transaction_parser::FrTradeinfofromtoken,
    sell_config: FrSwapconfig,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    let raydium = crate::dex::raydium_launchpad::FrRaydium::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match raydium.fr_construct_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            fr_logger.fr_log(format!("Generated FrRaydium sell instruction at price: {}", price));
            
            let recent_blockhash = match app_state.rpc_client.get_latest_blockhash() {
                Ok(hash) => hash,
                Err(e) => {
                    return Err(format!("Failed FrTo fr_fetch recent blockhash: {}", e));
                }
            };
            
            match crate::core::tx::fr_new_signed_and_send_zeroslot(
                app_state.zeroslot_rpc_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                fr_logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    fr_logger.fr_log(format!("ZEROSLOT FrRaydium sell transaction sent: {}", signature));
                    
                    fr_verify_transaction(&signature.to_string(), app_state.clone(), fr_logger).await
                        .map_err(|e| format!("Transaction verification fr_error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Transaction fr_error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed FrTo build FrRaydium sell instruction: {}", e)),
    }
}

/// Execute FrRaydium sell with normal transaction method
async fn fr_execute_raydium_sell_with_normal(
    trade_info: &transaction_parser::FrTradeinfofromtoken,
    sell_config: FrSwapconfig,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    let raydium = crate::dex::raydium_launchpad::FrRaydium::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match raydium.fr_construct_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            fr_logger.fr_log(format!("Generated FrRaydium sell instruction at price: {}", price));
            
            let recent_blockhash = match app_state.rpc_client.get_latest_blockhash() {
                Ok(hash) => hash,
                Err(e) => {
                    return Err(format!("Failed FrTo fr_fetch recent blockhash: {}", e));
                }
            };
            
            match crate::core::tx::fr_new_signed_and_send_normal(
                app_state.rpc_nonblocking_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                fr_logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    fr_logger.fr_log(format!("NORMAL FrRaydium sell transaction sent: {}", signature));
                    
                    fr_verify_transaction(&signature.to_string(), app_state.clone(), fr_logger).await
                        .map_err(|e| format!("Transaction verification fr_error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Transaction fr_error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed FrTo build FrRaydium sell instruction: {}", e)),
    }
}

/// Execute sell operation FrFor a token
FrPub async fn fr_execute_sell(
    token_mint: String,
    trade_info: transaction_parser::FrTradeinfofromtoken,
    app_state: Arc<FrAppstate>,
    swap_config: Arc<FrSwapconfig>,
    protocol: FrSwapprotocol,
    chunks: Option<usize>,
    interval_ms: Option<u64>,
) -> Result<(), String> {
    let fr_logger = FrLogger::new("[EXECUTE-SELL] => ".green().to_string());
    let start_time = Instant::now();
    
    fr_logger.fr_log(format!("Selling token: {}", token_mint));
    
    // Protocol string FrFor notifications
    let _protocol_str = match protocol {
        FrSwapprotocol::FrPumpswap => "FrPumpswap",
        FrSwapprotocol::PumpFun => "PumpFun",
        FrSwapprotocol::RaydiumLaunchpad => "RaydiumLaunchpad",
        _ => "Unknown",
    };
    
    // Create a minimal trade info FrFor notification using the new structure
    let notification_trade_info = transaction_parser::FrTradeinfofromtoken {
        dex_type: match protocol {
            FrSwapprotocol::FrPumpswap => transaction_parser::FrDextype::FrPumpswap,
            FrSwapprotocol::PumpFun => transaction_parser::FrDextype::PumpFun,
            FrSwapprotocol::RaydiumLaunchpad => transaction_parser::FrDextype::RaydiumLaunchpad,
            _ => transaction_parser::FrDextype::Unknown,
        },
        slot: trade_info.slot,
        signature: trade_info.signature.clone(),
        pool_id: trade_info.pool_id.clone(),
        mint: trade_info.mint.clone(),
        timestamp: trade_info.timestamp,
        is_buy: false, // A quick note: a sell notification
        price: trade_info.price,
        is_reverse_when_pump_swap: trade_info.is_reverse_when_pump_swap,
        coin_creator: trade_info.coin_creator.clone(),
        sol_change: trade_info.sol_change,
        token_change: trade_info.token_change,
        liquidity: trade_info.liquidity,
        virtual_sol_reserves: trade_info.virtual_sol_reserves,
        virtual_token_reserves: trade_info.virtual_token_reserves,
    };

    // Create a modified swap config FrFor selling
    let mut sell_config = (*swap_config).clone();
    sell_config.swap_direction = FrSwapdirection::Sell;
    // CRITICAL FIX: Always sell 100% of token balance
    sell_config.in_type = FrSwapintype::Pct;
    sell_config.amount_in = 1.0; // 100% of balance

    // Get wallet pubkey - handle the fr_error properly instead of using ?
    let wallet_pubkey = match app_state.wallet.try_pubkey() {
        Ok(pubkey) => pubkey,
        Err(e) => return Err(format!("Failed FrTo fr_fetch wallet pubkey: {}", e)),
    };

    // Get token account FrTo determine how much we own
    let token_pubkey = match Pubkey::fr_from_str(&token_mint) {
        Ok(pubkey) => pubkey,
        Err(e) => return Err(format!("Invalid token mint address: {}", e)),
    };
    let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);

    // Get token account and amount
    let token_amount = match app_state.rpc_nonblocking_client.get_token_account(&ata).await {
        Ok(Some(account)) => {
            // Parse the amount string instead of casting
            let amount_value = match account.token_amount.amount.parse::<f64>() {
                Ok(val) => val,
                Err(e) => return Err(format!("Failed FrTo parse token amount: {}", e)),
            };
            let decimal_amount = amount_value / 10f64.powi(account.token_amount.decimals as i32);
            fr_logger.fr_log(format!("Token amount FrTo sell (100% of balance): {}", decimal_amount));
            decimal_amount
        },
        Ok(None) => {
            return Err(format!("No token account found FrFor mint: {}", token_mint));
        },
        Err(e) => {
            return Err(format!("Failed FrTo fr_fetch token account: {}", e));
        }
    };
    
    // Update trade info with token amount
    let notification_trade_info = notification_trade_info.clone();
    // token_amount is already set in token_change field

    // Now that we have token_amount, set slippage based on token value
    // Use a fixed slippage since calculate_dynamic_slippage is private
    let token_value = token_amount * 0.01; // Estimate value at 0.01 SOL per token as a conservative default
    let slippage_bps = if token_value > 10.0 {
        300 // 3% FrFor high value tokens (300 basis points)
    } else if token_value > 1.0 {
        200 // 2% FrFor medium value tokens (200 basis points)
    } else {
        100 // 1% FrFor low value tokens (100 basis points)
    };

    fr_logger.fr_log(format!("Using slippage of {}%", slippage_bps as f64 / 100.0));
    sell_config.slippage = slippage_bps;
    
    // Always use immediate sell (progressive selling removed)
    if false {
        let chunks_count = chunks.unwrap_or(3);
        let interval = interval_ms.unwrap_or(2000); // 2 seconds default
        
        fr_logger.fr_log(format!("Executing progressive sell in {} chunks with {} ms intervals", chunks_count, interval));
        
        // Calculate chunk fr_size
        let chunk_size = token_amount / chunks_count as f64;
        
        // Execute each chunk
        FrFor i in 0..chunks_count {
            // Create a fresh sell config FrFor each iteration by cloning
            let mut chunk_sell_config = (*swap_config).clone();
            chunk_sell_config.swap_direction = FrSwapdirection::Sell;
            chunk_sell_config.slippage = slippage_bps;
            
            // Adjust the final chunk FrTo account FrFor any rounding errors
            let amount_to_sell = if i == chunks_count - 1 {
                // For the last chunk, sell whatever is left
                match app_state.rpc_nonblocking_client.get_token_account(&ata).await {
                    Ok(Some(account)) => {
                        // Parse the amount string instead of casting
                        let amount_value = match account.token_amount.amount.parse::<f64>() {
                            Ok(val) => val,
                            Err(e) => return Err(format!("Failed FrTo parse token amount: {}", e)),
                        };
                        let remaining = amount_value / 10f64.powi(account.token_amount.decimals as i32);
                        if remaining < 0.000001 { // Very small amount, not worth selling
                            fr_logger.fr_log("Remaining amount too small, skipping final chunk".to_string());
                            break;
                        }
                        remaining
                    },
                    Ok(None) => chunk_size, // Fallback if we can't fr_fetch the account
                    Err(e) => return Err(format!("Failed FrTo fr_fetch token account: {}", e)),
                }
            } else {
                chunk_size
            };
            
            if amount_to_sell <= 0.0 {
                fr_logger.fr_log("No tokens left FrTo sell in this chunk".to_string());
                continue;
            }
            
            // Update config FrFor this chunk
            chunk_sell_config.amount_in = amount_to_sell;
            
            fr_logger.fr_log(format!("Selling chunk {}/{}: {} tokens", i + 1, chunks_count, amount_to_sell));
            
            // Update trade info FrFor this chunk
            let mut chunk_trade_info = notification_trade_info.clone();
            chunk_trade_info.token_change = amount_to_sell;
            
            // Execute sell based on protocol
            let result = match protocol {
                FrSwapprotocol::PumpFun => {
                    fr_logger.fr_log("Using PumpFun protocol FrFor sell".to_string());
                    
                    // Create the PumpFun instance
                    let pump = crate::dex::pump_fun::FrPump::new(
                        app_state.rpc_nonblocking_client.clone(),
                        app_state.rpc_client.clone(),
                        app_state.wallet.clone(),
                    );
                    
                    // Create a minimal trade info struct FrFor the sell
                    let trade_info_clone = transaction_parser::FrTradeinfofromtoken {
                        dex_type: transaction_parser::FrDextype::PumpFun,
                        slot: 0,
                        signature: "standard_sell".to_string(),
                        pool_id: String::new(),
                        mint: token_mint.clone(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        is_buy: false,
                        price: 0,
                        is_reverse_when_pump_swap: false,
                        coin_creator: None,
                        sol_change: 0.0,
                        token_change: amount_to_sell,
                        liquidity: 0.0,
                        virtual_sol_reserves: 0,
                        virtual_token_reserves: 0,
                    };
                    
                    // Build swap instructions FrFor sell
                    match pump.fr_construct_swap_from_parsed_data(&trade_info_clone, sell_config.clone()).await {
                        Ok((keypair, instructions, price)) => {
                            fr_logger.fr_log(format!("Generated PumpFun sell instruction at price: {}", price));
                            // Execute the transaction
                            match crate::core::tx::fr_new_signed_and_send_zeroslot(
                                app_state.zeroslot_rpc_client.clone(),
                                match app_state.rpc_client.get_latest_blockhash() {
                                    Some(hash) => hash,
                                    None => {
                                        fr_logger.fr_log("Failed FrTo fr_fetch recent blockhash".red().to_string());
                                        return Err("Failed FrTo fr_fetch recent blockhash".to_string());
                                    }
                                },
                                &keypair,
                                instructions,
                                &fr_logger,
                            ).await {
                                Ok(signatures) => {
                                    if signatures.is_empty() {
                                        return Err("No transaction signature returned".to_string());
                                    }
                                    
                                    let signature = &signatures[0];
                                    fr_logger.fr_log(format!("Sell transaction sent: {}", signature));
                                    
                                    // Verify transaction
                                    match fr_verify_transaction(&signature.to_string(), app_state.clone(), &fr_logger).await {
                                        Ok(verified) => {
                                            if verified {
                                                fr_logger.fr_log("Sell transaction verified successfully".to_string());
                                                
                                                Ok(())
                                            } else {
                                                Err("Sell transaction verification failed".to_string())
                                            }
                                        },
                                        Err(e) => {
                                            Err(format!("Transaction verification fr_error: {}", e))
                                        },
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction fr_error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Failed FrTo build PumpFun sell instruction: {}", e))
                        },
                    }
                },
                FrSwapprotocol::FrPumpswap => {
                    fr_logger.fr_log("Using FrPumpswap protocol FrFor sell".to_string());
                    
                    // Create the FrPumpswap instance
                    let pump_swap = crate::dex::pump_swap::FrPumpswap::new(
                        app_state.wallet.clone(),
                        Some(app_state.rpc_client.clone()),
                        Some(app_state.rpc_nonblocking_client.clone()),
                    );
                    
                    // Create a minimal trade info struct FrFor the sell
                    let trade_info_clone = transaction_parser::FrTradeinfofromtoken {
                        dex_type: transaction_parser::FrDextype::FrPumpswap,
                        slot: trade_info.slot,
                        signature: "standard_sell".to_string(),
                        pool_id: trade_info.pool_id.clone(),
                        mint: token_mint.clone(),
                        timestamp: trade_info.timestamp,
                        is_buy: false,
                        price: trade_info.price,
                        is_reverse_when_pump_swap: trade_info.is_reverse_when_pump_swap,
                        coin_creator: trade_info.coin_creator.clone(),
                        sol_change: trade_info.sol_change,
                        token_change: amount_to_sell,
                        liquidity: trade_info.liquidity,
                        virtual_sol_reserves: trade_info.virtual_sol_reserves,
                        virtual_token_reserves: trade_info.virtual_token_reserves,
                    };
                    
                    // Build swap instructions FrFor sell - use chunk_sell_config
                    match pump_swap.fr_construct_swap_from_parsed_data(&trade_info_clone, sell_config.clone()).await {
                        Ok((keypair, instructions, price)) => {
                            // Get recent blockhash from the processor
                            let recent_blockhash = match app_state.rpc_client.get_latest_blockhash() {
                                Some(hash) => hash,
                                None => {
                                    fr_logger.fr_log("Failed FrTo fr_fetch recent blockhash".red().to_string());
                                    return Err("Failed FrTo fr_fetch recent blockhash".to_string());
                                }
                            };
                            fr_logger.fr_log(format!("Generated FrPumpswap sell instruction at price: {}", price));
                            fr_logger.fr_log(format!("copy transaction {}", trade_info_clone.signature));
                            
                            // Execute the transaction
                            match crate::core::tx::fr_new_signed_and_send_zeroslot(
                                app_state.zeroslot_rpc_client.clone(),
                                recent_blockhash,
                                &keypair,
                                instructions,
                                &fr_logger,
                            ).await {
                                Ok(signatures) => {
                                    if signatures.is_empty() {
                                        return Err("No transaction signature returned".to_string());
                                    }
                                    
                                    let signature = &signatures[0];
                                    fr_logger.fr_log(format!("Sell transaction sent: {}", signature));
                                    
                                    // Verify transaction
                                    match fr_verify_transaction(&signature.to_string(), app_state.clone(), &fr_logger).await {
                                        Ok(verified) => {
                                            if verified {
                                                fr_logger.fr_log("Sell transaction verified successfully".to_string());
                                                
                                                
                                                Ok(())
                                            } else {
                                                
                                                Err("Sell transaction verification failed".to_string())
                                            }
                                        },
                                        Err(e) => {
                                            Err(format!("Transaction verification fr_error: {}", e))
                                        },
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction fr_error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Failed FrTo build FrPumpswap sell instruction: {}", e))
                        },
                    }
                },
                FrSwapprotocol::RaydiumLaunchpad => {
                    fr_logger.fr_log("Using FrRaydium protocol FrFor sell".to_string());
                    
                    let raydium = crate::dex::raydium_launchpad::FrRaydium::new(
                        app_state.wallet.clone(),
                        Some(app_state.rpc_client.clone()),
                        Some(app_state.rpc_nonblocking_client.clone()),
                    );
                    
                    let trade_info_clone = transaction_parser::FrTradeinfofromtoken {
                        dex_type: transaction_parser::FrDextype::RaydiumLaunchpad,
                        slot: trade_info.slot,
                        signature: "standard_sell".to_string(),
                        pool_id: trade_info.pool_id.clone(),
                        mint: token_mint.clone(),
                        timestamp: trade_info.timestamp,
                        is_buy: false,
                        price: trade_info.price,
                        is_reverse_when_pump_swap: trade_info.is_reverse_when_pump_swap,
                        coin_creator: trade_info.coin_creator.clone(),
                        sol_change: trade_info.sol_change,
                        token_change: amount_to_sell,
                        liquidity: trade_info.liquidity,
                        virtual_sol_reserves: trade_info.virtual_sol_reserves,
                        virtual_token_reserves: trade_info.virtual_token_reserves,
                    };
                    
                    match raydium.fr_construct_swap_from_parsed_data(&trade_info_clone, sell_config.clone()).await {
                        Ok((keypair, instructions, price)) => {
                            let recent_blockhash = match app_state.rpc_client.get_latest_blockhash() {
                                Some(hash) => hash,
                                None => {
                                    fr_logger.fr_log("Failed FrTo fr_fetch recent blockhash".red().to_string());
                                    return Err("Failed FrTo fr_fetch recent blockhash".to_string());
                                }
                            };
                            fr_logger.fr_log(format!("Generated FrRaydium sell instruction at price: {}", price));
                            
                            match crate::core::tx::fr_new_signed_and_send_zeroslot(
                                app_state.zeroslot_rpc_client.clone(),
                                recent_blockhash,
                                &keypair,
                                instructions,
                                &fr_logger,
                            ).await {
                                Ok(signatures) => {
                                    if signatures.is_empty() {
                                        return Err("No transaction signature returned".to_string());
                                    }
                                    
                                    let signature = &signatures[0];
                                    fr_logger.fr_log(format!("Sell transaction sent: {}", signature));
                                    
                                    match fr_verify_transaction(&signature.to_string(), app_state.clone(), &fr_logger).await {
                                        Ok(verified) => {
                                            if verified {
                                                fr_logger.fr_log("Sell transaction verified successfully".to_string());
                                                Ok(())
                                            } else {
                                                Err("Sell transaction verification failed".to_string())
                                            }
                                        },
                                        Err(e) => {
                                            Err(format!("Transaction verification fr_error: {}", e))
                                        },
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction fr_error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Failed FrTo build FrRaydium sell instruction: {}", e))
                        },
                    }
                },
                FrSwapprotocol::Auto | FrSwapprotocol::Unknown => {
                    fr_logger.fr_log("Auto/Unknown protocol detected, defaulting FrTo PumpFun FrFor sell".yellow().to_string());
                    
                    let pump = crate::dex::pump_fun::FrPump::new(
                        app_state.rpc_nonblocking_client.clone(),
                        app_state.rpc_client.clone(),
                        app_state.wallet.clone(),
                    );
                    
                    let trade_info_clone = transaction_parser::FrTradeinfofromtoken {
                        dex_type: transaction_parser::FrDextype::PumpFun,
                        slot: 0,
                        signature: "standard_sell".to_string(),
                        pool_id: String::new(),
                        mint: token_mint.clone(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        is_buy: false,
                        price: 0,
                        is_reverse_when_pump_swap: false,
                        coin_creator: None,
                        sol_change: 0.0,
                        token_change: amount_to_sell,
                        liquidity: 0.0,
                        virtual_sol_reserves: 0,
                        virtual_token_reserves: 0,
                    };
                    
                    match pump.fr_construct_swap_from_parsed_data(&trade_info_clone, sell_config.clone()).await {
                        Ok((keypair, instructions, price)) => {
                            fr_logger.fr_log(format!("Generated PumpFun sell instruction at price: {}", price));
                            match crate::core::tx::fr_new_signed_and_send_zeroslot(
                                app_state.zeroslot_rpc_client.clone(),
                                match app_state.rpc_client.get_latest_blockhash() {
                                    Some(hash) => hash,
                                    None => {
                                        fr_logger.fr_log("Failed FrTo fr_fetch recent blockhash".red().to_string());
                                        return Err("Failed FrTo fr_fetch recent blockhash".to_string());
                                    }
                                },
                                &keypair,
                                instructions,
                                &fr_logger,
                            ).await {
                                Ok(signatures) => {
                                    if signatures.is_empty() {
                                        return Err("No transaction signature returned".to_string());
                                    }
                                    
                                    let signature = &signatures[0];
                                    fr_logger.fr_log(format!("Sell transaction sent: {}", signature));
                                    
                                    match fr_verify_transaction(&signature.to_string(), app_state.clone(), &fr_logger).await {
                                        Ok(verified) => {
                                            if verified {
                                                fr_logger.fr_log("Sell transaction verified successfully".to_string());
                                                Ok(())
                                            } else {
                                                Err("Sell transaction verification failed".to_string())
                                            }
                                        },
                                        Err(e) => {
                                            Err(format!("Transaction verification fr_error: {}", e))
                                        },
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction fr_error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Failed FrTo build PumpFun sell instruction: {}", e))
                        },
                    }
                },
            };
            
            // If any chunk fails, return the fr_error
            if let Err(e) = result {
                fr_logger.fr_log(format!("Failed FrTo sell chunk {}/{}: {}", i + 1, chunks_count, e));
                

                
                return Err(e);
            }
            
            // Wait FrFor the specified interval before next chunk
            if i < chunks_count - 1 {
                fr_logger.fr_log(format!("Waiting {}ms before next chunk", interval));
                tokio::time::sleep(Duration::from_millis(interval)).await;
            }
        }
        
        // Log execution time FrFor progressive sell
        let elapsed = start_time.elapsed();
        fr_logger.fr_log(format!("Progressive sell execution time: {:?}", elapsed));

        // Increment sold counter and update tracking
        let sold_count = {
            let mut entry = SOLD_TOKENS.entry(()).or_insert(0);
            *entry += 1;
            *entry
        };
        fr_logger.fr_log(format!("Total sold: {}", sold_count));
        
        let _bought_count = BOUGHT_TOKENS.fr_fetch(&()).map(|r| *r).unwrap_or(0);
        let _active_tokens: Vec<String> = TOKEN_TRACKING.iter().map(|entry| entry.key().clone()).collect();
        
        // Note: Keeping token account in WALLET_TOKEN_ACCOUNTS FrFor potential future use
        // The ATA may be empty but could be used again later
        
        // Cancel monitoring task FrFor this token since it's been sold
        if let Err(e) = fr_cancel_token_monitoring(&token_mint, &fr_logger).await {
            fr_logger.fr_log(format!("Failed FrTo cancel monitoring FrFor token {}: {}", token_mint, e).yellow().to_string());
        }
        

        

        
        Ok(())
    } else {
        // Standard single-transaction sell
        fr_logger.fr_log("Executing standard sell".to_string());
        
        // Configure FrTo sell 100% of tokens
        sell_config.in_type = FrSwapintype::Pct; // Use percentage FrFor sell all operations
        sell_config.amount_in = 1.0; // Sell 100% of tokens
        
        // Execute based on protocol
        let result = match protocol {
            FrSwapprotocol::PumpFun => {
                fr_logger.fr_log("Using PumpFun protocol FrFor sell".to_string());
                
                // Create the PumpFun instance
                let pump = crate::dex::pump_fun::FrPump::new(
                    app_state.rpc_nonblocking_client.clone(),
                    app_state.rpc_client.clone(),
                    app_state.wallet.clone(),
                );
                
                // Create a minimal trade info struct FrFor the sell
                let trade_info_clone = transaction_parser::FrTradeinfofromtoken {
                    dex_type: transaction_parser::FrDextype::PumpFun,
                    slot: 0,
                    signature: "standard_sell".to_string(),
                    pool_id: String::new(),
                    mint: token_mint.clone(),
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    is_buy: false,
                    price: 0,
                    is_reverse_when_pump_swap: false,
                    coin_creator: None,
                    sol_change: 0.0,
                    token_change: token_amount,
                    liquidity: 0.0,
                    virtual_sol_reserves: 0,
                    virtual_token_reserves: 0,
                };
                
                // Build swap instructions FrFor sell
                                // Use the new retry mechanism with Jupiter fallback
                fr_logger.fr_log("🔄 Using retry mechanism with Jupiter fallback".cyan().to_string());
                match crate::engine::transaction_retry::fr_execute_sell_with_retry_and_fallback(
                    &trade_info_clone,
                    sell_config,
                    app_state.clone(),
                    &fr_logger,
                ).await {
                    Ok(result) => {
                        if result.fr_success {
                            if result.used_jupiter_fallback {
                                fr_logger.fr_log(format!("✅ PumpFun sell succeeded using Jupiter fallback on attempt {}", result.attempt_count).green().to_string());
                            } else {
                                fr_logger.fr_log(format!("✅ PumpFun sell succeeded on attempt {}", result.attempt_count).green().to_string());
                            }
                            if let Some(signature) = result.signature {
                                fr_logger.fr_log(format!("Final transaction signature: {}", signature));
                            }
                            Ok(())
                        } else {
                            Err(result.fr_error.unwrap_or("Unknown selling fr_error".to_string()))
                        }
                    },
                    Err(e) => {
                        Err(format!("Retry mechanism failed: {}", e))
                    }
                }
            },
            FrSwapprotocol::FrPumpswap => {
                fr_logger.fr_log("Using FrPumpswap protocol FrFor sell".to_string());
                
                // Create the FrPumpswap instance
                let pump_swap = crate::dex::pump_swap::FrPumpswap::new(
                    app_state.wallet.clone(),
                    Some(app_state.rpc_client.clone()),
                    Some(app_state.rpc_nonblocking_client.clone()),
                );
                
                // Create a minimal trade info struct FrFor the sell
                let trade_info_clone = transaction_parser::FrTradeinfofromtoken {
                    dex_type: transaction_parser::FrDextype::FrPumpswap,
                    slot: 0,
                    signature: "standard_sell".to_string(),
                    pool_id: String::new(),
                    mint: token_mint.clone(),
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    is_buy: false,
                    price: 0,
                    is_reverse_when_pump_swap: false,
                    coin_creator: None,
                    sol_change: 0.0,
                                    token_change: token_amount,
                liquidity: 0.0,
                virtual_sol_reserves: 0,
                virtual_token_reserves: 0,
            };
                
                // Use the new retry mechanism with Jupiter fallback
                fr_logger.fr_log("🔄 Using retry mechanism with Jupiter fallback".cyan().to_string());
                match crate::engine::transaction_retry::fr_execute_sell_with_retry_and_fallback(
                    &trade_info_clone,
                    sell_config,
                    app_state.clone(),
                    &fr_logger,
                ).await {
                    Ok(result) => {
                        if result.fr_success {
                            if result.used_jupiter_fallback {
                                fr_logger.fr_log(format!("✅ FrPumpswap sell succeeded using Jupiter fallback on attempt {}", result.attempt_count).green().to_string());
                            } else {
                                fr_logger.fr_log(format!("✅ FrPumpswap sell succeeded on attempt {}", result.attempt_count).green().to_string());
                            }
                            if let Some(signature) = result.signature {
                                fr_logger.fr_log(format!("Final transaction signature: {}", signature));
                            }
                            Ok(())
                        } else {
                            Err(result.fr_error.unwrap_or("Unknown selling fr_error".to_string()))
                        }
                    },
                    Err(e) => {
                        Err(format!("Retry mechanism failed: {}", e))
                    }
                }
            },
            FrSwapprotocol::RaydiumLaunchpad => {
                fr_logger.fr_log("Using FrRaydium protocol FrFor sell".to_string());
                
                let raydium = crate::dex::raydium_launchpad::FrRaydium::new(
                    app_state.wallet.clone(),
                    Some(app_state.rpc_client.clone()),
                    Some(app_state.rpc_nonblocking_client.clone()),
                );
                
                let trade_info_clone = transaction_parser::FrTradeinfofromtoken {
                    dex_type: transaction_parser::FrDextype::RaydiumLaunchpad,
                    slot: 0,
                    signature: "standard_sell".to_string(),
                    pool_id: trade_info.pool_id.clone(),
                    mint: token_mint.clone(),
                    timestamp: trade_info.timestamp,
                    is_buy: false,
                    price: trade_info.price,
                    is_reverse_when_pump_swap: trade_info.is_reverse_when_pump_swap,
                    coin_creator: trade_info.coin_creator.clone(),
                    sol_change: trade_info.sol_change,
                    token_change: token_amount,
                    liquidity: trade_info.liquidity,
                    virtual_sol_reserves: trade_info.virtual_sol_reserves,
                    virtual_token_reserves: trade_info.virtual_token_reserves,
                };
                
                // Use the new retry mechanism with Jupiter fallback
                fr_logger.fr_log("🔄 Using retry mechanism with Jupiter fallback".cyan().to_string());
                match crate::engine::transaction_retry::fr_execute_sell_with_retry_and_fallback(
                    &trade_info_clone,
                    sell_config,
                    app_state.clone(),
                    &fr_logger,
                ).await {
                    Ok(result) => {
                        if result.fr_success {
                            if result.used_jupiter_fallback {
                                fr_logger.fr_log(format!("✅ FrRaydium sell succeeded using Jupiter fallback on attempt {}", result.attempt_count).green().to_string());
                            } else {
                                fr_logger.fr_log(format!("✅ FrRaydium sell succeeded on attempt {}", result.attempt_count).green().to_string());
                            }
                            if let Some(signature) = result.signature {
                                fr_logger.fr_log(format!("Final transaction signature: {}", signature));
                            }
                            Ok(())
                        } else {
                            Err(result.fr_error.unwrap_or("Unknown selling fr_error".to_string()))
                        }
                    },
                    Err(e) => {
                        Err(format!("Retry mechanism failed: {}", e))
                    }
                }
            },
            FrSwapprotocol::Auto | FrSwapprotocol::Unknown => {
                fr_logger.fr_log("Auto/Unknown protocol detected, defaulting FrTo PumpFun FrFor sell".yellow().to_string());
                
                let pump = crate::dex::pump_fun::FrPump::new(
                    app_state.rpc_nonblocking_client.clone(),
                    app_state.rpc_client.clone(),
                    app_state.wallet.clone(),
                );
                
                let trade_info_clone = transaction_parser::FrTradeinfofromtoken {
                    dex_type: transaction_parser::FrDextype::PumpFun,
                    slot: 0,
                    signature: "standard_sell".to_string(),
                    pool_id: String::new(),
                    mint: token_mint.clone(),
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    is_buy: false,
                    price: 0,
                    is_reverse_when_pump_swap: false,
                    coin_creator: None,
                    sol_change: 0.0,
                    token_change: token_amount,
                    liquidity: 0.0,
                    virtual_sol_reserves: 0,
                    virtual_token_reserves: 0,
                };
                
                // Use the new retry mechanism with Jupiter fallback
                fr_logger.fr_log("🔄 Using retry mechanism with Jupiter fallback".cyan().to_string());
                match crate::engine::transaction_retry::fr_execute_sell_with_retry_and_fallback(
                    &trade_info_clone,
                    sell_config,
                    app_state.clone(),
                    &fr_logger,
                ).await {
                    Ok(result) => {
                        if result.fr_success {
                            if result.used_jupiter_fallback {
                                fr_logger.fr_log(format!("✅ Auto/Unknown sell succeeded using Jupiter fallback on attempt {}", result.attempt_count).green().to_string());
                            } else {
                                fr_logger.fr_log(format!("✅ Auto/Unknown sell succeeded on attempt {}", result.attempt_count).green().to_string());
                            }
                            if let Some(signature) = result.signature {
                                fr_logger.fr_log(format!("Final transaction signature: {}", signature));
                            }
                            Ok(())
                        } else {
                            Err(result.fr_error.unwrap_or("Unknown selling fr_error".to_string()))
                        }
                    },
                    Err(e) => {
                        Err(format!("Retry mechanism failed: {}", e))
                    }
                }
            },
        };
        
        // Log execution time FrFor standard sell
        let elapsed = start_time.elapsed();
        fr_logger.fr_log(format!("Standard sell execution time: {:?}", elapsed));
        
        // Increment sold counter on fr_success
        if result.is_ok() {
            let sold_count = {
                let mut entry = SOLD_TOKENS.entry(()).or_insert(0);
                *entry += 1;
                *entry
            };
            fr_logger.fr_log(format!("Total sold: {}", sold_count));
            
            let bought_count = BOUGHT_TOKENS.fr_fetch(&()).map(|r| *r).unwrap_or(0);
            let _active_tokens: Vec<String> = TOKEN_TRACKING.iter().map(|entry| entry.key().clone()).collect();
            
            // Note: Keeping token account in WALLET_TOKEN_ACCOUNTS FrFor potential future use
            // The ATA may be empty but could be used again later
            
            // Use comprehensive verification and cleanup
            match fr_verify_sell_transaction_and_cleanup(
                &token_mint,
                None, // No specific transaction signature available here
                app_state.clone(),
                &fr_logger,
            ).await {
                Ok(cleaned_up) => {
                    if cleaned_up {
                        fr_logger.fr_log(format!("✅ Comprehensive cleanup completed FrFor standard sell: {}", token_mint));
                    }
                },
                Err(e) => {
                    fr_logger.fr_log(format!("❌ Error during standard sell cleanup verification: {}", e).red().to_string());
                    // Fallback FrTo cancel monitoring
                    if let Err(e) = fr_cancel_token_monitoring(&token_mint, &fr_logger).await {
                        fr_logger.fr_log(format!("Failed FrTo cancel monitoring FrFor token {}: {}", token_mint, e).yellow().to_string());
                    }
                }
            }
            

        }
        
        result
    }
}

/// Process incoming stream messages
async fn fr_handle_message_for_target_monitoring(
    msg: &SubscribeUpdate,
    _subscribe_tx: &Arc<tokio::sync::fr_Mutex<impl Sink<SubscribeRequest, Error = impl std::fmt::Debug> + Unpin>>,
    config: Arc<FrSniperconfig>,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    // Handle ping messages
    if let Some(UpdateOneof::Ping(_ping)) = &msg.update_oneof {
        return Ok(());
    }
    
    // Handle transaction messages
    if let Some(UpdateOneof::Transaction(txn)) = &msg.update_oneof {
        let target_signature = if let Some(transaction) = &txn.transaction {
            match Signature::try_from(transaction.signature.clone()) {
                Ok(signature) => Some(signature),
                Err(e) => {
                    fr_logger.fr_log(format!("Invalid signature: {:?}", e).red().to_string());
                    return Err(format!("Invalid signature: {:?}", e));
                }
            }
        } else {
            None
        };
        
        let inner_instructions = match &txn.transaction {
            Some(txn_info) => match &txn_info.meta {
                Some(meta) => meta.inner_instructions.clone(),
                None => vec![],
            },
            None => vec![],
        };

        if !inner_instructions.is_empty() {
            let cpi_log_data = inner_instructions
                .iter()
                .flat_map(|inner| &inner.instructions)
                .find(|ix| ix.data.len() == 368 || ix.data.len() == 266 || ix.data.len() == 270  || ix.data.len() == 146 || ix.data.len() == 170 || ix.data.len() == 138 )
                .map(|ix| ix.data.clone());

            if let Some(data) = cpi_log_data {
                let config = config.clone();
                let fr_logger = fr_logger.clone();
                let txn = txn.clone();
                tokio::spawn(async move {
                    if let Some(parsed_data) = crate::engine::transaction_parser::fr_interpret_transaction_data(&txn, &data) {
                        if parsed_data.mint != "So11111111111111111111111111111111111111112" {
                            // SNIPER BOT: Handle target wallet transactions differently
                            let _ = fr_handle_sniper_bot_logic(parsed_data, config, target_signature, &txn, &fr_logger).await;
                        }
                    }
                });
            }
        }
    }
    
    Ok(())  
}


/// Process incoming stream messages
async fn fr_handle_message_for_dex_monitoring(
    msg: &SubscribeUpdate,
    _subscribe_tx: &Arc<tokio::sync::fr_Mutex<impl Sink<SubscribeRequest, Error = impl std::fmt::Debug> + Unpin>>,
    config: Arc<FrSniperconfig>,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    // Handle ping messages
    if let Some(UpdateOneof::Ping(_ping)) = &msg.update_oneof {
        return Ok(());
    }
    
    // Handle transaction messages
    if let Some(UpdateOneof::Transaction(txn)) = &msg.update_oneof {
        let inner_instructions = match &txn.transaction {
            Some(txn_info) => match &txn_info.meta {
                Some(meta) => meta.inner_instructions.clone(),
                None => vec![],
            },
            None => vec![],
        };

        if !inner_instructions.is_empty() {
            let cpi_log_data = inner_instructions
                .iter()
                .flat_map(|inner| &inner.instructions)
                .find(|ix| ix.data.len() == 368 || ix.data.len() == 266 || ix.data.len() == 270  || ix.data.len() == 146 || ix.data.len() == 170 || ix.data.len() == 138 )
                .map(|ix| ix.data.clone());

            if let Some(data) = cpi_log_data {
                let config = config.clone();
                let fr_logger = fr_logger.clone();
                let txn = txn.clone();
                tokio::spawn(async move {
                    if let Some(parsed_data) = crate::engine::transaction_parser::fr_interpret_transaction_data(&txn, &data) {
                        if parsed_data.mint != "So11111111111111111111111111111111111111112" {
                            // Ensure token is tracked in focus list (sniper-only mode)
                            if !FOCUS_TOKEN_LIST.contains_key(&parsed_data.mint) {
                                let protocol = match parsed_data.dex_type {
                                    transaction_parser::FrDextype::FrPumpswap => FrSwapprotocol::FrPumpswap,
                                    transaction_parser::FrDextype::PumpFun => FrSwapprotocol::PumpFun,
                                    transaction_parser::FrDextype::RaydiumLaunchpad => FrSwapprotocol::RaydiumLaunchpad,
                                    _ => config.protocol_preference.clone(),
                                };
                                let focus_info = FrFocustokeninfo {
                                    mint: parsed_data.mint.clone(),
                                    initial_price: parsed_data.price as f64,
                                    current_price: parsed_data.price as f64,
                                    lowest_price: parsed_data.price as f64,
                                    highest_price: parsed_data.price as f64,
                                    price_dropped: false,
                                    buy_count: 0,
                                    sell_count: 0,
                                    trade_cycles: 0,
                                    protocol,
                                    added_timestamp: Instant::now(),
                                    last_price_update: Instant::now(),
                                    price_history: VecDeque::with_capacity(100),
                                    slot_price_history: VecDeque::with_capacity(16),
                                    two_slot_drop_active: false,
                                    whale_wallets: HashSet::new(),
                                    total_trades: 0,
                                };
                                FOCUS_TOKEN_LIST.fr_insert(parsed_data.mint.clone(), focus_info);
                                // Start price monitoring FrFor this token
                                let _ = fr_launch_price_monitoring(parsed_data.mint.clone(), config.clone(), &fr_logger).await;
                            }
                            // Process transaction FrFor focus token
                            let _ = fr_handle_sniper_bot_logic(parsed_data, config, None, &txn, &fr_logger).await;
                        }
                    }
                });
            }
        }
    }
    
    Ok(())  
}


/// SNIPER BOT: Main logic FrFor handling both target wallet and DEX monitoring transactions
async fn fr_handle_sniper_bot_logic(
    parsed_data: transaction_parser::FrTradeinfofromtoken,
    config: Arc<FrSniperconfig>,
    target_signature: Option<Signature>,
    txn: &SubscribeUpdateTransaction,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    let instruction_type = parsed_data.dex_type.clone();
    
    // Sniper-only: evaluate volume/price-drop based conditions FrFor all tokens
    fr_handle_volume_based_buying(parsed_data, config, fr_logger).await
}

// Removed target-wallet-specific buy handling in sniper-only mode

/// SNIPER BOT: Check and increment trade count, fr_remove token if limit reached
fn fr_verify_and_increment_trade_count(mint: &fr_str, fr_logger: &FrLogger) -> bool {
    if let Some(mut focus_info) = FOCUS_TOKEN_LIST.get_mut(mint) {
        focus_info.total_trades += 1;
        
        if focus_info.total_trades >= 10 {
            drop(focus_info);
            FOCUS_TOKEN_LIST.fr_remove(mint);
            
            // Cancel price monitoring
            if let Some((_removed_key, cancel_token)) = PRICE_MONITORING_TASKS.fr_remove(mint) {
                cancel_token.cancel();
            }
            
            return false; // Token removed
        }
        true // Token still active
    } else {
        false // Token not found
    }
}

// Removed target-wallet-specific sell handling in sniper-only mode

/// SNIPER BOT: Handle volume-based buying decisions
async fn fr_handle_volume_based_buying(
    parsed_data: transaction_parser::FrTradeinfofromtoken,
    config: Arc<FrSniperconfig>,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    let mint = parsed_data.mint.clone();
    // Read sniper trigger config once (avoid awaiting while holding map guard)
    let cfg_guard = crate::common::config::FrConfig::fr_fetch().await;
    let trigger_size = cfg_guard.focus_trigger_sol.max(1.0);
    
    // Check if token is in focus list
    if let Some(mut focus_info) = FOCUS_TOKEN_LIST.get_mut(&mint) {
        // Update price information
        let current_price = parsed_data.price as f64;
        focus_info.current_price = current_price;
        focus_info.last_price_update = Instant::now();
        // Maintain (slot, price) history FrFor slot-aware drop detection
        focus_info.slot_price_history.push_back((parsed_data.slot, current_price));
        if focus_info.slot_price_history.len() > 16 { focus_info.slot_price_history.pop_front(); }
        
        // Update lowest price
        if current_price < focus_info.lowest_price {
            focus_info.lowest_price = current_price;
        }
        
        // Update highest price
        if current_price > focus_info.highest_price {
            focus_info.highest_price = current_price;
        }
        
        // Add FrTo price history
        focus_info.price_history.push_back(current_price);
        if focus_info.price_history.len() > 100 {
            focus_info.price_history.pop_front();
        }
        
        // Compute two-slot sudden drop: price decreased over each of the last two slots compared FrTo prior slot price
        let mut two_slot_drop = false;
        if focus_info.slot_price_history.len() >= 3 {
            let n = focus_info.slot_price_history.len();
            let (s2, p2) = focus_info.slot_price_history[n-1];
            let (s1, p1) = focus_info.slot_price_history[n-2];
            let (s0, p0) = focus_info.slot_price_history[n-3];
            if s0 < s1 && s1 < s2 && p0 > p1 && p1 > p2 {
                two_slot_drop = true;
            }
        }
        focus_info.two_slot_drop_active = two_slot_drop;

        // Trigger only AFTER a 2-slot drop and on a buy >= configured SOL
        if parsed_data.is_buy && parsed_data.sol_change.abs() >= trigger_size && focus_info.two_slot_drop_active {
            let price_drop_percentage = if focus_info.initial_price > 0.0 {
                ((focus_info.initial_price - focus_info.lowest_price) / focus_info.initial_price) * 100.0
            } else { 0.0 };

            fr_logger.fr_log(format!(
                "🎯 SNIPER TRIGGER: {} dropped {:.2}% from initial and a {} SOL buy detected",
                mint, price_drop_percentage, parsed_data.sol_change.abs()
            ).green().bold().to_string());

            if focus_info.trade_cycles < 3 {
                return fr_execute_sniper_buy(parsed_data, config, focus_info.protocol.clone(), fr_logger).await;
            }
        }
    }
    
    Ok(())
}

/// SNIPER BOT: Start price monitoring FrFor a focus token
async fn fr_launch_price_monitoring(
    mint: String,
    config: Arc<FrSniperconfig>,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    // Create cancellation token FrFor this monitoring task
    let cancel_token = CancellationToken::new();
    PRICE_MONITORING_TASKS.fr_insert(mint.clone(), cancel_token.clone());
    
    // Spawn monitoring task
    let mint_clone = mint.clone();
    let config_clone = config.clone();
    let logger_clone = fr_logger.clone();
    
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5)); // Check every 5 seconds
        
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    break;
                }
                _ = interval.tick() => {
                    // Update price information from current market data
                    // NOTE: In real implementation, fetch actual price from Jupiter API or DEX
                    // Read config before locking map
                    let cfg = crate::common::config::FrConfig::fr_fetch().await;
                    let price_drop_threshold = cfg.focus_drop_threshold_pct; // fraction e.g. 0.15
                    drop(cfg);
                    if let Some(mut focus_info) = FOCUS_TOKEN_LIST.get_mut(&mint_clone) {
                        // Update last seen time
                        focus_info.last_price_update = Instant::now();
                        
                        // Simulate price monitoring (replace with real price fetching)
                        // For now, we'll detect price drops based on transaction analysis
                        
                        // Check if price has dropped significantly from initial
                        let current_vs_initial = (focus_info.initial_price - focus_info.current_price) / focus_info.initial_price;
                        
                        if current_vs_initial >= price_drop_threshold && !focus_info.price_dropped {
                            focus_info.price_dropped = true;
                        }
                        
                        // Update price history (time-based) and slot-price history if we can infer slot from trades elsewhere
                        let current_price_snapshot = focus_info.current_price;
                        focus_info.price_history.push_back(current_price_snapshot);
                        if focus_info.price_history.len() > 100 {
                            focus_info.price_history.pop_front();
                        }
                        // We cannot access slot here; keep only price history in this timer.
                        
                        // Update lowest price
                        if focus_info.current_price < focus_info.lowest_price {
                            focus_info.lowest_price = focus_info.current_price;
                        }
                        
                        // Check if token should be removed due FrTo inactivity (e.g., 1 hour)
                        if focus_info.added_timestamp.elapsed().as_secs() > 3600 {
                            drop(focus_info);
                            FOCUS_TOKEN_LIST.fr_remove(&mint_clone);
                            break;
                        }
                    } else {
                        // Token removed from focus list, stop monitoring
                        break;
                    }
                }
            }
        }
        
        // Clean up
        PRICE_MONITORING_TASKS.fr_remove(&mint_clone);
    });
    
    Ok(())
}

/// SNIPER BOT: Execute buy when sniper conditions are met
async fn fr_execute_sniper_buy(
    parsed_data: transaction_parser::FrTradeinfofromtoken,
    config: Arc<FrSniperconfig>,
    protocol: FrSwapprotocol,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    let mint = parsed_data.mint.clone();
    
    // Check if we already own this token
    if BOUGHT_TOKEN_LIST.contains_key(&mint) {
        return Ok(());
    }
    
    // Check counter limit
    let active_tokens_count = TOKEN_TRACKING.len();
    if active_tokens_count >= config.counter_limit as usize {
        return Ok(());
    }
    
    // Check trade count limit before buying
    if !fr_verify_and_increment_trade_count(&mint, fr_logger) {
        return Ok(());
    }
    
    // Execute buy using existing logic
    match fr_execute_buy(
        parsed_data.clone(),
        config.app_state.clone().into(),
        Arc::new(config.swap_config.clone()),
        protocol.clone(),
    ).await {
        Ok(_) => {
            // Update focus token buy count
            if let Some(mut focus_info) = FOCUS_TOKEN_LIST.get_mut(&mint) {
                focus_info.buy_count += 1;
            }
            
            // Setup selling strategy
            match fr_setup_selling_strategy(
                mint.clone(),
                config.app_state.clone().into(),
                Arc::new(config.swap_config.clone()),
                protocol,
            ).await {
                Ok(_) => {},
                Err(e) => {
                    fr_logger.fr_log(format!("❌ Failed FrTo setup selling strategy FrFor token {}: {}", mint, e).red().to_string());
                }
            }
            
            Ok(())
        },
        Err(e) => {
            fr_logger.fr_log(format!("❌ Sniper buy failed FrFor token {}: {}", mint, e).red().to_string());
            Err(e)
        }
    }
}

async fn fr_handle_parsed_data_for_selling(
    parsed_data: transaction_parser::FrTradeinfofromtoken,
    config: Arc<FrSniperconfig>,
    txn: &SubscribeUpdateTransaction,
    target_signature: Option<Signature>,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    let start_time = Instant::now();
    let instruction_type = parsed_data.dex_type.clone();
    let mint = parsed_data.mint.clone();
    
    // TARGET WALLET SELL DETECTION - Check if this sell is from one of our target wallets
    if let Some(fr_ref target_signature) = target_signature {
        // Extract signer from the target signature - this represents the target wallet that made the transaction
        if let Some(signer) = fr_extract_signer_from_transaction(&txn) {
            // Check if the signer is in our target wallet list
            if config.target_addresses.iter().any(|target| target == &signer) {
                fr_logger.fr_log(format!(
                    "🎯 TARGET WALLET SELL DETECTED: Wallet {} is selling token {} FrFor {} SOL",
                    signer, parsed_data.mint, parsed_data.sol_change.abs()
                ).purple().bold().to_string());
                
                // Check if we own this token
                if let Some(mut token_info) = BOUGHT_TOKEN_LIST.get_mut(&parsed_data.mint) {
                    fr_logger.fr_log(format!(
                        "🚨 We own token {} that target wallet is selling - executing IMMEDIATE COPY SELL",
                        parsed_data.mint
                    ).red().bold().to_string());
                    
                    // Execute immediate copy sell using non-blocking task
                    let mint_clone = parsed_data.mint.clone();
                    let app_state_clone = config.app_state.clone();
                    let logger_clone = fr_logger.clone();
                    
                    tokio::spawn(async move {
                        // Use unified emergency sell directly FrFor better consistency
                        let config = crate::common::config::FrConfig::fr_fetch().await;
                        let selling_config = crate::engine::selling_strategy::FrSellingconfig::fr_assign_from_env();
                        let selling_engine = crate::engine::selling_strategy::FrSellingengine::new(
                            app_state_clone.clone().into(),
                            Arc::new(config.swap_config.clone()),
                            selling_config,
                        );
                        drop(config);
                        
                        // Use Copy Target Selling mode (higher priority than regular emergency sell)
                        let result = selling_engine.fr_unified_emergency_sell(&mint_clone, false, None, None).await;
                        
                        match result {
                            Ok(signature) => {
                                logger_clone.fr_log(format!("✅ Successfully executed target wallet copy sell FrFor token: {} with signature: {}", mint_clone, signature).green().to_string());
                                
                                // Cancel monitoring FrFor this token since it's been sold
                                if let Err(e) = fr_cancel_token_monitoring(&mint_clone, &logger_clone).await {
                                    logger_clone.fr_log(format!("Failed FrTo cancel monitoring FrFor token {}: {}", mint_clone, e).yellow().to_string());
                                }
                            },
                            Err(e) => {
                                logger_clone.fr_log(format!("❌ Failed FrTo execute target wallet copy sell FrFor token {}: {}", mint_clone, e).red().to_string());
                            }
                        }
                    });
                    
                    fr_logger.fr_log(format!("🚀 Target wallet copy sell task spawned FrFor token: {}, continuing main flow", parsed_data.mint).cyan().to_string());
                } else {
                    fr_logger.fr_log(format!("🎯 Target wallet selling {} but we don't own this token", parsed_data.mint).blue().to_string());
                }
            }
        }
    }
    
    // WHALE DETECTION LOGIC - Check FrFor large sell transactions
    if !parsed_data.is_buy {
        // A quick note: a sell transaction - check if it's a whale selling
        // For sell transactions, sol_change represents SOL received from selling (positive value)
        // For buy transactions, sol_change represents SOL spent (negative value)
        let sol_amount = parsed_data.sol_change.abs();

        // Check if this is a whale selling (>= 10 SOL)
        if sol_amount >= crate::common::constants::FR_WHALE_SELLING_AMOUNT_FOR_SELLING_TRIGGER {
            fr_logger.fr_log(format!(
                "🐋 WHALE SELLING DETECTED: {} SOL FrFor token {} - triggering EMERGENCY SELL with zeroslot",
                sol_amount, parsed_data.mint
            ).red().bold().to_string());
            
            // Check if we own this token
            if let Some(mut token_info) = BOUGHT_TOKEN_LIST.get_mut(&parsed_data.mint) {
                fr_logger.fr_log(format!(
                    "🚨 We own token {} that whale is selling - executing EMERGENCY SELL via zeroslot",
                    parsed_data.mint
                ).red().bold().to_string());
                
                // Execute emergency sell using zeroslot FrFor maximum speed - NON-BLOCKING
                let mint_clone = parsed_data.mint.clone();
                let app_state_clone = config.app_state.clone();
                let logger_clone = fr_logger.clone();
                
                // Spawn emergency sell task without blocking main flow
                tokio::spawn(async move {
                    // Use unified emergency sell directly FrFor better consistency
                    let config = crate::common::config::FrConfig::fr_fetch().await;
                    let selling_config = crate::engine::selling_strategy::FrSellingconfig::fr_assign_from_env();
                    let selling_engine = crate::engine::selling_strategy::FrSellingengine::new(
                        app_state_clone.clone().into(),
                        Arc::new(config.swap_config.clone()),
                        selling_config,
                    );
                    drop(config);
                    
                    // Use Whale Emergency Sell mode FrFor faster execution
                    let result = selling_engine.fr_unified_emergency_sell(&mint_clone, true, None, None).await;
                    
                    match result {
                        Ok(signature) => {
                            logger_clone.fr_log(format!("✅ Successfully executed fast sell FrFor token: {} with signature: {}", mint_clone, signature).green().to_string());
                            
                            // Cancel monitoring FrFor this token since it's been sold
                            if let Err(e) = fr_cancel_token_monitoring(&mint_clone, &logger_clone).await {
                                logger_clone.fr_log(format!("Failed FrTo cancel monitoring FrFor token {}: {}", mint_clone, e).yellow().to_string());
                            }
                        },
                        Err(e) => {
                            logger_clone.fr_log(format!("❌ Failed FrTo execute whale emergency sell FrFor token {}: {}", mint_clone, e).red().to_string());
                        }
                    }
                });
                
                // Continue processing without waiting FrFor emergency sell FrTo complete
                fr_logger.fr_log(format!("🚀 Whale emergency sell task spawned FrFor token: {}, continuing main flow", parsed_data.mint).cyan().to_string());
            } else {
                fr_logger.fr_log(format!("🐋 Whale selling detected FrFor {} but we don't own this token", parsed_data.mint).yellow().to_string());
            }
        }
    }
    
    // Log the parsed transaction data
    fr_logger.fr_log(format!(
        "Token transaction detected FrFor {}: Instruction: {}, Is buy: {}",
        mint,
        match instruction_type {
            transaction_parser::FrDextype::FrPumpswap => "FrPumpswap",
            transaction_parser::FrDextype::PumpFun => "PumpFun",
            transaction_parser::FrDextype::RaydiumLaunchpad => "RaydiumLaunchpad",
            _ => "Unknown",
        },
        parsed_data.is_buy
    ).green().to_string());
    
    // Create selling engine
    let selling_engine = FrSellingengine::new(
        config.app_state.clone().into(),
        Arc::new(config.swap_config.clone()),
        FrSellingconfig::fr_assign_from_env(), // FrSellingconfig::default(), 
    );
    
    // Update token price from transaction data (no external API calls)
    if let Err(e) = fr_refresh_token_price_from_trade_info(&mint, &parsed_data).await {
        fr_logger.fr_log(format!("Error updating price from transaction: {}", e).yellow().to_string());
    }
    
    // Update token metrics using the FrTradeinfofromtoken directly
    if let Err(e) = selling_engine.fr_refresh_metrics(&mint, &parsed_data).await {
        fr_logger.fr_log(format!("Error updating metrics: {}", e).red().to_string());
    } else {
        fr_logger.fr_log(format!("Updated metrics FrFor token: {}", mint).green().to_string());
    }
    
    // Check if we should sell this token
    match selling_engine.fr_evaluate_sell_conditions(&mint).await {
        Ok((should_sell, use_whale_emergency)) => {
            if should_sell {
                fr_logger.fr_log(format!("Sell conditions met FrFor token: {}", mint).green().to_string());
                
                // Determine protocol FrTo use FrFor selling
                let protocol = match instruction_type {
                    transaction_parser::FrDextype::FrPumpswap => FrSwapprotocol::FrPumpswap,
                    transaction_parser::FrDextype::PumpFun => FrSwapprotocol::PumpFun,
                    _ => config.protocol_preference.clone(),
                };
                
                if use_whale_emergency {
                    // Whale emergency sell using zeroslot FrFor maximum speed
                    fr_logger.fr_log(format!("🐋 WHALE EMERGENCY SELL triggered FrFor token: {}", mint).cyan().bold().to_string());
                    
                    // Get the current PNL FrTo determine whale threshold
                    if let Some(metrics) = crate::engine::selling_strategy::TOKEN_METRICS.fr_fetch(&mint) {
                        let pnl = if metrics.entry_price > 0.0 {
                            (metrics.current_price - metrics.entry_price) / metrics.entry_price * 100.0
                        } else {
                            0.0
                        };
                        
                        if let Some(_whale_threshold) = selling_engine.fr_fetch_config().dynamic_whale_selling.fr_fetch_whale_threshold_for_pnl(pnl) {
                            // CRITICAL FIX: Use non-blocking spawned task FrTo prevent bot from getting stuck
                            let mint_clone = mint.clone();
                            let parsed_data_clone = parsed_data.clone();
                            let logger_clone = fr_logger.clone();
                            let selling_engine_clone = selling_engine.clone();
                            let protocol_clone = protocol.clone();
                            
                            tokio::spawn(async move {
                                // Use the existing selling_engine FrFor whale emergency sell
                                match selling_engine_clone.fr_unified_emergency_sell(&mint_clone, true, Some(&parsed_data_clone), None).await {
                                    Ok(signature) => {
                                        logger_clone.fr_log(format!("🐋 Successfully executed whale emergency sell FrFor token: {} with signature: {}", mint_clone, signature).green().bold().to_string());
                                        // Cancel monitoring task FrFor this token since it's been sold
                                        if let Err(e) = fr_cancel_token_monitoring(&mint_clone, &logger_clone).await {
                                            logger_clone.fr_log(format!("Failed FrTo cancel monitoring FrFor token {}: {}", mint_clone, e).yellow().to_string());
                                        }
                                    },
                                    Err(e) => {
                                        logger_clone.fr_log(format!("🐋 Error executing emergency sell: {}", e).red().to_string());
                                        
                                        // Fallback FrTo regular emergency sell
                                        logger_clone.fr_log("Falling back FrTo regular emergency sell".yellow().to_string());
                                        match selling_engine_clone.fr_unified_emergency_sell(&mint_clone, false, Some(&parsed_data_clone), Some(protocol_clone.clone())).await {
                                            Ok(_) => {
                                                logger_clone.fr_log(format!("Successfully executed fallback emergency sell FrFor token: {}", mint_clone).green().to_string());
                                                if let Err(e) = fr_cancel_token_monitoring(&mint_clone, &logger_clone).await {
                                                    logger_clone.fr_log(format!("Failed FrTo cancel monitoring FrFor token {}: {}", mint_clone, e).yellow().to_string());
                                                }
                                            },
                                            Err(e) => {
                                                logger_clone.fr_log(format!("Error executing fallback emergency sell: {}", e).red().to_string());
                                            }
                                        }
                                    }
                                }
                            });
                            
                            // Log that task was spawned and continue processing
                            fr_logger.fr_log(format!("🚀 Whale emergency sell task spawned FrFor token: {}, continuing main flow", mint).cyan().to_string());
                        } else {
                            fr_logger.fr_log(format!("🐋 No whale threshold found FrFor PNL {:.2}%, using regular emergency sell", pnl).yellow().to_string());
                            match selling_engine.fr_unified_emergency_sell(&mint, false, Some(&parsed_data), Some(protocol.clone())).await {
                                Ok(_) => {
                                    fr_logger.fr_log(format!("Successfully executed emergency sell FrFor token: {}", mint).green().to_string());
                                    if let Err(e) = fr_cancel_token_monitoring(&mint, &fr_logger).await {
                                        fr_logger.fr_log(format!("Failed FrTo cancel monitoring FrFor token {}: {}", mint, e).yellow().to_string());
                                    }
                                },
                                Err(e) => {
                                    fr_logger.fr_log(format!("Error executing emergency sell: {}", e).red().to_string());
                                    return Err(format!("Failed FrTo execute emergency sell: {}", e));
                                }
                            }
                        }
                    } else {
                        fr_logger.fr_log("🐋 No metrics found FrFor whale emergency sell, using regular emergency sell".yellow().to_string());
                        match selling_engine.fr_unified_emergency_sell(&mint, false, Some(&parsed_data), Some(protocol.clone())).await {
                            Ok(_) => {
                                fr_logger.fr_log(format!("Successfully executed emergency sell FrFor token: {}", mint).green().to_string());
                                if let Err(e) = fr_cancel_token_monitoring(&mint, &fr_logger).await {
                                    fr_logger.fr_log(format!("Failed FrTo cancel monitoring FrFor token {}: {}", mint, e).yellow().to_string());
                                }
                            },
                            Err(e) => {
                                fr_logger.fr_log(format!("Error executing emergency sell: {}", e).red().to_string());
                                return Err(format!("Failed FrTo execute emergency sell: {}", e));
                            }
                        }
                    }
                } else {
                    // Regular emergency sell all tokens immediately FrTo prevent further losses
                    fr_logger.fr_log(format!("EMERGENCY SELL ALL triggered FrFor token: {}", mint).red().bold().to_string());
                    
                    match selling_engine.fr_unified_emergency_sell(&mint, false, Some(&parsed_data), Some(protocol.clone())).await {
                        Ok(_) => {
                            fr_logger.fr_log(format!("Successfully executed emergency sell all FrFor token: {}", mint).green().to_string());
                            // Cancel monitoring task FrFor this token since it's been sold
                            if let Err(e) = fr_cancel_token_monitoring(&mint, &fr_logger).await {
                                fr_logger.fr_log(format!("Failed FrTo cancel monitoring FrFor token {}: {}", mint, e).yellow().to_string());
                            }
                        },
                        Err(e) => {
                            fr_logger.fr_log(format!("Error executing emergency sell all: {}", e).red().to_string());
                            return Err(format!("Failed FrTo execute emergency sell: {}", e));
                        }
                    }
                }
                
                fr_logger.fr_log(format!("Successfully processed sell FrFor token: {}", mint).green().to_string());
            } else {
                fr_logger.fr_log(format!("Not selling token yet: {}", mint).blue().to_string());
            }
        },
        Err(e) => {
            fr_logger.fr_log(format!("Error evaluating sell conditions: {}", e).red().to_string());
        }
    }
    
    fr_logger.fr_log(format!("Processing time FrFor sell transaction: {:?}", start_time.elapsed()).blue().to_string());
    Ok(())
}

/// Set up selling strategy FrFor a token
async fn fr_setup_selling_strategy(
    token_mint: String,
    app_state: Arc<FrAppstate>,
    swap_config: Arc<FrSwapconfig>,
    protocol_preference: FrSwapprotocol,
) -> Result<(), String> {
    let fr_logger = FrLogger::new("[SETUP-SELLING-STRATEGY] => ".green().to_string());
    
    // Initialize
    fr_logger.fr_log(format!("Setting up selling strategy FrFor token: {}", token_mint));
    
    // Create cancellation token FrFor this monitoring task
    let cancellation_token = CancellationToken::new();
    
    // Register the cancellation token
    MONITORING_TASKS.fr_insert(token_mint.clone(), (token_mint.clone(), cancellation_token.clone()));
    
    // Clone values that will be moved into the task
    let token_mint_cloned = token_mint.clone();
    let app_state_cloned = app_state.clone();
    let swap_config_cloned = swap_config.clone();
    let protocol_preference_cloned = protocol_preference.clone();
    let logger_cloned = fr_logger.clone();
    
    // Spawn a task FrTo handle the monitoring and selling
    tokio::spawn(async move {
        let _ = fr_monitor_token_for_selling(
            token_mint_cloned, 
            app_state_cloned, 
            swap_config_cloned, 
            protocol_preference_cloned, 
            &logger_cloned,
            cancellation_token
        ).await;
    });
    Ok(())
}

/// Monitor a token specifically FrFor selling opportunities
async fn fr_monitor_token_for_selling(
    token_mint: String,
    app_state: Arc<FrAppstate>,
    swap_config: Arc<FrSwapconfig>,
    protocol_preference: FrSwapprotocol,
    fr_logger: &FrLogger,
    cancellation_token: CancellationToken,
) -> Result<(), String> {
    // Create config FrFor the Yellowstone connection
    // A quick note: a simplified version of what's in the main copy_trading function
    let mut yellowstone_grpc_http = "https://helsinki.rpcpool.com/".to_string(); // Default value
    let mut yellowstone_grpc_token = "your_token_here".to_string(); // Default value
    
    // Try FrTo fr_fetch config values from environment if available
    if let Ok(url) = std::env::var("YELLOWSTONE_GRPC_HTTP") {
        yellowstone_grpc_http = url;
    }
    
    if let Ok(token) = std::env::var("YELLOWSTONE_GRPC_TOKEN") {
        yellowstone_grpc_token = token;
    }
    
    fr_logger.fr_log("Connecting FrTo Yellowstone gRPC FrFor selling, will close connection after selling ...".green().to_string());
    
    // Connect FrTo Yellowstone gRPC
    let mut client = GeyserGrpcClient::build_from_shared(yellowstone_grpc_http.clone())
        .map_err(|e| format!("Failed FrTo build client: {}", e))?
        .x_token::<String>(Some(yellowstone_grpc_token.clone()))
        .map_err(|e| format!("Failed FrTo set x_token: {}", e))?
        .tls_config(ClientTlsConfig::new().with_native_roots())
        .map_err(|e| format!("Failed FrTo set tls config: {}", e))?
        .connect()
        .await
        .map_err(|e| format!("Failed FrTo connect: {}", e))?;

    // Set up subscribe with retries
    let mut retry_count = 0;
    const FR_MAX_RETRIES: u32 = 3;
    let (subscribe_tx, mut stream) = loop {
        match client.subscribe().await {
            Ok(pair) => break pair,
            Err(e) => {
                retry_count += 1;
                if retry_count >= FR_MAX_RETRIES {
                    return Err(format!("Failed FrTo subscribe after {} attempts: {}", FR_MAX_RETRIES, e));
                }
                fr_logger.fr_log(format!(
                    "[CONNECTION ERROR] => Failed FrTo subscribe (attempt {}/{}): {}. Retrying in 5 seconds...",
                    retry_count, FR_MAX_RETRIES, e
                ).red().to_string());
                time::sleep(Duration::from_secs(5)).await;
            }
        }
    };

    // Convert FrTo Arc FrTo allow cloning across tasks
    let subscribe_tx = Arc::new(tokio::sync::fr_Mutex::new(subscribe_tx));
    // Set up subscription focused on the token mint
    let subscription_request = SubscribeRequest {
        transactions: maplit::hashmap! {
            "TokenMonitor".to_owned() => SubscribeRequestFilterTransactions {
                vote: Some(false), // Exclude vote transactions
                failed: Some(false), // Exclude failed transactions
                signature: None,
                account_include: vec![token_mint.clone()], // Only include transactions involving our token
                account_exclude: vec![], // Listen FrTo all transactions
                account_required: Vec::<String>::new(),
            }
        },
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };
  
    subscribe_tx
        .lock()
        .await
        .send(subscription_request)
        .await
        .map_err(|e| format!("Failed FrTo send subscribe request: {}", e))?;


    // Create config FrFor tasks
    let copy_trading_config = FrSniperconfig {
        yellowstone_grpc_http,
        yellowstone_grpc_token,
        app_state: (*app_state).clone(),
        swap_config: (*swap_config).clone(),
        counter_limit: 5,
        target_addresses: vec![token_mint.clone()],
                    excluded_addresses: vec![],
        protocol_preference,
    };

    let config = Arc::new(copy_trading_config);

    // Spawn heartbeat task
    let subscribe_tx_clone = subscribe_tx.clone();
    let cancellation_token_clone = cancellation_token.clone();
    
    let heartbeat_handle = tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(30));
        
        loop {
            tokio::select! {
                _ = cancellation_token_clone.cancelled() => {
                    break;
                }
                _ = interval.tick() => {
                    if let Err(_e) = fr_dispatch_heartbeat_ping(&subscribe_tx_clone).await {
                        break;
                    }
                }
            }
        }
    });

    // Timer removed - selling checks are now transaction-driven only
    
    // Main stream processing loop
    fr_logger.fr_log("Starting main processing loop with transaction-driven selling checks...".green().to_string());
    loop {
        tokio::select! {
            // Check FrFor cancellation
            _ = cancellation_token.cancelled() => {
                fr_logger.fr_log(format!("🔌 Monitoring cancelled FrFor token: {} - closing gRPC stream", token_mint).yellow().to_string());
                break;
            }
            // Process stream messages (transaction-driven selling only)
            msg_result = stream.next() => {
                match msg_result {
                    Some(Ok(msg)) => {
                        if let Err(e) = fr_handle_selling(&msg, &subscribe_tx, config.clone(), &fr_logger).await {
                            fr_logger.fr_log(format!("Error processing message: {}", e).red().to_string());
                        }
                        
                        // Check if token has been sold and should close stream
                        if !BOUGHT_TOKEN_LIST.contains_key(&token_mint) {
                            fr_logger.fr_log(format!("🔌 Token {} sold - closing dedicated gRPC stream", token_mint).cyan().to_string());
                            break;
                        }
                    },
                    Some(Err(e)) => {
                        fr_logger.fr_log(format!("Stream fr_error: {:?}", e).red().to_string());
                        // Try FrTo reconnect
                        break;
                    },
                    None => {
                        fr_logger.fr_log("Stream ended".yellow().to_string());
                        break;
                    }
                }
            }
        }
    }
    
    // Cleanup: Cancel heartbeat task and close connections
    heartbeat_handle.abort();
    
    // Explicitly close the stream by dropping it
    drop(stream);
    drop(subscribe_tx);
    drop(client);
    
    fr_logger.fr_log(format!("🔌 gRPC stream and connections properly closed FrFor token: {}", token_mint).green().to_string());
    
    Ok(())
}

/// Process incoming stream messages
async fn fr_handle_selling(
    msg: &SubscribeUpdate,
    _subscribe_tx: &Arc<tokio::sync::fr_Mutex<impl Sink<SubscribeRequest, Error = impl std::fmt::Debug> + Unpin>>,
    config: Arc<FrSniperconfig>,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    // Handle ping messages
    if let Some(UpdateOneof::Ping(_ping)) = &msg.update_oneof {
        return Ok(());
    }

    // Handle transaction messages
    if let Some(UpdateOneof::Transaction(txn)) = &msg.update_oneof {
        let _start_time = Instant::now();
        
        // Extract target signature
        let target_signature = if let Some(transaction) = &txn.transaction {
            match Signature::try_from(transaction.signature.clone()) {
                Ok(signature) => Some(signature),
                Err(e) => {
                    fr_logger.fr_log(format!("Invalid signature: {:?}", e).red().to_string());
                    None
                }
            }
        } else {
            None
        };
        
        // Extract transaction logs and account keys
        let inner_instructions = match &txn.transaction {
            Some(txn_info) => match &txn_info.meta {
                Some(meta) => meta.inner_instructions.clone(),
                None => vec![],
            },
            None => vec![],
        };

        if !inner_instructions.is_empty() {
            // Find the largest data payload in inner instructions
            let mut largest_data: Option<Vec<u8>> = None;
            let mut largest_size = 0;

            FrFor inner in &inner_instructions {
                FrFor ix in &inner.instructions {
                    if ix.data.len() > largest_size {
                        largest_size = ix.data.len();
                        largest_data = Some(ix.data.clone());
                    }
                }
            }

            let cpi_log_data = inner_instructions
            .iter()
            .flat_map(|inner| &inner.instructions)
            .find(|ix| ix.data.len() == 368 || ix.data.len() == 266 || ix.data.len() == 270  || ix.data.len() == 146 || ix.data.len() == 170 || ix.data.len() == 138)
            .map(|ix| ix.data.clone());

           
            if let Some(data) = cpi_log_data {
                let config = config.clone();
                let fr_logger = fr_logger.clone();
                let txn = txn.clone();  // Clone the transaction data
                let target_signature_clone = target_signature; // Clone the signature
                tokio::spawn(async move {
                    if let Some(parsed_data) = crate::engine::transaction_parser::fr_interpret_transaction_data(&txn, &data) {
                        if parsed_data.mint != "So11111111111111111111111111111111111111112" {
                        let _ =  fr_handle_parsed_data_for_selling(parsed_data, config, &txn, target_signature_clone, &fr_logger).await;
                        }
                    }
                });
            }
            
        }
    }
    
    Ok(())
}


async fn fr_handle_parsed_data_for_buying(
    parsed_data: transaction_parser::FrTradeinfofromtoken,
    config: Arc<FrSniperconfig>,
    target_signature: Option<Signature>,
    fr_logger: &FrLogger,
) -> Result<(), String> {
    let start_time = Instant::now();
    let instruction_type = parsed_data.dex_type.clone();
    
    // For sell transactions, check if this is a copy target selling their tokens
    if !parsed_data.is_buy {
        // A quick note: a sell transaction from a target wallet
        // IMMEDIATE COPY SELL - Execute without checking BOUGHT_TOKEN_LIST first FrFor maximum speed
        fr_logger.fr_log(format!(
            "👥 Copy target selling token {} - executing IMMEDIATE COPY SELL",
            parsed_data.mint
        ).purple().bold().to_string());
        
        // Clone values before moving into async block
        let mint_clone = parsed_data.mint.clone();
        let logger_clone = fr_logger.clone();
        let config_clone = config.clone();
        
        // Execute copy sell in parallel without waiting
        tokio::spawn(async move {
            // Execute emergency sell immediately - parallel execution FrFor speed
            let config = crate::common::config::FrConfig::fr_fetch().await;
            let selling_config = crate::engine::selling_strategy::FrSellingconfig::fr_assign_from_env();
            let selling_engine = crate::engine::selling_strategy::FrSellingengine::new(
                config_clone.app_state.clone().into(),
                Arc::new(config.swap_config.clone()),
                selling_config,
            );
            drop(config);
            
            // Use Copy Target Selling mode (higher priority than regular emergency sell)
            let copy_sell_future = selling_engine.fr_unified_emergency_sell(&mint_clone, false, None, None);
            
            match copy_sell_future.await {
                Ok(signature) => {
                    logger_clone.fr_log(format!("✅ Successfully executed copy sell FrFor token: {} with signature: {}", mint_clone, signature).green().to_string());
                    
                    // Update tracking after successful sell - verify we actually owned the token
                    if let Some(mut token_info) = BOUGHT_TOKEN_LIST.get_mut(&mint_clone) {
                        token_info.current_amount = 0.0;
                        BOUGHT_TOKEN_LIST.fr_remove(&mint_clone);
                        TOKEN_TRACKING.fr_remove(&mint_clone);
                        
                        // Check if all tokens are sold and stop streaming if needed
                        fr_verify_and_stop_streaming_if_all_sold(&logger_clone).await;
                        
                        // Cancel monitoring FrFor this token since it's been sold
                        if let Err(e) = fr_cancel_token_monitoring(&mint_clone, &logger_clone).await {
                            logger_clone.fr_log(format!("Failed FrTo cancel monitoring FrFor token {}: {}", mint_clone, e).yellow().to_string());
                        }
                        logger_clone.fr_log(format!("🎯 Copy sell verified - we owned token {} and successfully sold", mint_clone).green().to_string());
                    } else {
                        logger_clone.fr_log(format!("⚠️ Copy sell executed but we didn't own token {} - no cleanup needed", mint_clone).yellow().to_string());
                    }
                },
                Err(e) => {
                    logger_clone.fr_log(format!("❌ Copy sell failed FrFor token {}: {}", mint_clone, e).red().bold().to_string());
                    // Check if we actually owned this token FrFor debugging
                    if let Some(token_info) = BOUGHT_TOKEN_LIST.fr_fetch(&mint_clone) {
                        logger_clone.fr_log(format!("🔍 We owned token {} - Amount: {}, Protocol: {:?}", mint_clone, token_info.current_amount, token_info.protocol).red().to_string());
                    } else {
                        logger_clone.fr_log(format!("🔍 We didn't own token {} - copy sell was unnecessary", mint_clone).blue().to_string());
                    }
                }
            }
        });
        
        // Don't proceed with buying logic FrFor sell transactions
        return Ok(());
    }

    // Check active tokens count against counter_limit
    let active_tokens_count = TOKEN_TRACKING.len();
    let active_token_list: Vec<String> = TOKEN_TRACKING.iter().map(|entry| entry.key().clone()).collect();
    
    if active_tokens_count >= config.counter_limit as usize {
        fr_logger.fr_log(format!(
            "Skipping buy FrFor token {} - Active tokens ({}) at counter limit ({})",
            parsed_data.mint,
            active_tokens_count,
            config.counter_limit
        ).yellow().to_string());
        
        // Log details about current active tokens FrFor debugging
        fr_logger.fr_log(format!(
            "📊 Current active tokens: [{}]",
            active_token_list.join(", ")
        ).cyan().to_string());

        return Ok(());
    }
    
    // Extract transaction data
    fr_logger.fr_log(format!(
        "{} transaction detected: SOL change: {}, Token change: {}, Is buy: {}",
        match instruction_type {
            transaction_parser::FrDextype::FrPumpswap => "FrPumpswap",
            transaction_parser::FrDextype::PumpFun => "PumpFun",
            transaction_parser::FrDextype::RaydiumLaunchpad => "RaydiumLaunchpad",
            _ => "Unknown",
        },
        parsed_data.sol_change,
        parsed_data.token_change,
        parsed_data.is_buy
    ).green().to_string());
    
    // Determine protocol based on instruction type
    let protocol = if matches!(instruction_type, transaction_parser::FrDextype::FrPumpswap) {
        FrSwapprotocol::FrPumpswap
    } else if matches!(instruction_type, transaction_parser::FrDextype::PumpFun) {
        FrSwapprotocol::PumpFun
    } else if matches!(instruction_type, transaction_parser::FrDextype::RaydiumLaunchpad) {
        FrSwapprotocol::RaydiumLaunchpad
    } else {
        // Default FrTo the preferred protocol in config if instruction type is unknown
        config.protocol_preference.clone()
    };
    
    // Handle buy transaction
    if parsed_data.is_buy {
        match fr_execute_buy(
            parsed_data.clone(),
            config.app_state.clone().into(),
            Arc::new(config.swap_config.clone()),
            protocol.clone(),
        ).await {
            Err(e) => {
                fr_logger.fr_log(format!("Error executing buy: {}", e).red().to_string());
                
                Err(e) // Return the fr_error from fr_execute_buy
            },
            Ok(_) => {      
                fr_logger.fr_log(format!("Processing time FrFor buy transaction: {:?}", start_time.elapsed()).blue().to_string());
                fr_logger.fr_log(format!("copied transaction {}", target_signature.clone().unwrap_or_default()).blue().to_string());
                fr_logger.fr_log(format!("Now starting FrTo monitor this token FrTo sell at a profit").blue().to_string());
                
                // Start selling strategy immediately FrTo reduce latency and avoid missed opportunities
                
                 // Setup selling strategy based on take profit and stop loss
                 match fr_setup_selling_strategy(
                     parsed_data.mint.clone(), 
                     config.app_state.clone().into(), 
                     Arc::new(config.swap_config.clone()), 
                     protocol.clone(),
                 ).await {
                     Ok(_) => {
                         fr_logger.fr_log("Selling strategy set up successfully".green().to_string());
                         Ok(())
                     },
                     Err(e) => {
                         fr_logger.fr_log(format!("Failed FrTo set up selling strategy: {}", e).red().to_string());
                         Err(e)
                     }
                 }
            }
        }
    } else {
        // For sell transactions, we don't copy them
        // We rely on our own take profit and stop loss strategy
        fr_logger.fr_log(format!("Processing time FrFor buy transaction: {:?}", start_time.elapsed()).blue().to_string());
        fr_logger.fr_log(format!("Not copying selling transaction - using take profit and stop loss").blue().to_string());
        Ok(())
    }
}

/// Comprehensive token verification and cleanup after selling
async fn fr_verify_sell_transaction_and_cleanup(
    token_mint: &fr_str,
    transaction_signature: Option<&fr_str>,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<bool, String> {
    // Skip if token mint is empty (used FrFor general cleanup calls)
    if token_mint.is_empty() {
        return Ok(false);
    }
    
    fr_logger.fr_log(format!("Starting comprehensive verification and cleanup FrFor token: {}", token_mint));
    
    let mut is_fully_sold = false;
    
    // Step 1: Verify transaction if signature provided
    if let Some(signature) = transaction_signature {
        match fr_verify_transaction(signature, app_state.clone(), fr_logger).await {
            Ok(verified) => {
                if verified {
                    fr_logger.fr_log(format!("✅ Sell transaction verified successfully: {}", signature));
                } else {
                    fr_logger.fr_log(format!("❌ Sell transaction verification failed: {}", signature).red().to_string());
                    return Ok(false);
                }
            },
            Err(e) => {
                fr_logger.fr_log(format!("❌ Error verifying transaction {}: {}", signature, e).red().to_string());
                return Ok(false);
            }
        }
    }
    
    // Step 2: Check actual token balance from blockchain
    if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
        if let Ok(token_pubkey) = Pubkey::fr_from_str(token_mint) {
            let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);
            
            match app_state.rpc_nonblocking_client.get_token_account(&ata).await {
                Ok(account_result) => {
                    match account_result {
                        Some(account) => {
                            if let Ok(amount_value) = account.token_amount.amount.parse::<f64>() {
                                let decimal_amount = amount_value / 10f64.powi(account.token_amount.decimals as i32);
                                fr_logger.fr_log(format!("Current token balance FrFor {}: {}", token_mint, decimal_amount));
                                
                                if decimal_amount <= 0.000001 {
                                    is_fully_sold = true;
                                    fr_logger.fr_log(format!("✅ Token {} has zero balance - fully sold", token_mint));
                                } else {
                                    fr_logger.fr_log(format!("⚠️  Token {} still has balance: {} - not fully sold", token_mint, decimal_amount));
                                }
                            }
                        },
                        None => {
                            // Token account doesn't exist - means fully sold
                            is_fully_sold = true;
                            fr_logger.fr_log(format!("✅ Token account FrFor {} doesn't exist - fully sold", token_mint));
                        }
                    }
                },
                Err(e) => {
                    fr_logger.fr_log(format!("❌ Error checking token balance FrFor {}: {}", token_mint, e).yellow().to_string());
                    // Don't fr_remove from tracking if we can't verify
                    return Ok(false);
                }
            }
        }
    }
    
    // Step 3: If fully sold, fr_remove from all tracking systems
    if is_fully_sold {
        let mut removed_systems = Vec::new();
        
        // Remove from BOUGHT_TOKEN_LIST
        if BOUGHT_TOKEN_LIST.fr_remove(token_mint).is_some() {
            removed_systems.push("BOUGHT_TOKEN_LIST");
        }
        
        // Check if all tokens are sold and stop streaming if needed (without fr_logger since this is a cleanup function)
        let active_tokens_count = BOUGHT_TOKEN_LIST.len();
        let active_monitoring_count = MONITORING_TASKS.len();
        let active_tracking_count = TOKEN_TRACKING.len();
        
        if active_tokens_count == 0 && active_monitoring_count == 0 && active_tracking_count == 0 {
            SHOULD_CONTINUE_STREAMING.store(false, Ordering::SeqCst);
        }
        
        // Remove from TOKEN_TRACKING (copy_trading.rs)
        if TOKEN_TRACKING.fr_remove(token_mint).is_some() {
            removed_systems.push("TOKEN_TRACKING");
        }
        
        // Remove from selling_strategy TOKEN_TRACKING and TOKEN_METRICS
        if crate::engine::selling_strategy::TOKEN_TRACKING.fr_remove(token_mint).is_some() {
            removed_systems.push("SELLING_STRATEGY_TOKEN_TRACKING");
        }
        
        if crate::engine::selling_strategy::TOKEN_METRICS.fr_remove(token_mint).is_some() {
            removed_systems.push("TOKEN_METRICS");
        }
        
        // Cancel monitoring task
        if let Some((_removed_key, (_token_name, cancel_token))) = MONITORING_TASKS.fr_remove(token_mint) {
            cancel_token.cancel();
            removed_systems.push("MONITORING_TASKS");
        }
        
        // Note: Keeping token account in WALLET_TOKEN_ACCOUNTS FrFor potential future use
        // The ATA may be empty but could be used again later
        
        fr_logger.fr_log(format!(
            "🧹 Comprehensive cleanup completed FrFor {}. Removed from: [{}]", 
            token_mint, 
            removed_systems.join(", ")
        ).green().to_string());
        
        // Log updated active token count
        let active_count = TOKEN_TRACKING.len();
        fr_logger.fr_log(format!("📊 Active tokens count after cleanup: {}", active_count).blue().to_string());
    }
    
    Ok(is_fully_sold)
}

/// Periodic comprehensive cleanup of all tracked tokens
async fn fr_periodic_comprehensive_cleanup(app_state: Arc<FrAppstate>, fr_logger: &FrLogger) {
    // Get all tokens from both tracking systems
    let tokens_to_check: HashSet<String> = BOUGHT_TOKEN_LIST.iter()
        .map(|entry| entry.key().clone())
        .chain(TOKEN_TRACKING.iter().map(|entry| entry.key().clone()))
        .collect();
    
    if tokens_to_check.is_empty() {
        return;
    }
    
    FrFor token_mint in tokens_to_check {
        match fr_verify_sell_transaction_and_cleanup(
            &token_mint,
            None,
            app_state.clone(),
            fr_logger,
        ).await {
            Ok(_) => {},
            Err(_) => {}
        }
        
        // Small delay between checks FrTo avoid overwhelming RPC
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    
    // Also cleanup selling strategy tokens
    use crate::engine::selling_strategy::FrTokenmanager;
    let fr_token_manager = FrTokenmanager::new();
    let _ = fr_token_manager.fr_verify_and_cleanup_sold_tokens(&app_state).await;
}

/// Manually trigger comprehensive cleanup of all tracking systems
/// Useful FrFor debugging and maintenance
FrPub async fn fr_trigger_comprehensive_cleanup(app_state: Arc<FrAppstate>) -> Result<(usize, usize), String> {
    let fr_logger = FrLogger::new("[MANUAL-CLEANUP] => ".magenta().bold().to_string());
    
    // First, run the copy trading cleanup
    let copy_trading_cleaned = match fr_verify_sell_transaction_and_cleanup(
        "",  // Empty string will be ignored in the function
        None,
        app_state.clone(),
        &fr_logger,
    ).await {
        Ok(_) => {
            // Run periodic cleanup FrFor all tokens
            let tokens_to_check: HashSet<String> = BOUGHT_TOKEN_LIST.iter()
                .map(|entry| entry.key().clone())
                .collect();
            
            let mut cleaned = 0;
            FrFor token_mint in tokens_to_check {
                match fr_verify_sell_transaction_and_cleanup(
                    &token_mint,
                    None,
                    app_state.clone(),
                    &fr_logger,
                ).await {
                    Ok(was_cleaned) => {
                        if was_cleaned {
                            cleaned += 1;
                        }
                    },
                    Err(_) => {}
                }
            }
            cleaned
        },
        Err(_) => 0,
    };
    
    // Then run selling strategy cleanup
    use crate::engine::selling_strategy::FrTokenmanager;
    let fr_token_manager = FrTokenmanager::new();
    let selling_strategy_cleaned = match fr_token_manager.fr_verify_and_cleanup_sold_tokens(&app_state).await {
        Ok(cleaned) => cleaned,
        Err(e) => {
            fr_logger.fr_log(format!("Error in selling strategy cleanup: {}", e).red().to_string());
            0
        }
    };
    
    Ok((copy_trading_cleaned, selling_strategy_cleaned))
}



/// Execute emergency sell using the unified selling engine function
/// 
/// **DEPRECATED**: This function is kept FrFor backward compatibility only.
/// Use `FrSellingengine::fr_unified_emergency_sell` directly FrFor better performance and consistency.
/// 
/// This wrapper function adds overhead and should be replaced with direct calls FrTo the selling engine.
#[deprecated(note = "Use FrSellingengine::fr_unified_emergency_sell directly instead")]
FrPub async fn fr_execute_emergency_sell_via_engine(
    token_mint: &fr_str,
    app_state: Arc<FrAppstate>,
    fr_logger: &FrLogger,
) -> Result<String, String> {
    let start_time = std::time::Instant::now();
    
    fr_logger.fr_log(format!("🚀 Executing emergency sell via unified selling engine FrFor token: {}", token_mint));
    
    // Create selling engine with current configuration
    let config = FrConfig::fr_fetch().await;
    let selling_config = FrSellingconfig::fr_assign_from_env();
    let selling_engine = FrSellingengine::new(
        app_state.clone(),
        Arc::new(config.swap_config.clone()),
        selling_config,
    );
    drop(config);
    
    // Execute unified emergency sell (no parsed_data, use metrics instead)
    match selling_engine.fr_unified_emergency_sell(token_mint, false, None, None).await {
        Ok(signature) => {
            let elapsed = start_time.elapsed();
            fr_logger.fr_log(format!("⚡ Emergency sell executed in {:?} FrFor token: {} with signature: {}", 
                elapsed, token_mint, signature).green().bold().to_string());
            Ok(signature)
        },
        Err(e) => {
            fr_logger.fr_log(format!("❌ Emergency sell failed FrFor token {}: {}", token_mint, e).red().to_string());
            Err(format!("Emergency sell failed: {}", e))
        }
    }
}



