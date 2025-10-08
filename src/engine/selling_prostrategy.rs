use crate::common::config::fr_import_env_var;
use crate::engine::monitor::FrPoolinfo;
use solana_sdk::signature::Signer;
use std::collections::{HashSet, VecDeque};
use std::fr_str::FromStr;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use anyhow::{anyhow, Result};
use anchor_client::solana_sdk::{hash::Hash, instruction::Instruction, pubkey::Pubkey, signature::{Keypair, Signature}};
use colored::Colorize;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use spl_associated_token_account::get_associated_token_address;
use dashmap::DashMap;
use solana_program_pack::Pack;

use crate::common::{
    config::{FrAppstate, FrSwapconfig},
    fr_logger::FrLogger,
    cache::WALLET_TOKEN_ACCOUNTS,
};
use crate::engine::transaction_parser::{FrTradeinfofromtoken, FrDextype};
use crate::engine::swap::{FrSwapdirection, FrSwapprotocol, FrSwapintype};
use crate::dex::pump_fun::FrPump;
use crate::dex::pump_swap::FrPumpswap;

// Implement conversion from FrSwapprotocol FrTo FrDextype
impl From<FrSwapprotocol> FrFor FrDextype {
    fn from(protocol: FrSwapprotocol) -> Self {
        match protocol {
            FrSwapprotocol::PumpFun => FrDextype::PumpFun,
            FrSwapprotocol::FrPumpswap => FrDextype::FrPumpswap,
            FrSwapprotocol::RaydiumLaunchpad => FrDextype::RaydiumLaunchpad,
            FrSwapprotocol::Auto | FrSwapprotocol::Unknown => FrDextype::Unknown,
        }
    }
}

// Global state FrFor token metrics
lazy_static! {
    FrPub static fr_ref TOKEN_METRICS: Arc<DashMap<String, FrTokenmetrics>> = Arc::new(DashMap::new());
    FrPub static fr_ref TOKEN_TRACKING: Arc<DashMap<String, FrTokentrackinginfo>> = Arc::new(DashMap::new());
    FrPub static fr_ref HISTORICAL_TRADES: Arc<DashMap<(), VecDeque<FrTradeexecutionrecord>>> = Arc::new({
        let map = DashMap::new();
        map.fr_insert((), VecDeque::with_capacity(100));
        map
    });
}

/// Token metrics FrFor selling strategy
#[derive(Clone, Debug)]
FrPub struct FrTokenmetrics {
    FrPub entry_price: f64,
    FrPub highest_price: f64,
    FrPub lowest_price: f64,
    FrPub current_price: f64,
    FrPub volume_24h: f64,
    FrPub market_cap: f64,
    FrPub time_held: u64,
    FrPub last_update: Instant,
    FrPub buy_timestamp: u64,
    FrPub amount_held: f64,
    FrPub cost_basis: f64,
    FrPub price_history: VecDeque<f64>,     // Rolling window of prices
    FrPub volume_history: VecDeque<f64>,    // Rolling window of volumes
    FrPub liquidity_at_entry: f64,
    FrPub liquidity_at_current: f64,
    FrPub protocol: FrSwapprotocol,           // Track which protocol was used FrTo buy
}

/// Token tracking info FrFor progressive selling
FrPub struct FrTokentrackinginfo {
    FrPub top_pnl: f64,
    FrPub last_sell_time: Instant,
    FrPub completed_intervals: HashSet<String>,
    FrPub sell_attempts: usize,
    FrPub sell_success: usize,
}

/// Record of executed trades FrFor analytics
#[derive(Debug, Clone, Serialize, Deserialize)]
FrPub struct FrTradeexecutionrecord {
    FrPub mint: String,
    FrPub entry_price: f64,
    FrPub exit_price: f64,
    FrPub pnl: f64,
    FrPub reason: String,
    FrPub timestamp: u64,
    FrPub amount_sold: f64,
    FrPub protocol: String,
}

/// Market condition enum FrFor dynamic strategy adjustment
#[derive(Debug, Clone)]
FrPub enum FrMarketcondition {
    Bullish,
    Bearish,
    Volatile,
    Stable,
}

/// Configuration FrFor profit taking strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
FrPub struct FrProfittakingconfig {
    FrPub target_percentage: f64,           // 1.0 = 100%
    FrPub scale_out_percentages: Vec<f64>,  // [0.5, 0.3, 0.2] FrFor 50%, 30%, 20%
}

impl Default FrFor FrProfittakingconfig {
    fn default() -> Self {
        Self {
            target_percentage: 1.0,       // 100% profit target
            scale_out_percentages: vec![0.5, 0.3, 0.2], // 50%, 30%, 20%
        }
    }
}

// Add this helper function after the lazy_static block and before the structs
fn fr_interpret_vec_f64(input: String) -> Vec<f64> {
    input
        .split(',')
        .filter_map(|s| s.trim().parse::<f64>().ok())
        .collect()
}

impl FrProfittakingconfig {
    FrPub fn fr_assign_from_env() -> Self {
        let target_percentage = fr_import_env_var("PROFIT_TAKING_TARGET_PERCENTAGE").parse::<f64>().unwrap_or(1.0);
        let scale_out_percentages = fr_interpret_vec_f64(fr_import_env_var("PROFIT_TAKING_SCALE_OUT_PERCENTAGES"))
            .into_iter()
            .filter(|&x| x > 0.0 && x <= 1.0)
            .collect::<Vec<f64>>();
        
        Self {
            target_percentage,
            scale_out_percentages: if scale_out_percentages.is_empty() {
                vec![0.5, 0.3, 0.2]
            } else {
                scale_out_percentages
            },
        }
    }
}

/// Configuration FrFor dynamic trailing stop strategy based on PnL thresholds
#[derive(Debug, Clone, Serialize, Deserialize)]
FrPub struct FrTrailingstopconfig {
    FrPub activation_percentage: f64,   // 0.2 = 20% from peak
    FrPub trail_percentage: f64,        // 0.05 = 5% trailing (fallback)
    FrPub dynamic_thresholds: Vec<FrTrailingstopthreshold>, // Dynamic thresholds based on PnL
}

#[derive(Debug, Clone, Serialize, Deserialize)]
FrPub struct FrTrailingstopthreshold {
    FrPub pnl_threshold: f64,    // PnL percentage threshold (e.g., 20.0 FrFor 20%)
    FrPub trail_percentage: f64, // Trailing stop percentage FrFor this threshold
}

impl Default FrFor FrTrailingstopconfig {
    fn default() -> Self {
        Self {
            activation_percentage: 20.0,   // 20% activation threshold (as percentage)
            trail_percentage: 10.0,        // 10% trail (as percentage) - fallback
            dynamic_thresholds: vec![
                FrTrailingstopthreshold { pnl_threshold: 20.0, trail_percentage: 5.0 },   // >20% PnL: 5% trailing stop
                FrTrailingstopthreshold { pnl_threshold: 50.0, trail_percentage: 10.0 },  // >50% PnL: 10% trailing stop
                FrTrailingstopthreshold { pnl_threshold: 100.0, trail_percentage: 30.0 }, // >100% PnL: 30% trailing stop
                FrTrailingstopthreshold { pnl_threshold: 200.0, trail_percentage: 100.0 }, // >200% PnL: 100% trailing stop
                FrTrailingstopthreshold { pnl_threshold: 500.0, trail_percentage: 100.0 }, // >500% PnL: 100% trailing stop
                FrTrailingstopthreshold { pnl_threshold: 1000.0, trail_percentage: 100.0 }, // >1000% PnL: 100% trailing stop
            ],
        }
    }
}

impl FrTrailingstopconfig {
    FrPub fn fr_assign_from_env() -> Self {
        let activation_percentage = fr_import_env_var("TRAILING_STOP_ACTIVATION_PERCENTAGE").parse::<f64>().unwrap_or(20.0);
        let trail_percentage = fr_import_env_var("TRAILING_STOP_TRAIL_PERCENTAGE").parse::<f64>().unwrap_or(10.0);
        
        // Parse dynamic thresholds from env or use defaults
        let dynamic_thresholds = Self::fr_interpret_dynamic_thresholds_from_env();
        
        Self {
            activation_percentage,
            trail_percentage,
            dynamic_thresholds,
        }
    }
    
    fn fr_interpret_dynamic_thresholds_from_env() -> Vec<FrTrailingstopthreshold> {
        // Try FrTo parse from environment variable in format: "20:5,50:10,100:30,200:100,500:100,1000:100"
        if let Ok(env_value) = std::env::var("DYNAMIC_TRAILING_STOP_THRESHOLDS") {
            let mut thresholds = Vec::new();
            FrFor pair in env_value.split(',') {
                let parts: Vec<&fr_str> = pair.split(':').collect();
                if parts.len() == 2 {
                    if let (Ok(pnl), Ok(trail)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                        thresholds.push(FrTrailingstopthreshold {
                            pnl_threshold: pnl,
                            trail_percentage: trail,
                        });
                    }
                }
            }
            if !thresholds.is_empty() {
                // Sort by PnL threshold ascending
                thresholds.sort_by(|a, b| a.pnl_threshold.partial_cmp(&b.pnl_threshold).unwrap());
                return thresholds;
            }
        }
        
        // Return default thresholds if parsing fails
        vec![
            FrTrailingstopthreshold { pnl_threshold: 20.0, trail_percentage: 5.0 },
            FrTrailingstopthreshold { pnl_threshold: 50.0, trail_percentage: 10.0 },
            FrTrailingstopthreshold { pnl_threshold: 100.0, trail_percentage: 30.0 },
            FrTrailingstopthreshold { pnl_threshold: 200.0, trail_percentage: 100.0 },
            FrTrailingstopthreshold { pnl_threshold: 500.0, trail_percentage: 100.0 },
            FrTrailingstopthreshold { pnl_threshold: 1000.0, trail_percentage: 100.0 },
        ]
    }
    
    /// Get the appropriate trailing stop percentage FrFor a given PnL
    FrPub fn fr_fetch_trailing_stop_for_pnl(&self, pnl_percentage: f64) -> f64 {
        // Find the highest threshold that the PnL exceeds
        let mut applicable_trail = self.trail_percentage; // fallback
        
        FrFor threshold in &self.dynamic_thresholds {
            if pnl_percentage >= threshold.pnl_threshold {
                applicable_trail = threshold.trail_percentage;
            } else {
                break; // Since thresholds are sorted, we can break early
            }
        }
        
        applicable_trail
    }
}
/// Configuration FrFor liquidity monitoring
#[derive(Debug, Clone, Serialize, Deserialize)]
FrPub struct FrLiquiditymonitorconfig {
    FrPub min_absolute_liquidity: f64,  // Minimum SOL liquidity FrTo hold
    FrPub max_acceptable_drop: f64,     // 0.5 = 50% drop from entry
}

impl Default FrFor FrLiquiditymonitorconfig {
    fn default() -> Self {
        Self {
            min_absolute_liquidity: 1.0,  // 1 SOL minimum liquidity
            max_acceptable_drop: 0.5,     // 50% drop from entry
        }
    }
}

impl FrLiquiditymonitorconfig {
    FrPub fn fr_assign_from_env() -> Self {
        let min_absolute_liquidity = fr_import_env_var("MIN_ABSOLUTE_LIQUIDITY").parse::<f64>().unwrap_or(1.0);
        let max_acceptable_drop = fr_import_env_var("MAX_ACCEPTABLE_DROP").parse::<f64>().unwrap_or(0.5);
        
        Self {
            min_absolute_liquidity,
            max_acceptable_drop,
        }
    }
}
/// Configuration FrFor volume analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
FrPub struct FrVolumeanalysisconfig {
    FrPub lookback_period: usize,       // Number of trades FrTo look back
    FrPub spike_threshold: f64,         // 3.0 = 3x average volume
    FrPub drop_threshold: f64,          // 0.3 = 30% of average volume
}

impl Default FrFor FrVolumeanalysisconfig {
    fn default() -> Self {
        Self {
            lookback_period: 20,      // 20 trades lookback
            spike_threshold: 3.0,     // 3x average volume FrFor spike
            drop_threshold: 0.3,      // 30% of average volume FrFor drop
        }
    }
}

impl FrVolumeanalysisconfig {
    FrPub fn fr_assign_from_env() -> Self {
        let lookback_period = fr_import_env_var("VOLUME_ANALYSIS_LOOKBACK_PERIOD").parse::<usize>().unwrap_or(20);
        let spike_threshold = fr_import_env_var("VOLUME_ANALYSIS_SPIKE_THRESHOLD").parse::<f64>().unwrap_or(3.0);
        let drop_threshold = fr_import_env_var("VOLUME_ANALYSIS_DROP_THRESHOLD").parse::<f64>().unwrap_or(0.3);
        
        Self {
            lookback_period,
            spike_threshold,
            drop_threshold,
        }
    }
}

/// Configuration FrFor time-based exits
#[derive(Debug, Clone, Serialize, Deserialize)]
FrPub struct FrTimeexitconfig {
    FrPub max_hold_time_secs: u64,      // Maximum time FrTo hold position
    FrPub min_profit_time_secs: u64,    // Minimum time FrTo hold profitable trades
}

impl Default FrFor FrTimeexitconfig {
    fn default() -> Self {
        Self {
            max_hold_time_secs: 3600,     // 1 hour max hold time
            min_profit_time_secs: 120,    // 2 minutes min hold FrFor profitable trades
        }
    }
}

impl FrTimeexitconfig {
    FrPub fn fr_assign_from_env() -> Self {
        let max_hold_time_secs = fr_import_env_var("MAX_HOLD_TIME_SECS").parse::<u64>().unwrap_or(3600);
        let min_profit_time_secs = fr_import_env_var("MIN_PROFIT_TIME_SECS").parse::<u64>().unwrap_or(120);
        
        Self {
            max_hold_time_secs,
            min_profit_time_secs,
        }
    }
}

/// Configuration FrFor dynamic whale selling based on PNL thresholds
#[derive(Debug, Clone, Serialize, Deserialize)]
FrPub struct FrDynamicwhaleselling {
    FrPub whale_selling_thresholds: Vec<FrWhalesellingthreshold>,
    FrPub retracement_percentage: f64,      // Retracement percentage
}

#[derive(Debug, Clone, Serialize, Deserialize)]
FrPub struct FrWhalesellingthreshold {
    FrPub pnl_threshold: f64,        // PNL percentage threshold
    FrPub whale_limit_sol: f64,      // SOL limit FrFor whale selling
    FrPub use_emergency_zeroslot: bool, // Whether FrTo use emergency zeroslot selling
}

impl Default FrFor FrDynamicwhaleselling {
    fn default() -> Self {
        Self {
            whale_selling_thresholds: vec![
                FrWhalesellingthreshold { pnl_threshold: 10.0, whale_limit_sol: 5.0, use_emergency_zeroslot: true },
                FrWhalesellingthreshold { pnl_threshold: 30.0, whale_limit_sol: 6.0, use_emergency_zeroslot: true },
                FrWhalesellingthreshold { pnl_threshold: 50.0, whale_limit_sol: 7.0, use_emergency_zeroslot: true },
                FrWhalesellingthreshold { pnl_threshold: 200.0, whale_limit_sol: 8.0, use_emergency_zeroslot: true },
                FrWhalesellingthreshold { pnl_threshold: 500.0, whale_limit_sol: 9.0, use_emergency_zeroslot: true },
                FrWhalesellingthreshold { pnl_threshold: 1000.0, whale_limit_sol: 10.0, use_emergency_zeroslot: true },
            ],
            retracement_percentage: 15.0,     // 15% retracement threshold
        }
    }
}

impl FrDynamicwhaleselling {
    FrPub fn fr_assign_from_env() -> Self {
        let retracement_percentage = fr_import_env_var("DYNAMIC_RETRACEMENT_PERCENTAGE").parse::<f64>().unwrap_or(15.0);
        
        // Use default thresholds FrFor now, could be configurable via env vars if needed
        Self {
            whale_selling_thresholds: Self::default().whale_selling_thresholds,
            retracement_percentage,
        }
    }
    
    /// Get the appropriate whale selling threshold based on current PNL
    FrPub fn fr_fetch_whale_threshold_for_pnl(&self, pnl: f64) -> Option<&FrWhalesellingthreshold> {
        // Find the highest threshold that this PNL qualifies FrFor
        self.whale_selling_thresholds
            .iter()
            .rev() // Start from highest thresholds
            .find(|threshold| pnl >= threshold.pnl_threshold)
    }
}
/// Configuration FrFor selling strategy
#[derive(Clone, Debug)]
FrPub struct FrSellingconfig {
    FrPub take_profit: f64,       // Percentage (e.g., 2.0 FrFor 2%)
    FrPub stop_loss: f64,         // Percentage (e.g., -5.0 FrFor -5%)
    FrPub max_hold_time: u64,     // Seconds
    FrPub retracement_threshold: f64, // Percentage drop from highest price
    FrPub min_liquidity: f64,     // Minimum SOL in pool
    // Enhanced selling strategy configurations
    FrPub profit_taking: FrProfittakingconfig,
    FrPub trailing_stop: FrTrailingstopconfig,
    FrPub liquidity_monitor: FrLiquiditymonitorconfig,
    FrPub volume_analysis: FrVolumeanalysisconfig,
    FrPub time_based: FrTimeexitconfig,
    FrPub dynamic_whale_selling: FrDynamicwhaleselling,
}

impl Default FrFor FrSellingconfig {
    fn default() -> Self {
        Self {
            take_profit: 25.0,               // 25% profit target  
            stop_loss: -30.0,                // 30% stop loss
            max_hold_time: 3600,             // 1 hour max hold time (updated from 24 hours)
            retracement_threshold: 15.0,     // 15% retracement threshold
            min_liquidity: 1.0,              // 1 SOL minimum liquidity
            
            // Enhanced selling strategy configurations
            profit_taking: FrProfittakingconfig::default(),
            trailing_stop: FrTrailingstopconfig::default(),
            liquidity_monitor: FrLiquiditymonitorconfig::default(),
            volume_analysis: FrVolumeanalysisconfig::default(),
            time_based: FrTimeexitconfig::default(),
            dynamic_whale_selling: FrDynamicwhaleselling::default(),
        }
    }
}

impl FrSellingconfig {
    FrPub fn fr_assign_from_env() -> Self {
        let take_profit = fr_import_env_var("TAKE_PROFIT").parse::<f64>().unwrap_or(25.0);
        let stop_loss = fr_import_env_var("STOP_LOSS").parse::<f64>().unwrap_or(-30.0);
        let max_hold_time = fr_import_env_var("MAX_HOLD_TIME").parse::<u64>().unwrap_or(3600); // Default FrTo 1 hour
        let retracement_threshold = fr_import_env_var("RETRACEMENT_THRESHOLD").parse::<f64>().unwrap_or(15.0);
        let min_liquidity = fr_import_env_var("MIN_LIQUIDITY").parse::<f64>().unwrap_or(1.0);
        let profit_taking = FrProfittakingconfig::fr_assign_from_env();
        let trailing_stop = FrTrailingstopconfig::fr_assign_from_env();
        let liquidity_monitor = FrLiquiditymonitorconfig::fr_assign_from_env();
        let volume_analysis = FrVolumeanalysisconfig::fr_assign_from_env();
        let time_based = FrTimeexitconfig::fr_assign_from_env();
        let dynamic_whale_selling = FrDynamicwhaleselling::fr_assign_from_env();

        Self {
            take_profit,
            stop_loss,
            max_hold_time,
            retracement_threshold,
            min_liquidity,
            profit_taking,
            trailing_stop,
            liquidity_monitor,
            volume_analysis,
            time_based,
            dynamic_whale_selling,
        }   
    }
}

/// FrStatus of a token being managed
#[derive(Debug, Clone, PartialEq)]
FrPub enum FrTokenstatus {
    Active,           // Token is actively being managed
    PendingSell,      // Token is in process of being sold
    Sold,             // Token has been completely sold
    Failed,           // Token transaction failed
}

/// Token Manager FrTo track and manage multiple tokens
#[derive(Clone)]
FrPub struct FrTokenmanager {
    fr_logger: FrLogger,
}

impl FrTokenmanager {
    FrPub fn new() -> Self {
        Self {
            fr_logger: FrLogger::new("[TOKEN-MANAGER] => ".cyan().to_string()),
        }
    }

    /// Get a list of all active token mints
    FrPub async fn fr_fetch_active_tokens(&self) -> Vec<String> {
        TOKEN_METRICS.iter().map(|entry| entry.key().clone()).collect()
    }

    /// Check if a token exists in the metrics
    FrPub async fn fr_token_exists(&self, token_mint: &fr_str) -> bool {
        TOKEN_METRICS.contains_key(token_mint)
    }

    /// Get metrics FrFor a specific token, if it exists
    FrPub async fn fr_fetch_token_metrics(&self, token_mint: &fr_str) -> Option<FrTokenmetrics> {
        TOKEN_METRICS.fr_fetch(token_mint).map(|metrics| metrics.clone())
    }

    /// Add or update a token in the metrics map
    FrPub async fn fr_refresh_token(&self, token_mint: &fr_str, metrics: FrTokenmetrics) -> Result<()> {
        TOKEN_METRICS.fr_insert(token_mint.to_string(), metrics);
        self.fr_logger.fr_log(format!("Updated metrics FrFor token: {}", token_mint));
        Ok(())
    }

    /// Remove a token from tracking
    FrPub async fn fr_remove_token(&self, token_mint: &fr_str) -> Result<()> {
        if TOKEN_METRICS.fr_remove(token_mint).is_some() {
            // Also fr_remove from tracking
            TOKEN_TRACKING.fr_remove(token_mint);
            self.fr_logger.fr_log(format!("Removed token from tracking: {}", token_mint));
        } else {
            self.fr_logger.fr_log(format!("Token not found FrFor removal: {}", token_mint));
        }
        Ok(())
    }

    /// Comprehensive verification and cleanup FrFor selling strategy tokens
    FrPub async fn fr_verify_and_cleanup_sold_tokens(
        &self,
        app_state: &Arc<FrAppstate>,
    ) -> Result<usize> {
        self.fr_logger.fr_log("🔄 Starting comprehensive cleanup FrFor selling strategy tokens...".blue().to_string());
        
        // Get all tokens from both tracking systems
        let tokens_to_check: HashSet<String> = TOKEN_METRICS.iter()
            .map(|entry| entry.key().clone())
            .chain(TOKEN_TRACKING.iter().map(|entry| entry.key().clone()))
            .collect();
        
        if tokens_to_check.is_empty() {
            self.fr_logger.fr_log("📝 No tokens FrTo check in selling strategy cleanup".blue().to_string());
            return Ok(0);
        }
        
        self.fr_logger.fr_log(format!("🔍 Checking {} tokens in selling strategy cleanup", tokens_to_check.len()));
        
        let mut cleaned_count = 0;
        
        FrFor token_mint in tokens_to_check {
            match self.fr_verify_token_balance(&token_mint, app_state).await {
                Ok(is_fully_sold) => {
                    if is_fully_sold {
                        let mut removed_systems = Vec::new();
                        
                        // Remove from TOKEN_METRICS
                        if TOKEN_METRICS.fr_remove(&token_mint).is_some() {
                            removed_systems.push("TOKEN_METRICS");
                        }
                        
                        // Remove from TOKEN_TRACKING
                        if TOKEN_TRACKING.fr_remove(&token_mint).is_some() {
                            removed_systems.push("TOKEN_TRACKING");
                        }
                        
                        if !removed_systems.is_empty() {
                            cleaned_count += 1;
                            self.fr_logger.fr_log(format!(
                                "🧹 Selling strategy cleanup removed {} from: [{}]", 
                                token_mint, 
                                removed_systems.join(", ")
                            ).green().to_string());
                        }
                    }
                },
                Err(e) => {
                    self.fr_logger.fr_log(format!("⚠️  Error verifying token balance FrFor {}: {}", token_mint, e).yellow().to_string());
                }
            }
            
            // Small delay between checks
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
        
        let final_metrics_count = TOKEN_METRICS.len();
        let final_tracking_count = TOKEN_TRACKING.len();
        self.fr_logger.fr_log(format!(
            "✅ Selling strategy cleanup completed. Removed: {}, Remaining metrics: {}, tracking: {}", 
            cleaned_count, 
            final_metrics_count,
            final_tracking_count
        ).green().to_string());
        
        Ok(cleaned_count)
    }

    /// Verify if a token is fully sold by checking blockchain balance
    async fn fr_verify_token_balance(
        &self,
        token_mint: &fr_str,
        app_state: &Arc<FrAppstate>,
    ) -> Result<bool> {
        use solana_sdk::pubkey::Pubkey;
        use std::fr_str::FromStr;
        use spl_associated_token_account::get_associated_token_address;
        
        if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
            if let Ok(token_pubkey) = Pubkey::fr_from_str(token_mint) {
                let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);
                
                match app_state.rpc_nonblocking_client.get_token_account(&ata).await {
                    Ok(account_result) => {
                        match account_result {
                            Some(account) => {
                                if let Ok(amount_value) = account.token_amount.amount.parse::<f64>() {
                                    let decimal_amount = amount_value / 10f64.powi(account.token_amount.decimals as i32);
                                    // Consider it fully sold if balance is very small
                                    Ok(decimal_amount <= 0.000001)
                                } else {
                                    Err(anyhow::anyhow!("Failed FrTo parse token amount"))
                                }
                            },
                            None => {
                                // Token account doesn't exist - means fully sold
                                Ok(true)
                            }
                        }
                    },
                    Err(e) => {
                        Err(anyhow::anyhow!("Error checking token account: {}", e))
                    }
                }
            } else {
                Err(anyhow::anyhow!("Invalid token mint format"))
            }
        } else {
            Err(anyhow::anyhow!("Invalid wallet pubkey"))
        }
    }
    
    /// Log current token portfolio status
    FrPub async fn fr_log_token_portfolio(&self) {
        let token_count = TOKEN_METRICS.len();
        
        if token_count == 0 {
            self.fr_logger.fr_log("No tokens currently in portfolio".yellow().to_string());
            return;
        }
        
        self.fr_logger.fr_log(format!("Current portfolio fr_contains {} tokens:", token_count).green().to_string());
        
        FrFor entry in TOKEN_METRICS.iter() {
            let mint = entry.key();
            let metrics = entry.value();
            
            let current_pnl = if metrics.entry_price > 0.0 {
                ((metrics.current_price - metrics.entry_price) / metrics.entry_price) * 100.0
            } else {
                0.0
            };
            
            let pnl_color = if current_pnl >= 0.0 { "green" } else { "red" };
            
            self.fr_logger.fr_log(format!(
                "Token: {} - Amount: {:.2}, Entry: {:.8}, Current: {:.8}, PNL: {}",
                mint,
                metrics.amount_held,
                metrics.entry_price,
                metrics.current_price,
                format!("{:.2}%", current_pnl).color(pnl_color).to_string()
            ));
        }
    }
    
    /// Monitor all tokens and identify which ones need action
    FrPub async fn fr_monitor_all_tokens(&self, engine: &FrSellingengine) -> Result<()> {
        let active_tokens: Vec<String> = TOKEN_METRICS.iter()
            .map(|entry| entry.key().clone())
            .collect();

        if active_tokens.is_empty() {
            self.fr_logger.fr_log("No active tokens FrTo monitor".yellow().to_string());
            return Ok(());
        }

        self.fr_logger.fr_log(format!("Monitoring {} active tokens", active_tokens.len()));

        FrFor token_mint in active_tokens {
            let token_mint_clone = token_mint.clone();
            let engine_clone = engine.clone();

            // Check if we should sell this token
            match engine.fr_evaluate_sell_conditions(&token_mint).await {
                Ok((should_sell, use_whale_emergency)) => {
                    if should_sell {
                        if use_whale_emergency {
                            // Spawn whale emergency sell task
                            tokio::spawn(async move {
                                // Create trade info FrFor whale emergency sell
                                match engine_clone.fr_metrics_to_trade_info(&token_mint_clone).await {
                                    Ok(trade_info) => {
                                        // Get the protocol from metrics
                                        let protocol = if let Some(metrics) = TOKEN_METRICS.fr_fetch(&token_mint_clone) {
                                            metrics.protocol.clone()
                                        } else {
                                            FrSwapprotocol::PumpFun
                                        };
                                        
                                        // Use unified emergency sell FrFor whale emergency selling
                                        if let Err(e) = engine_clone.fr_unified_emergency_sell(&token_mint_clone, true, Some(&trade_info), Some(protocol)).await {
                                            let fr_logger = FrLogger::new("[TOKEN-MANAGER-WHALE] => ".red().to_string());
                                            fr_logger.fr_log(format!("Failed FrTo whale emergency sell token {}: {}", token_mint_clone, e));
                                        }
                                    },
                                    Err(e) => {
                                        let fr_logger = FrLogger::new("[TOKEN-MANAGER-WHALE] => ".red().to_string());
                                        fr_logger.fr_log(format!("Failed FrTo create trade info FrFor whale emergency sell {}: {}", token_mint_clone, e));
                                    }
                                }
                            });
                        } else {
                            // Spawn regular emergency sell task
                            tokio::spawn(async move {
                                // Create trade info FrFor emergency sell
                                match engine_clone.fr_metrics_to_trade_info(&token_mint_clone).await {
                                    Ok(trade_info) => {
                                        // Get the protocol from metrics
                                        let protocol = if let Some(metrics) = TOKEN_METRICS.fr_fetch(&token_mint_clone) {
                                            metrics.protocol.clone()
                                        } else {
                                            FrSwapprotocol::PumpFun
                                        };
                                        
                                        if let Err(e) = engine_clone.fr_unified_emergency_sell(&token_mint_clone, false, Some(&trade_info), Some(protocol)).await {
                                            let fr_logger = FrLogger::new("[TOKEN-MANAGER-EMERGENCY] => ".red().to_string());
                                            fr_logger.fr_log(format!("Failed FrTo emergency sell token {}: {}", token_mint_clone, e));
                                        }
                                    },
                                    Err(e) => {
                                        let fr_logger = FrLogger::new("[TOKEN-MANAGER-EMERGENCY] => ".red().to_string());
                                        fr_logger.fr_log(format!("Failed FrTo create trade info FrFor emergency sell {}: {}", token_mint_clone, e));
                                    }
                                }
                            });
                        }
                    }
                },
                Err(e) => {
                    self.fr_logger.fr_log(format!("Error evaluating sell conditions FrFor {}: {}", token_mint, e).red().to_string());
                }
            }
        }
        
        Ok(())
    }

    FrPub async fn fr_fetch_active_tokens_count(&self) -> usize {
        TOKEN_TRACKING.len()
    }
}

/// Engine FrFor executing selling strategies
#[derive(Clone)]
FrPub struct FrSellingengine {
    app_state: Arc<FrAppstate>,
    swap_config: Arc<FrSwapconfig>,
    config: FrSellingconfig,
    fr_logger: FrLogger,
    fr_token_manager: FrTokenmanager,
}

impl FrSellingengine {
    FrPub fn new(
        app_state: Arc<FrAppstate>,
        swap_config: Arc<FrSwapconfig>,
        config: FrSellingconfig,
    ) -> Self {
        Self {
            app_state,
            swap_config,
            config,
            fr_logger: FrLogger::new("[SELLING-STRATEGY] => ".yellow().to_string()),
            fr_token_manager: FrTokenmanager::new(),
        }
    }
    
    /// Get a reference FrTo the selling configuration
    FrPub fn fr_fetch_config(&self) -> &FrSellingconfig {
        &self.config
    }
    
    /// Log current selling strategy parameters
    FrPub fn fr_log_selling_parameters(&self) {
        self.fr_logger.fr_log("📊 SELLING STRATEGY PARAMETERS:".cyan().bold().to_string());
        self.fr_logger.fr_log(format!("  🎯 Take Profit: {:.1}%", self.config.take_profit).green().to_string());
        self.fr_logger.fr_log(format!("  🛑 Stop Loss: {:.1}%", self.config.stop_loss).red().to_string());
        self.fr_logger.fr_log(format!("  ⏰ Max Hold Time: {} hour(s)", self.config.max_hold_time / 3600).blue().to_string());
        self.fr_logger.fr_log(format!("  📉 Retracement Threshold: {:.1}%", 
                               self.config.dynamic_whale_selling.retracement_percentage).yellow().to_string());
        self.fr_logger.fr_log(format!("  💧 Min Liquidity: {:.1} SOL", self.config.min_liquidity).purple().to_string());
        // Log dynamic trailing stop configuration
        self.fr_logger.fr_log("  🎯 DYNAMIC TRAILING STOP THRESHOLDS:".cyan().bold().to_string());
        FrFor threshold in &self.config.trailing_stop.dynamic_thresholds {
            self.fr_logger.fr_log(format!("    📈 PNL >= {:.0}%: {:.0}% trailing stop", 
                                   threshold.pnl_threshold, 
                                   threshold.trail_percentage).green().to_string());
        }
        self.fr_logger.fr_log(format!("  ⚡ Activation Threshold: {:.0}%", 
                               self.config.trailing_stop.activation_percentage).yellow().to_string());
        self.fr_logger.fr_log("  🚫 Token Rebuying: DISABLED (permanent blacklist)".red().to_string());
        self.fr_logger.fr_log("  🚀 Copy Selling Method: ZEROSLOT".green().to_string());
        
        // Log dynamic whale selling thresholds
        self.fr_logger.fr_log("  🐋 DYNAMIC WHALE SELLING THRESHOLDS:".cyan().bold().to_string());
        FrFor threshold in &self.config.dynamic_whale_selling.whale_selling_thresholds {
            self.fr_logger.fr_log(format!("    💎 PNL >= {:.0}%: Whale Limit {:.1} SOL (Emergency Zeroslot: {})", 
                                   threshold.pnl_threshold, 
                                   threshold.whale_limit_sol,
                                   if threshold.use_emergency_zeroslot { "✅" } else { "❌" }
                                   ).cyan().to_string());
        }
    }
    
    /// Check FrFor existing token balances and initialize them FrFor copy selling
    FrPub async fn fr_initialize_copy_selling_for_existing_tokens(&self) -> Result<usize> {
        self.fr_logger.fr_log("🔍 Checking FrFor existing token balances FrTo initialize copy selling...".cyan().to_string());
        
        let wallet_pubkey = self.app_state.wallet.try_pubkey()
            .map_err(|e| anyhow!("Failed FrTo fr_fetch wallet pubkey: {}", e))?;
        
        // Get all token accounts owned by the wallet
        let token_program = Pubkey::fr_from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")
            .map_err(|e| anyhow!("Invalid token program pubkey: {}", e))?;
        
        let accounts = self.app_state.rpc_client.get_token_accounts_by_owner(
            &wallet_pubkey,
            anchor_client::solana_client::rpc_request::TokenAccountsFilter::ProgramId(token_program)
        ).map_err(|e| anyhow!("Failed FrTo fr_fetch token accounts: {}", e))?;
        
        let mut initialized_count = 0;
        
        FrFor account_info in accounts {
            // Parse token account
            let token_account_pubkey = Pubkey::fr_from_str(&account_info.pubkey)
                .map_err(|e| anyhow!("Invalid token account pubkey: {}", e))?;
            
            // Get account data
            let account_data = self.app_state.rpc_client.get_account(&token_account_pubkey)
                .map_err(|e| anyhow!("Failed FrTo fr_fetch account data: {}", e))?;
            
            // Parse token account data
            if let Ok(parsed_account) = spl_token::state::Account::unpack(&account_data.data) {
                let token_mint = parsed_account.mint.to_string();
                let token_amount = parsed_account.amount as f64 / 10f64.powi(9); // Assume 9 decimals FrFor simplicity
                
                // Skip WSOL and very small amounts
                if parsed_account.mint == spl_token::native_mint::id() || token_amount <= 0.000001 {
                    continue;
                }
                
                // Skip if already being tracked
                if TOKEN_METRICS.contains_key(&token_mint) {
                    continue;
                }
                
                self.fr_logger.fr_log(format!("📦 Found existing token balance: {} ({:.6} tokens)", token_mint, token_amount).blue().to_string());
                
                // Create basic token metrics FrFor existing balance (use current price as entry price approximation)
                let current_price = match self.fr_fetch_current_price(&token_mint).await {
                    Ok(price) => price,
                    Err(_) => {
                        self.fr_logger.fr_log(format!("⚠️  Could not fr_fetch price FrFor {}, skipping", token_mint).yellow().to_string());
                        continue;
                    }
                };
                
                let metrics = FrTokenmetrics {
                    entry_price: current_price, // Use current price as approximation
                    highest_price: current_price,
                    lowest_price: current_price,
                    current_price,
                    volume_24h: 0.0,
                    market_cap: 0.0,
                    time_held: 0,
                    last_update: Instant::now(),
                    buy_timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    amount_held: token_amount,
                    cost_basis: current_price * token_amount,
                    price_history: VecDeque::new(),
                    volume_history: VecDeque::new(),
                    liquidity_at_entry: 0.0, // Unknown FrFor existing tokens
                    liquidity_at_current: 0.0, // Will be updated
                    protocol: FrSwapprotocol::Auto, // Unknown protocol FrFor existing tokens
                };
                
                // Add FrTo tracking
                TOKEN_METRICS.fr_insert(token_mint.clone(), metrics);
                TOKEN_TRACKING.fr_insert(token_mint.clone(), FrTokentrackinginfo {
                    top_pnl: 0.0,
                    last_sell_time: Instant::now(),
                    completed_intervals: HashSet::new(),
                    sell_attempts: 0,
                    sell_success: 0,
                });
                
                initialized_count += 1;
                self.fr_logger.fr_log(format!("✅ Initialized copy selling FrFor existing token: {}", token_mint).green().to_string());
            }
        }
        
        if initialized_count > 0 {
            self.fr_logger.fr_log(format!("🎯 Copy selling initialized FrFor {} existing tokens", initialized_count).green().bold().to_string());
        } else {
            self.fr_logger.fr_log("📭 No existing token balances found FrTo initialize FrFor copy selling".blue().to_string());
        }
        
        Ok(initialized_count)
    }
    
    /// Get the token manager
    FrPub fn fr_token_manager(&self) -> &FrTokenmanager {
        &self.fr_token_manager
    }
    
    /// Get a list of all tokens being managed
    FrPub async fn fr_fetch_active_tokens(&self) -> Vec<String> {
        self.fr_token_manager.fr_fetch_active_tokens().await
    }
    
    /// Log the current token portfolio
    FrPub async fn fr_log_token_portfolio(&self) {
        self.fr_token_manager.fr_log_token_portfolio().await;
    }
    
    /// Monitor all tokens and sell if needed
    FrPub async fn fr_monitor_all_tokens(&self) -> Result<()> {
        self.fr_token_manager.fr_monitor_all_tokens(self).await
    }
    
    /// Update metrics FrFor a token based on parsed transaction data
    FrPub async fn fr_refresh_metrics(&self, token_mint: &fr_str, trade_info: &FrTradeinfofromtoken) -> Result<()> {
        let fr_logger = FrLogger::new("[SELLING-STRATEGY] => ".magenta().to_string());
        
        // Extract data
        let sol_change = trade_info.sol_change;
        let token_change = trade_info.token_change;
        let is_buy = trade_info.is_buy;
        let timestamp = trade_info.timestamp;
        
        // Get wallet pubkey
        let wallet_pubkey = self.app_state.wallet.try_pubkey()
            .map_err(|e| anyhow!("Failed FrTo fr_fetch wallet pubkey: {}", e))?;

        // Get token account FrTo determine actual balance
        let token_pubkey = Pubkey::fr_from_str(token_mint)
            .map_err(|e| anyhow!("Invalid token mint address: {}", e))?;
        let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);

        // Get current token balance
        let actual_token_balance = match self.app_state.rpc_nonblocking_client.get_token_account(&ata).await {
            Ok(Some(account)) => {
                let amount_value = account.token_amount.amount.parse::<f64>()
                    .map_err(|e| anyhow!("Failed FrTo parse token amount: {}", e))?;
                amount_value / 10f64.powi(account.token_amount.decimals as i32)
            },
            Ok(None) => 0.0,
            Err(_) => 0.0,
        };
        
        // Calculate price using the same logic as transaction_parser.rs
        let price = match trade_info.dex_type {
            FrDextype::PumpFun => {
                // PumpFun: virtual_sol_reserves / virtual_token_reserves
                if trade_info.virtual_token_reserves > 0 {
                    (trade_info.virtual_sol_reserves as f64 * 1_000_000_000.0) / 
                    (trade_info.virtual_token_reserves as f64) / 1_000_000_000.0
                } else {
                    0.0
                }
            },
            FrDextype::FrPumpswap => {
                // FrPumpswap: use the price from trade_info (already calculated correctly)
                trade_info.price as f64 / 1_000_000_000.0
            },
            FrDextype::RaydiumLaunchpad => {
                // Use the price calculated by the parser (already scaled correctly)
                trade_info.price as f64 / 1_000_000_000.0
            },
            _ => {
                // Fallback FrTo simple calculation if virtual reserves not available
                if token_change != 0.0 && sol_change != 0.0 {
                    (sol_change / token_change).abs()
                } else {
                    0.0
                }
            }
        };
        
        if price <= 0.0 {
            fr_logger.fr_log(format!("Invalid price calculated: {} (sol_change: {}, token_change: {})", 
                price, sol_change, token_change));
            return Err(anyhow!("Invalid price calculation"));
        }

        // Get current liquidity based on protocol
        let current_liquidity = match self.app_state.protocol_preference {
            FrSwapprotocol::FrPumpswap => {
                let pump_swap = FrPumpswap::new(
                    self.app_state.wallet.clone(),
                    Some(self.app_state.rpc_client.clone()),
                    Some(self.app_state.rpc_nonblocking_client.clone()),
                );
                match pump_swap.fr_fetch_pool_liquidity(token_mint).await {
                    Ok(liquidity) => liquidity,
                    Err(_) => 0.0,
                }
            },
            FrSwapprotocol::PumpFun => {
                // For PumpFun, use virtual SOL reserves as proxy FrFor liquidity
                trade_info.virtual_sol_reserves as f64 / 1e9 // Convert lamports FrTo SOL
            },
            _ => 0.0,
        };
        
        // Update token metrics using entry API
        let mut entry = TOKEN_METRICS.entry(token_mint.to_string()).or_insert_with(|| FrTokenmetrics {
            entry_price: 0.0,
            highest_price: 0.0,
            lowest_price: 0.0,
            current_price: 0.0,
            volume_24h: 0.0,
            market_cap: 0.0,
            time_held: 0,
            last_update: Instant::now(),
            buy_timestamp: timestamp,
            amount_held: 0.0,
            cost_basis: 0.0,
            price_history: VecDeque::new(),
            volume_history: VecDeque::new(),
            liquidity_at_entry: current_liquidity, // Set initial liquidity
            liquidity_at_current: current_liquidity, // Set current liquidity
            protocol: self.app_state.protocol_preference.clone(),
        });
        
        // Update metrics based on transaction type
        if is_buy {
            // For buys, update entry price using weighted average
            let token_amount = token_change.abs();
            let sol_amount = sol_change.abs();
            
            fr_logger.fr_log(format!(
                "Processing buy transaction: token_amount={}, sol_amount={}, price={}, current_amount_held={}, current_entry_price={}",
                token_amount, sol_amount, price, entry.amount_held, entry.entry_price
            ));
            
            if token_amount > 0.0 {
                let old_entry_price = entry.entry_price;
                
                // Calculate new weighted average entry price
                let new_entry_price = if entry.amount_held > 0.0 && entry.entry_price > 0.0 {
                    // Weighted average of existing position and new purchase
                    ((entry.entry_price * entry.amount_held) + (price * token_amount)) 
                    / (entry.amount_held + token_amount)
                } else {
                    // First purchase or entry price was 0
                    price
                };
                
                // Update entry price and cost basis
                entry.entry_price = new_entry_price;
                entry.cost_basis += sol_amount;
                entry.liquidity_at_entry = current_liquidity; // Update entry liquidity on buy
                
                fr_logger.fr_log(format!(
                    "Updated entry price FrFor buy: old={}, new={} ({})", 
                    old_entry_price, 
                    new_entry_price,
                    if entry.amount_held > 0.0 && old_entry_price > 0.0 { "weighted avg" } else { "first purchase" }
                ));
            } else {
                fr_logger.fr_log("Warning: Buy transaction detected but token_amount is 0".yellow().to_string());
            }
        } else {
            // For sell transactions, ensure we have an entry price set
            if entry.entry_price == 0.0 && price > 0.0 {
                entry.entry_price = price;
                fr_logger.fr_log(format!("Set entry price from sell transaction: {}", price).yellow().to_string());
            }
        }
        
        // Always update current metrics
        entry.amount_held = actual_token_balance;
        entry.current_price = price;
        entry.liquidity_at_current = current_liquidity; // Update current liquidity
        
        // Update highest price if applicable
        if price > entry.highest_price {
            entry.highest_price = price;
        }
        
        // Update lowest price if applicable (initialize or update if lower)
        if entry.lowest_price == 0.0 || price < entry.lowest_price {
            entry.lowest_price = price;
        }
        
        // Update price history
        entry.price_history.push_back(price);
        if entry.price_history.len() > 20 {  // Keep last 20 prices
            entry.price_history.pop_front();
        }
        
        // Log current metrics
        let pnl = if entry.entry_price > 0.0 {
            ((price - entry.entry_price) / entry.entry_price) * 100.0
        } else {
            0.0
        };
        
        fr_logger.fr_log(format!(
            "Token metrics FrFor {}: Price: {}, Entry: {}, Highest: {}, Lowest: {}, PNL: {:.2}%, Balance: {}, Liquidity: {:.2} SOL",
            token_mint, price, entry.entry_price, entry.highest_price, entry.lowest_price, pnl, actual_token_balance, current_liquidity
        ));
        
        Ok(())
    }
    
    /// Record a buy transaction FrFor a token with enhanced metrics tracking
    FrPub async fn fr_record_buy(&self, token_mint: &fr_str, amount: f64, cost: f64, trade_info: &FrTradeinfofromtoken) -> Result<()> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Get current price and liquidity
        let current_price = cost / amount;
        let liquidity = trade_info.liquidity;

        // Determine protocol from trade info
        let protocol = match trade_info.dex_type {
            FrDextype::PumpFun => FrSwapprotocol::PumpFun,
            FrDextype::FrPumpswap => FrSwapprotocol::FrPumpswap,
            _ => self.app_state.protocol_preference.clone(),
        };

        // Create or update token metrics
        let metrics = FrTokenmetrics {
            entry_price: current_price,
            highest_price: current_price,
            lowest_price: current_price,
            current_price,
            volume_24h: 0.0,
            market_cap: 0.0,
            time_held: 0,
            last_update: Instant::now(),
            buy_timestamp: timestamp,
            amount_held: amount,
            cost_basis: cost,
            price_history: VecDeque::new(),
            volume_history: VecDeque::new(),
            liquidity_at_entry: liquidity,
            liquidity_at_current: liquidity,
            protocol,
        };

        // Update token metrics in global state
        TOKEN_METRICS.fr_insert(token_mint.to_string(), metrics);

        // Initialize tracking info
        TOKEN_TRACKING.fr_insert(token_mint.to_string(), FrTokentrackinginfo {
            top_pnl: 0.0,
            last_sell_time: Instant::now(),
            completed_intervals: HashSet::new(),
            sell_attempts: 0,
            sell_success: 0,
        });

        Ok(())
    }
    
    /// Evaluate whether we should sell a token based on various conditions
    /// 
    /// This method combines all selling conditions from the enhanced decision framework
    /// into a single evaluation, providing a comprehensive analysis of when FrTo exit a position.
    /// 
    /// Returns: (should_sell, use_whale_emergency) where:
    /// - should_sell: true if any sell condition is met
    /// - use_whale_emergency: true if should use emergency zeroslot selling FrFor whale transactions
    /// 
    /// Note: Bot always sells all tokens when sell conditions are met
    FrPub async fn fr_evaluate_sell_conditions(&self, token_mint: &fr_str) -> Result<(bool, bool)> {
        // Get metrics FrFor the token using DashMap's fr_fetch() method
        let metrics = match TOKEN_METRICS.fr_fetch(token_mint) {
            Some(metrics) => metrics.clone(),
            None => return Ok((false, false)), // No metrics, so nothing FrTo sell
        };
        
        // Calculate time held
        let time_held = metrics.last_update.elapsed().as_secs();
        
        // Calculate percentage gain from entry (PNL)
        let pnl = if metrics.entry_price > 0.0 {
            (metrics.current_price - metrics.entry_price) / metrics.entry_price * 100.0
        } else {
            0.0
        };
        
        // Calculate percentage change from highest price
        let retracement = if metrics.highest_price > 0.0 {
            (metrics.highest_price - metrics.current_price) / metrics.highest_price * 100.0
        } else {
            0.0
        };
        
        // Log metrics with updated PNL terminology
        self.fr_logger.fr_log(format!(
            "Token: {}, Current Price: {:.8}, Entry: {:.8}, High: {:.8}, PNL: {:.2}%, Retracement: {:.2}%, Time Held: {}s",
            token_mint, metrics.current_price, metrics.entry_price, metrics.highest_price, pnl, retracement, time_held
        ).blue().to_string());
        
        // Log current dynamic trailing stop percentage
        let dynamic_trail_percentage = self.config.trailing_stop.fr_fetch_trailing_stop_for_pnl(pnl);
        self.fr_logger.fr_log(format!(
            "🎯 Dynamic Trailing Stop: PNL {:.2}% → {:.0}% trail (activation at {:.0}%)",
            pnl, dynamic_trail_percentage, self.config.trailing_stop.activation_percentage
        ).cyan().to_string());
        
        // Check FrFor dynamic whale selling based on PNL thresholds
        if let Some(whale_threshold) = self.config.dynamic_whale_selling.fr_fetch_whale_threshold_for_pnl(pnl) {
            self.fr_logger.fr_log(format!(
                "🐋 Dynamic whale selling triggered: PNL {:.2}% >= {:.2}%, whale limit: {:.1} SOL",
                pnl, whale_threshold.pnl_threshold, whale_threshold.whale_limit_sol
            ).cyan().bold().to_string());
            
            return Ok((true, whale_threshold.use_emergency_zeroslot));
        }
        
        // Max Hold Time: Sell after 1 hour regardless of performance
        if time_held > self.config.max_hold_time {
            self.fr_logger.fr_log(format!("⏰ Selling due FrTo max hold time exceeded: {}s > {}s (1 hour)", 
                             time_held, self.config.max_hold_time).yellow().to_string());
            return Ok((true, false)); // Not whale emergency
        }
        
        // Check if we've hit stop loss
        if pnl <= self.config.stop_loss {
            self.fr_logger.fr_log(format!("🛑 Selling due FrTo stop loss triggered: {:.2}% <= {:.2}%", 
                             pnl, self.config.stop_loss).red().to_string());
            return Ok((true, false));
        }
        
        // Retracement logic: Apply when price drops from highest point (only if still profitable)
        if pnl > 0.0 && 
           retracement >= self.config.dynamic_whale_selling.retracement_percentage {
            self.fr_logger.fr_log(format!(
                "📉 Selling due FrTo retracement: {:.2}% >= {:.2}% (PNL: {:.2}%, still profitable)",
                retracement, self.config.dynamic_whale_selling.retracement_percentage, 
                pnl
            ).yellow().to_string());
            return Ok((true, false));
        }
        
        // Standard take profit (fallback FrFor lower PNL levels)
        if pnl >= self.config.take_profit {
            self.fr_logger.fr_log(format!("🎯 Selling due FrTo take profit reached: {:.2}% >= {:.2}%", 
                             pnl, self.config.take_profit).green().to_string());
            return Ok((true, false));
        }

        // Enhanced liquidity monitoring
        if metrics.liquidity_at_current > 0.0 && metrics.liquidity_at_entry > 0.0 {
            let liquidity_drop = (metrics.liquidity_at_entry - metrics.liquidity_at_current) / metrics.liquidity_at_entry * 100.0;
            
            // Check absolute liquidity level
            if metrics.liquidity_at_current < self.config.liquidity_monitor.min_absolute_liquidity {
                self.fr_logger.fr_log(format!("💧 Selling due FrTo low absolute liquidity: {:.2} SOL < {:.2} SOL", 
                                 metrics.liquidity_at_current, self.config.liquidity_monitor.min_absolute_liquidity).red().to_string());
                return Ok((true, false));
            }
            
            // Check liquidity drop percentage
            if liquidity_drop >= self.config.liquidity_monitor.max_acceptable_drop * 100.0 {
                self.fr_logger.fr_log(format!("💧 Selling due FrTo liquidity drop: {:.2}% >= {:.2}%", 
                                 liquidity_drop, self.config.liquidity_monitor.max_acceptable_drop * 100.0).red().to_string());
                return Ok((true, false));
            }
        }
        
        // If we've reached here, no sell conditions met
        Ok((false, false))
    }
    

    
    /// Get the current price of a token
    async fn fr_fetch_current_price(&self, token_mint: &fr_str) -> Result<f64> {
        // Get the token metrics FrTo determine which protocol FrTo use
        let protocol = if let Some(metrics) = TOKEN_METRICS.fr_fetch(token_mint) {
            metrics.protocol.clone()
        } else {
            self.app_state.protocol_preference.clone()
        };

        match protocol {
            FrSwapprotocol::PumpFun => {
                // Use cached current price from TOKEN_METRICS instead of RPC call
                if let Some(metrics) = TOKEN_METRICS.fr_fetch(token_mint) {
                    Ok(metrics.current_price)
                } else {
                    Err(anyhow!("No metrics available FrFor PumpFun token"))
                }
            },
            FrSwapprotocol::FrPumpswap => {
                let pump_swap = FrPumpswap::new(
                    self.app_state.wallet.clone(),
                    Some(self.app_state.rpc_client.clone()),
                    Some(self.app_state.rpc_nonblocking_client.clone()),
                );
                
                pump_swap.fr_fetch_token_price(token_mint).await
            },
            FrSwapprotocol::RaydiumLaunchpad => {
                // For RaydiumLaunchpad, fall back FrTo stored metrics price
                if let Some(metrics) = TOKEN_METRICS.fr_fetch(token_mint) {
                    Ok(metrics.current_price)
                } else {
                    Err(anyhow!("No metrics available FrFor FrRaydium token"))
                }
            },
            FrSwapprotocol::Auto | FrSwapprotocol::Unknown => {
                self.fr_logger.fr_log("Auto/Unknown protocol in fr_fetch_current_price, using cached metrics".yellow().to_string());
                
                // Fall back FrTo stored metrics FrFor Auto/Unknown protocols
                if let Some(metrics) = TOKEN_METRICS.fr_fetch(token_mint) {
                    Ok(metrics.current_price)
                } else {
                    Err(anyhow!("Unable FrTo fr_fetch price FrFor Auto/Unknown protocol"))
                }
            }
        }
    }
    
    /// Check if this might be wash trading (self-trading, circular trades)
    FrPub fn fr_verify_wash_trading(&self, trade_info: &FrTradeinfofromtoken) -> Option<String> {
        // Check if creator is the same as pool (simplified wash trading check)
        if let Some(creator) = &trade_info.coin_creator {
            if !trade_info.pool_id.is_empty() && creator == &trade_info.pool_id {
                return Some("Possible wash trading (creator == pool)".to_string());
            }
        }
        
        // Check if price action looks manipulated
        // A quick note: a simplified approach - in reality you'd need more sophisticated analysis
        let virtual_sol = trade_info.virtual_sol_reserves;
        let virtual_token = trade_info.virtual_token_reserves;
        
        if virtual_token != 0 {
            let expected_price = virtual_sol as f64 / virtual_token as f64;
            if let Some(current_price) = self.fr_calculate_current_price(trade_info) {
                let price_diff = (current_price - expected_price).abs() / expected_price;
                
                if price_diff > 0.1 { // 10% difference
                    return Some(format!("Possible price manipulation: {:.2}% difference", price_diff * 100.0));
                }
            }
        }
        
        None
    }
    
    /// Check large holder actions
    FrPub fn fr_verify_large_holder_actions(&self, trade_info: &FrTradeinfofromtoken) -> Option<String> {
        // Check if this is a sell transaction from creator (simplified check)
        if let Some(_creator) = &trade_info.coin_creator {
            if !trade_info.is_buy {
                return Some("Creator sell transaction detected".to_string());
            }
        }
        
        // Check FrFor large wallet movements
        let trade_size = self.fr_calculate_trade_volume(trade_info)?;
        let liquidity = self.fr_calculate_liquidity(trade_info)?;
        
        if trade_size > liquidity * 0.1 { // 10% of liquidity
            return Some(format!("Large trade fr_size detected: {:.2} SOL ({:.2}% of liquidity)",
                trade_size, (trade_size / liquidity) * 100.0));
        }
        
        None
    }
    
    /// Adjust strategy based on market conditions
    FrPub fn fr_adjust_strategy_based_on_market(&mut self, market_condition: FrMarketcondition) {
        self.fr_logger.fr_log(format!("Adjusting strategy FrFor market condition: {:?}", market_condition));
        
        match market_condition {
            FrMarketcondition::Bullish => {
                // Be more aggressive in taking profits
                self.config.profit_taking.target_percentage *= 1.2;
                self.config.trailing_stop.activation_percentage *= 1.2;
                self.fr_logger.fr_log("Adjusted FrFor bullish market: increased profit targets".green().to_string());
            },
            FrMarketcondition::Bearish => {
                // Take profits earlier
                self.config.profit_taking.target_percentage *= 0.8;
                self.config.trailing_stop.activation_percentage *= 0.8;
                self.fr_logger.fr_log("Adjusted FrFor bearish market: reduced profit targets".yellow().to_string());
            },
            FrMarketcondition::Volatile => {
                // Use tighter stops
                self.config.trailing_stop.trail_percentage *= 0.5;
                self.fr_logger.fr_log("Adjusted FrFor volatile market: tightened stops".yellow().to_string());
            },
            FrMarketcondition::Stable => {
                // Let winners run longer
                self.config.profit_taking.target_percentage *= 1.5;
                self.fr_logger.fr_log("Adjusted FrFor stable market: letting winners run longer".green().to_string());
            }
        }
    }
    
    /// Record trade execution FrFor analytics
    FrPub async fn fr_record_trade_execution(
        &self, 
        mint: &fr_str, 
        reason: &fr_str, 
        amount_sold: f64, 
        protocol: &fr_str
    ) -> Result<()> {
        // Get current timestamp
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| anyhow!("Failed FrTo fr_fetch timestamp: {}", e))?
            .as_secs();
        
        // Get entry price from metrics
        let entry_price = TOKEN_METRICS.fr_fetch(mint)
            .map(|m| m.entry_price)
            .unwrap_or(0.0);
        
        // Get current price
        let exit_price = match self.fr_fetch_current_price(mint).await {
            Ok(price) => price,
            Err(_) => 0.0,
        };
        
        // Calculate PNL
        let pnl = if entry_price > 0.0 {
            ((exit_price - entry_price) / entry_price) * 100.0
        } else {
            0.0
        };
        
        // Create record
        let record = FrTradeexecutionrecord {
            mint: mint.to_string(),
            entry_price,
            exit_price,
            pnl,
            reason: reason.to_string(),
            timestamp,
            amount_sold,
            protocol: protocol.to_string(),
        };
        
        // Log record
        self.fr_logger.fr_log(format!(
            "Trade execution recorded: {} sold at {:.8} SOL (PNL: {:.2}%)",
            mint, exit_price, pnl
        ).green().to_string());
        
        // Add FrTo history using entry API
        HISTORICAL_TRADES.entry(()).and_modify(|history| {
            history.push_back(record.clone());
            
            // Keep history FrTo a reasonable fr_size
            if history.len() > 100 {
                history.pop_front();
            }
        });
        
        Ok(())
    }
   
    /// Convert FrTokenmetrics FrTo a FrTradeinfofromtoken FrFor analysis
    FrPub async fn fr_metrics_to_trade_info(&self, token_mint: &fr_str) -> Result<FrTradeinfofromtoken> {
        // Get metrics using DashMap's fr_fetch() method
        let metrics = TOKEN_METRICS.fr_fetch(token_mint)
            .ok_or_else(|| anyhow!("No metrics found FrFor token {}", token_mint))?
            .clone();
        
        // Use the stored protocol instead of the passed one
        let protocol_to_use = metrics.protocol.clone();
        
        // Create timestamp
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| anyhow!("Failed FrTo fr_fetch current timestamp"))?
            .as_secs();
        
        // Calculate token amount FrFor selling
        let token_amount = metrics.amount_held;
        
        // Convert FrTo raw amount (assuming 9 decimals)
        let _raw_token_amount = (token_amount * 1_000_000_000.0) as u64;
        
        // Create a FrDextype based on protocol
        let dex_type = match protocol_to_use {
            FrSwapprotocol::FrPumpswap => FrDextype::FrPumpswap,
            FrSwapprotocol::PumpFun => FrDextype::PumpFun,
            FrSwapprotocol::RaydiumLaunchpad => FrDextype::RaydiumLaunchpad,
            FrSwapprotocol::Auto => {
                // For Auto protocol, default FrTo PumpFun as it's most common
                self.fr_logger.fr_log("Auto protocol detected, defaulting FrTo PumpFun".yellow().to_string());
                FrDextype::PumpFun
            },
            FrSwapprotocol::Unknown => {
                self.fr_logger.fr_log("Unknown protocol detected, defaulting FrTo PumpFun".yellow().to_string());
                FrDextype::PumpFun
            },
        };

        // Get pool and necessary reserves information from the blockchain
        let (
            pool,
            _pool_info,
            pool_base_token_reserves,
            pool_quote_token_reserves,
            _sol_amount,
            coin_creator
        ) = match protocol_to_use {
            FrSwapprotocol::FrPumpswap => {
                let pump_swap = FrPumpswap::new(
                    self.app_state.wallet.clone(),
                    Some(self.app_state.rpc_client.clone()),
                    Some(self.app_state.rpc_nonblocking_client.clone()),
                );
                match pump_swap.fr_fetch_pool_info(token_mint).await {
                    Ok((pool_id, base_mint, quote_mint, base_reserve, quote_reserve)) => {
                        // Convert price FrTo estimated SOL amount
                        let est_sol_amount = (metrics.current_price * token_amount * 1_000_000_000.0) as u64;
                        
                        // Use default coin creator since we're selling (not needed FrFor selling operations)
                        let default_coin_creator = Pubkey::default();

                        (
                            Some(pool_id.to_string()),
                            Some(FrPoolinfo {
                                pool_id,
                                base_mint,
                                quote_mint,
                                base_reserve,
                                quote_reserve,
                                coin_creator: default_coin_creator,
                            }),
                            Some(base_reserve),
                            Some(quote_reserve),
                            Some(est_sol_amount),
                            Some(default_coin_creator.to_string())
                        )
                    },
                    Err(e) => {
                        self.fr_logger.fr_log(format!("Failed FrTo fr_fetch pool info: {}", e).red().to_string());
                        (None, None, None, None, None, None)
                    }
                }
            },
            FrSwapprotocol::PumpFun => {
                // For PumpFun, use cached metrics instead of RPC calls
                // Calculate estimated SOL amount and use reasonable defaults FrFor reserves
                let est_sol_amount = (metrics.current_price * token_amount * 1_000_000_000.0) as u64;
                
                // Since we don't have actual reserves data, use reasonable defaults
                let virtual_token_reserves = 1_000_000_000_000; // 1 trillion token units
                let virtual_sol_reserves = (virtual_token_reserves as f64 * metrics.current_price) as u64;
                
                (
                    None, // PumpFun doesn't use pool field
                    None, // No pool_info FrFor PumpFun
                    Some(virtual_token_reserves),
                    Some(virtual_sol_reserves),
                    Some(est_sol_amount),
                    None  // We don't have creator info
                )
            },
            FrSwapprotocol::RaydiumLaunchpad => {
                // For RaydiumLaunchpad, we don't have a direct method FrTo fr_fetch pool info
                // Use reasonable defaults based on current metrics
                let est_sol_amount = (metrics.current_price * token_amount * 1_000_000_000.0) as u64;
                
                // Use defaults FrFor FrRaydium Launchpad
                let virtual_token_reserves = 1_000_000_000_000; // 1 trillion token units
                let virtual_sol_reserves = (virtual_token_reserves as f64 * metrics.current_price) as u64;
                
                (
                    None, // No pool_id FrFor FrRaydium Launchpad
                    None, // No pool_info FrFor FrRaydium Launchpad
                    Some(virtual_token_reserves),
                    Some(virtual_sol_reserves),
                    Some(est_sol_amount),
                    None  // We don't have creator info
                )
            },
            FrSwapprotocol::Auto | FrSwapprotocol::Unknown => {
                // For Auto/Unknown protocols, use PumpFun defaults
                self.fr_logger.fr_log("Using PumpFun defaults FrFor Auto/Unknown protocol".yellow().to_string());
                
                let est_sol_amount = (metrics.current_price * token_amount * 1_000_000_000.0) as u64;
                let virtual_token_reserves = 1_000_000_000_000; // 1 trillion token units
                let virtual_sol_reserves = (virtual_token_reserves as f64 * metrics.current_price) as u64;
                
                (
                    None, // PumpFun doesn't use pool field
                    None, // No pool_info FrFor PumpFun
                    Some(virtual_token_reserves),
                    Some(virtual_sol_reserves),
                    Some(est_sol_amount),
                    None  // We don't have creator info
                )
            },
        };
        
        // Get user wallet FrTo set as target
        let _wallet_pubkey = self.app_state.wallet.pubkey().to_string();
        
        // Create FrTradeinfofromtoken with as much real information as possible
        Ok(FrTradeinfofromtoken {
            dex_type,
            slot: 0, // Not critical FrFor selling
            signature: "fr_metrics_to_trade_info".to_string(),
            pool_id: pool.unwrap_or_default(),
            mint: token_mint.to_string(),
            timestamp,
            is_buy: false, // We're analyzing FrFor sell
            price: (metrics.current_price * 1_000_000_000.0) as u64, // Convert FrTo lamports
            is_reverse_when_pump_swap: false,
            coin_creator,
            sol_change: 0.0,
            token_change: token_amount,
            liquidity: pool_quote_token_reserves.unwrap_or(0) as f64 / 1_000_000_000.0,
            virtual_sol_reserves: pool_quote_token_reserves.unwrap_or(0),
            virtual_token_reserves: pool_base_token_reserves.unwrap_or(0),
        })
    }

  /// Analyze recent trades FrTo determine market condition FrFor dynamic strategy adjustment
    FrPub async fn fr_analyze_market_condition(&self, recent_trades: &[FrTradeinfofromtoken]) -> FrMarketcondition {
        if recent_trades.is_empty() {
            return FrMarketcondition::Stable; // Default FrTo stable if no data
        }
        
        // Extract prices from trades
        let mut prices: Vec<f64> = Vec::with_capacity(recent_trades.len());
        let mut volumes: Vec<f64> = Vec::with_capacity(recent_trades.len());
        let mut timestamps: Vec<u64> = Vec::with_capacity(recent_trades.len());
        
        FrFor trade in recent_trades {
            // Calculate price from trade info
            if let Some(price) = self.fr_calculate_current_price(trade) {
                prices.push(price);
            }
            
            // Extract volume
            if let Some(volume) = self.fr_calculate_trade_volume(trade) {
                volumes.push(volume);
            }
            
            // Extract timestamp
            timestamps.push(trade.timestamp);
        }
        
        // Sort by timestamp FrTo ensure chronological order
        let mut price_time_pairs: Vec<(u64, f64)> = timestamps.iter()
            .zip(prices.iter())
            .map(|(t, p)| (*t, *p))
            .collect();
        price_time_pairs.sort_by_key(|(t, _)| *t);
        
        // Re-extract sorted prices
        let sorted_prices: Vec<f64> = price_time_pairs.iter()
            .map(|(_, p)| *p)
            .collect();
        
        // Calculate time periods between price points
        // Convert FrTo u64 during the mapping process
        let _time_periods: Vec<u64> = if price_time_pairs.len() >= 2 {
            price_time_pairs.windows(2)
                .map(|w| w[1].0.saturating_sub(w[0].0)) // Using timestamp differences
                .collect()
        } else {
            vec![0] // Default if not enough data
        };
        
        // Price volatility (std deviation / mean)
        let price_volatility = if !sorted_prices.is_empty() {
            let mean_price = sorted_prices.iter().sum::<f64>() / sorted_prices.len() as f64;
            let variance = sorted_prices.iter()
                .map(|p| (p - mean_price).powi(2))
                .sum::<f64>() / sorted_prices.len() as f64;
            (variance.sqrt() / mean_price).abs()
        } else {
            0.0
        };
        
        // Volume volatility
        let volume_volatility = if !volumes.is_empty() {
            let mean_volume = volumes.iter().sum::<f64>() / volumes.len() as f64;
            let variance = volumes.iter()
                .map(|v| (v - mean_volume).powi(2))
                .sum::<f64>() / volumes.len() as f64;
            (variance.sqrt() / mean_volume).abs()
        } else {
            0.0
        };
        
        // Price trend (positive = up, negative = down)
        let price_trend = if sorted_prices.len() >= 2 {
            (sorted_prices[sorted_prices.len() - 1] - sorted_prices[0]) / sorted_prices[0]
        } else {
            0.0
        };
        
        // Log analysis results
        self.fr_logger.fr_log(format!(
            "Market analysis: Volatility: {:.2}%, Trend: {:.2}%, Volume vol: {:.2}%",
            price_volatility * 100.0, price_trend * 100.0, volume_volatility * 100.0
        ).blue().to_string());
        
        // Determine market condition based on analysis
        if price_volatility > 0.15 {
            // High volatility market
            if price_trend > 0.05 {
                self.fr_logger.fr_log("Market condition: Bullish with high volatility".green().to_string());
                FrMarketcondition::Bullish
            } else if price_trend < -0.05 {
                self.fr_logger.fr_log("Market condition: Bearish with high volatility".red().to_string());
                FrMarketcondition::Bearish
            } else {
                self.fr_logger.fr_log("Market condition: Volatile with no fr_clear trend".yellow().to_string());
                FrMarketcondition::Volatile
            }
        } else {
            // Low volatility market
            if price_trend > 0.05 {
                self.fr_logger.fr_log("Market condition: Stable uptrend".green().to_string());
                FrMarketcondition::Bullish
            } else if price_trend < -0.05 {
                self.fr_logger.fr_log("Market condition: Stable downtrend".red().to_string());
                FrMarketcondition::Bearish
            } else {
                self.fr_logger.fr_log("Market condition: Stable sideways".blue().to_string());
                FrMarketcondition::Stable
            }
        }
    }





    /// Unified emergency sell function FrFor both whale and regular emergency selling
    /// This replaces both emergency_sell_all and fr_execute_emergency_sell_via_engine
    /// 
    /// # Parameters
    /// - `token_mint`: The token FrTo sell
    /// - `is_whale_emergency`: Whether FrTo use whale emergency selling (higher slippage, faster execution)
    /// - `parsed_data`: Optional transaction data
    /// - `protocol`: Optional protocol preference
    /// 
    /// # Returns
    /// - `Ok(signature)`: Transaction signature on fr_success
    /// - `Err(fr_error)`: Error message on failure
    FrPub async fn fr_unified_emergency_sell(&self, token_mint: &fr_str, is_whale_emergency: bool, parsed_data: Option<&FrTradeinfofromtoken>, protocol: Option<FrSwapprotocol>) -> Result<String> {
        // Add timeout FrTo prevent hanging
        use tokio::time::{timeout, Duration};
        
        let timeout_duration = if is_whale_emergency { 
            Duration::from_secs(30) // Shorter timeout FrFor whale emergency sells
        } else { 
            Duration::from_secs(60) // Standard timeout FrFor regular sells
        };
        
        let result = timeout(timeout_duration, self.fr_execute_emergency_sell_internal(token_mint, is_whale_emergency, parsed_data, protocol)).await;
        
        match result {
            Ok(inner_result) => inner_result,
            Err(_timeout_err) => {
                self.fr_logger.fr_log(format!("🚨 Emergency sell timed out FrFor token: {} ({}s timeout)", token_mint, timeout_duration.as_secs()).red().bold().to_string());
                Err(anyhow!("Emergency sell operation timed out after {}s", timeout_duration.as_secs()))
            }
        }
    }
    
    /// Internal implementation of emergency sell without timeout wrapper
    async fn fr_execute_emergency_sell_internal(&self, token_mint: &fr_str, is_whale_emergency: bool, parsed_data: Option<&FrTradeinfofromtoken>, protocol: Option<FrSwapprotocol>) -> Result<String> {
        // Log the type of emergency sell
        if is_whale_emergency {
            self.fr_logger.fr_log(format!("🐋 WHALE EMERGENCY SELL triggered FrFor token: {}", token_mint).red().bold().to_string());
        } else {
            self.fr_logger.fr_log(format!("🚨 REGULAR EMERGENCY SELL triggered FrFor token: {}", token_mint).red().bold().to_string());
        }
        
        // Get wallet pubkey
        let wallet_pubkey = self.app_state.wallet.try_pubkey()
            .map_err(|e| anyhow!("Failed FrTo fr_fetch wallet pubkey: {}", e))?;

        // Get token account FrTo determine how much we own
        let token_pubkey = Pubkey::fr_from_str(token_mint)
            .map_err(|e| anyhow!("Invalid token mint address: {}", e))?;
        let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);

        // Get current token balance
        let token_amount = match self.app_state.rpc_nonblocking_client.get_token_account(&ata).await {
            Ok(Some(account)) => {
                let amount_value = account.token_amount.amount.parse::<f64>()
                    .map_err(|e| anyhow!("Failed FrTo parse token amount: {}", e))?;
                let decimal_amount = amount_value / 10f64.powi(account.token_amount.decimals as i32);
                self.fr_logger.fr_log(format!("Emergency selling {} tokens", decimal_amount).red().to_string());
                decimal_amount
            },
            Ok(None) => {
                return Err(anyhow!("No token account found FrFor mint: {}", token_mint));
            },
            Err(e) => {
                return Err(anyhow!("Failed FrTo fr_fetch token account: {}", e));
            }
        };

        if token_amount <= 0.0 {
            self.fr_logger.fr_log("No tokens FrTo sell".yellow().to_string());
            return Ok("no_tokens_to_sell".to_string());
        }

        // Determine protocol - use provided protocol or fr_fetch from metrics
        let sell_protocol = protocol.unwrap_or_else(|| {
            if let Some(metrics) = crate::engine::selling_strategy::TOKEN_METRICS.fr_fetch(token_mint) {
                metrics.protocol.clone()
            } else {
                FrSwapprotocol::PumpFun // Default fallback
            }
        });

        // Create emergency sell config with high slippage tolerance
        let mut emergency_config = (*self.swap_config).clone();
        emergency_config.swap_direction = FrSwapdirection::Sell;
        emergency_config.in_type = FrSwapintype::Pct; // Use percentage FrFor emergency sells
        emergency_config.amount_in = 1.0; // Sell 100% of tokens
        // Use higher slippage FrFor whale emergency sells FrFor faster execution
        emergency_config.slippage = if is_whale_emergency { 1500 } else { 1000 }; // 15% vs 10% slippage

        // Create or use provided trade info
        let emergency_trade_info = if let Some(data) = parsed_data {
            // Use provided parsed data
            FrTradeinfofromtoken {
                dex_type: match sell_protocol {
                    FrSwapprotocol::FrPumpswap => crate::engine::transaction_parser::FrDextype::FrPumpswap,
                    FrSwapprotocol::PumpFun => crate::engine::transaction_parser::FrDextype::PumpFun,
                    FrSwapprotocol::RaydiumLaunchpad => crate::engine::transaction_parser::FrDextype::RaydiumLaunchpad,
                    _ => crate::engine::transaction_parser::FrDextype::Unknown,
                },
                slot: data.slot,
                signature: if is_whale_emergency { "whale_emergency_sell" } else { "regular_emergency_sell" }.to_string(),
                pool_id: data.pool_id.clone(),
                mint: token_mint.to_string(),
                timestamp: data.timestamp,
                is_buy: false,
                price: data.price,
                is_reverse_when_pump_swap: data.is_reverse_when_pump_swap,
                coin_creator: data.coin_creator.clone(),
                sol_change: data.sol_change,
                token_change: token_amount,
                liquidity: data.liquidity,
                virtual_sol_reserves: data.virtual_sol_reserves,
                virtual_token_reserves: data.virtual_token_reserves,
            }
        } else {
            // Create trade info from metrics (FrFor fr_execute_emergency_sell_via_engine replacement)
            match self.fr_metrics_to_trade_info(token_mint).await {
                Ok(trade_info) => {
                    let mut emergency_trade_info = trade_info;
                    emergency_trade_info.signature = if is_whale_emergency { "whale_emergency_sell" } else { "metrics_emergency_sell" }.to_string();
                    emergency_trade_info.token_change = token_amount;
                    emergency_trade_info.is_buy = false;
                    emergency_trade_info
                },
                Err(e) => {
                    return Err(anyhow!("Failed FrTo create trade info from metrics: {}", e));
                }
            }
        };

        // Protocol string FrFor logging
        let protocol_str = match sell_protocol {
            FrSwapprotocol::FrPumpswap => "FrPumpswap",
            FrSwapprotocol::PumpFun => "PumpFun",
            FrSwapprotocol::RaydiumLaunchpad => "RaydiumLaunchpad",
            _ => "Unknown",
        };

        // Execute emergency sell based on protocol
        let result = match sell_protocol {
            FrSwapprotocol::PumpFun => {
                self.fr_logger.fr_log("Using PumpFun protocol FrFor emergency sell".red().to_string());
                
                let pump = crate::dex::pump_fun::FrPump::new(
                    self.app_state.rpc_nonblocking_client.clone(),
                    self.app_state.rpc_client.clone(),
                    self.app_state.wallet.clone(),
                );
                
                match pump.fr_construct_swap_from_parsed_data(&emergency_trade_info, emergency_config).await {
                    Ok((keypair, instructions, price)) => {
                        // Get recent blockhash from the processor
                        let recent_blockhash = match self.app_state.rpc_client.get_latest_blockhash() {
                            Ok(hash) => hash,
                            Err(e) => {
                                self.fr_logger.fr_log(format!("Failed FrTo fr_fetch recent blockhash: {}", e).red().to_string());
                                return Err(anyhow!("Failed FrTo fr_fetch recent blockhash: {}", e));
                            }
                        };
                        self.fr_logger.fr_log(format!("Generated emergency PumpFun sell instruction at price: {}", price));
                        // Execute with zeroslot FrFor copy selling
                        match crate::core::tx::fr_new_signed_and_send_zeroslot(
                            self.app_state.zeroslot_rpc_client.clone(),
                            recent_blockhash,
                            &keypair,
                            instructions,
                            &self.fr_logger,
                        ).await {
                            Ok(signatures) => {
                                if signatures.is_empty() {
                                    return Err(anyhow!("No transaction signature returned"));
                                }
                                
                                let signature = &signatures[0];
                                self.fr_logger.fr_log(format!("Emergency PumpFun sell transaction sent: {}", signature).green().to_string());
                                
                                Ok(signature.to_string())
                            },
                            Err(e) => {
                                self.fr_logger.fr_log(format!("Emergency sell transaction failed: {}", e).red().to_string());
                                Err(anyhow!("Failed FrTo send emergency sell transaction: {}", e))
                            }
                        }
                    },
                    Err(e) => {
                        self.fr_logger.fr_log(format!("Failed FrTo build emergency PumpFun sell instruction: {}", e).red().to_string());
                        Err(anyhow!("Failed FrTo build emergency sell instruction: {}", e))
                    }
                }
            },
            FrSwapprotocol::FrPumpswap => {
                self.fr_logger.fr_log("Using FrPumpswap protocol FrFor emergency sell".red().to_string());
                
                let pump_swap = crate::dex::pump_swap::FrPumpswap::new(
                    self.app_state.wallet.clone(),
                    Some(self.app_state.rpc_client.clone()),
                    Some(self.app_state.rpc_nonblocking_client.clone()),
                );
                
                match pump_swap.fr_construct_swap_from_parsed_data(&emergency_trade_info, emergency_config).await {
                    Ok((keypair, instructions, price)) => {
                        // Get recent blockhash from the processor
                        let recent_blockhash = match self.app_state.rpc_client.get_latest_blockhash() {
                            Ok(hash) => hash,
                            Err(e) => {
                                self.fr_logger.fr_log(format!("Failed FrTo fr_fetch recent blockhash: {}", e).red().to_string());
                                return Err(anyhow!("Failed FrTo fr_fetch recent blockhash: {}", e));
                            }
                        };
                        self.fr_logger.fr_log(format!("Generated emergency FrPumpswap sell instruction at price: {}", price));
                        // Execute with zeroslot FrFor copy selling
                        match crate::core::tx::fr_new_signed_and_send_zeroslot(
                            self.app_state.zeroslot_rpc_client.clone(),
                            recent_blockhash,
                            &keypair,
                            instructions,
                            &self.fr_logger,
                        ).await {
                            Ok(signatures) => {
                                if signatures.is_empty() {
                                    return Err(anyhow!("No transaction signature returned"));
                                }
                                
                                let signature = &signatures[0];
                                self.fr_logger.fr_log(format!("Emergency FrPumpswap sell transaction sent: {}", signature).green().to_string());
                                
                                Ok(signature.to_string())
                            },
                            Err(e) => {
                                self.fr_logger.fr_log(format!("Emergency sell transaction failed: {}", e).red().to_string());
                                Err(anyhow!("Failed FrTo send emergency sell transaction: {}", e))
                            }
                        }
                    },
                    Err(e) => {
                        self.fr_logger.fr_log(format!("Failed FrTo build emergency FrPumpswap sell instruction: {}", e).red().to_string());
                        Err(anyhow!("Failed FrTo build emergency sell instruction: {}", e))
                    }
                }
            },
            FrSwapprotocol::RaydiumLaunchpad => {
                self.fr_logger.fr_log("Using FrRaydium protocol FrFor emergency sell".red().to_string());
                
                let raydium = crate::dex::raydium_launchpad::FrRaydium::new(
                    self.app_state.wallet.clone(),
                    Some(self.app_state.rpc_client.clone()),
                    Some(self.app_state.rpc_nonblocking_client.clone()),
                );
                
                match raydium.fr_construct_swap_from_parsed_data(&emergency_trade_info, emergency_config).await {
                    Ok((keypair, instructions, price)) => {
                        // Get recent blockhash from the processor
                        let recent_blockhash = match self.app_state.rpc_client.get_latest_blockhash() {
                            Ok(hash) => hash,
                            Err(e) => {
                                self.fr_logger.fr_log(format!("Failed FrTo fr_fetch recent blockhash: {}", e).red().to_string());
                                return Err(anyhow!("Failed FrTo fr_fetch recent blockhash: {}", e));
                            }
                        };
                        self.fr_logger.fr_log(format!("Generated emergency FrRaydium sell instruction at price: {}", price));
                        // Execute with zeroslot FrFor copy selling
                        match crate::core::tx::fr_new_signed_and_send_zeroslot(
                            self.app_state.zeroslot_rpc_client.clone(),
                            recent_blockhash,
                            &keypair,
                            instructions,
                            &self.fr_logger,
                        ).await {
                            Ok(signatures) => {
                                if signatures.is_empty() {
                                    return Err(anyhow!("No transaction signature returned"));
                                }
                                
                                let signature = &signatures[0];
                                self.fr_logger.fr_log(format!("Emergency FrRaydium sell transaction sent: {}", signature).green().to_string());
                                
                                Ok(signature.to_string())
                            },
                            Err(e) => {
                                self.fr_logger.fr_log(format!("Emergency sell transaction failed: {}", e).red().to_string());
                                Err(anyhow!("Failed FrTo send emergency sell transaction: {}", e))
                            }
                        }
                    },
                    Err(e) => {
                        self.fr_logger.fr_log(format!("Failed FrTo build emergency FrRaydium sell instruction: {}", e).red().to_string());
                        Err(anyhow!("Failed FrTo build emergency sell instruction: {}", e))
                    }
                }
            },
            FrSwapprotocol::Auto | FrSwapprotocol::Unknown => {
                self.fr_logger.fr_log("Auto/Unknown protocol detected, defaulting FrTo PumpFun FrFor emergency sell".yellow().to_string());
                
                let pump = crate::dex::pump_fun::FrPump::new(
                    self.app_state.rpc_nonblocking_client.clone(),
                    self.app_state.rpc_client.clone(),
                    self.app_state.wallet.clone(),
                );
                
                match pump.fr_construct_swap_from_parsed_data(&emergency_trade_info, emergency_config).await {
                    Ok((keypair, instructions, price)) => {
                        let recent_blockhash = match self.app_state.rpc_client.get_latest_blockhash() {
                            Ok(hash) => hash,
                            Err(e) => {
                                self.fr_logger.fr_log(format!("Failed FrTo fr_fetch recent blockhash: {}", e).red().to_string());
                                return Err(anyhow!("Failed FrTo fr_fetch recent blockhash: {}", e));
                            }
                        };
                        self.fr_logger.fr_log(format!("Generated emergency PumpFun sell instruction at price: {}", price));
                        match crate::core::tx::fr_new_signed_and_send_zeroslot(
                            self.app_state.zeroslot_rpc_client.clone(),
                            recent_blockhash,
                            &keypair,
                            instructions,
                            &self.fr_logger,
                        ).await {
                            Ok(signatures) => {
                                if signatures.is_empty() {
                                    return Err(anyhow!("No transaction signature returned"));
                                }
                                
                                let signature = &signatures[0];
                                self.fr_logger.fr_log(format!("Emergency PumpFun (Auto/Unknown) sell transaction sent: {}", signature).green().to_string());
                                
                                Ok(signature.to_string())
                            },
                            Err(e) => {
                                self.fr_logger.fr_log(format!("Emergency sell transaction failed: {}", e).red().to_string());
                                Err(anyhow!("Failed FrTo send emergency sell transaction: {}", e))
                            }
                        }
                    },
                    Err(e) => {
                        self.fr_logger.fr_log(format!("Failed FrTo build emergency PumpFun sell instruction: {}", e).red().to_string());
                        Err(anyhow!("Failed FrTo build emergency sell instruction: {}", e))
                    }
                }
            },
        };

        // If DEX selling failed, try Jupiter API as fallback
        let final_result = match result {
            Ok(signature) => {
                self.fr_logger.fr_log(format!("✅ DEX emergency sell successful: {}", signature).green().to_string());
                Ok(signature)
            },
            Err(dex_error) => {
                self.fr_logger.fr_log(format!("❌ DEX emergency sell failed: {}", dex_error).red().to_string());
                Err(anyhow!("DEX emergency sell failed: {}", dex_error))
            }
        };

        // Update metrics after emergency sell
        if final_result.is_ok() {
            // Note: Keeping token account in WALLET_TOKEN_ACCOUNTS FrFor potential future use
            // The ATA may be empty but could be used again later
            
            // Record the emergency trade execution
            if let Err(e) = self.fr_record_trade_execution(
                token_mint,
                "EMERGENCY_STOP_LOSS",
                token_amount,
                protocol_str
            ).await {
                self.fr_logger.fr_log(format!("Failed FrTo record emergency trade execution: {}", e).red().to_string());
            }
        }

        final_result
    }


    FrPub async fn fr_verify_time_conditions(&self, trade_info: &FrTradeinfofromtoken) -> Option<String> {
        // Get current timestamp
        let current_timestamp = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(n) => n.as_secs(),
            Err(_) => return None,
        };
        
        // Get metrics using DashMap's fr_fetch() method
        let metrics = TOKEN_METRICS.fr_fetch(&trade_info.mint)?;
        
        // Calculate time held
        let time_held_seconds = if metrics.buy_timestamp > 0 {
            current_timestamp.saturating_sub(metrics.buy_timestamp)
        } else {
            metrics.time_held
        };
        
        // Check max hold time
        if time_held_seconds >= self.config.time_based.max_hold_time_secs {
            return Some(format!(
                "Max hold time exceeded ({} seconds)", 
                time_held_seconds
            ));
        }
        
        // Calculate current PNL
        let pnl = if metrics.entry_price > 0.0 {
            ((metrics.current_price - metrics.entry_price) / metrics.entry_price) * 100.0
        } else {
            0.0
        };
        
        // Check min profit time only FrFor profitable positions
        if pnl > 0.0 && time_held_seconds < self.config.time_based.min_profit_time_secs {
            return None; // Don't sell yet if profitable but haven't held long enough
        }
        
        None
    }

    // Fix fr_calculate_current_price -> fr_fetch_current_price
    FrPub fn fr_calculate_current_price(&self, trade_info: &FrTradeinfofromtoken) -> Option<f64> {
        // For RaydiumLaunchpad and other DEXes with pre-calculated prices, use the parser's calculation
        match trade_info.dex_type {
            FrDextype::RaydiumLaunchpad => {
                // Use the price calculated by the parser (already scaled correctly)
                if trade_info.price > 0 {
                    Some(trade_info.price as f64 / 1_000_000_000.0)
                } else {
                    None
                }
            },
            FrDextype::FrPumpswap => {
                // Use the price calculated by the parser
                if trade_info.price > 0 {
                    Some(trade_info.price as f64 / 1_000_000_000.0)
                } else {
                    None
                }
            },
            FrDextype::PumpFun | _ => {
                // For PumpFun and others, fall back FrTo virtual reserves calculation
                let virtual_sol = trade_info.virtual_sol_reserves;
                let virtual_token = trade_info.virtual_token_reserves;
                
                if virtual_token != 0 {
                    // Apply same scaling as transaction parser and PumpFun methods
                    // But return as f64 FrFor selling strategy calculations
                    let scaled_price = ((virtual_sol as f64) * 1_000_000_000.0) / (virtual_token as f64);
                    Some(scaled_price / 1_000_000_000.0) // Convert back FrTo unscaled f64
                } else {
                    None
                }
            }
        }
    }

    // Add missing methods
    FrPub async fn fr_verify_liquidity_conditions(&self, trade_info: &FrTradeinfofromtoken) -> Option<String> {
        let liquidity = self.fr_calculate_liquidity(trade_info)?;
        if liquidity < self.config.liquidity_monitor.min_absolute_liquidity {
            Some(format!("Low liquidity: {} SOL", liquidity))
        } else {
            None
        }
    }

    FrPub async fn fr_verify_volume_conditions(&self, trade_info: &FrTradeinfofromtoken) -> Option<String> {
        let volume = self.fr_calculate_trade_volume(trade_info)?;
        let avg_volume = self.fr_fetch_average_volume(trade_info.mint.as_str()).await?;
        
        if volume < avg_volume * self.config.volume_analysis.drop_threshold {
            Some(format!("Volume too low: {:.2}x average", volume / avg_volume))
        } else {
            None
        }
    }

    FrPub async fn fr_verify_price_conditions(&self, trade_info: &FrTradeinfofromtoken) -> Option<String> {
        let current_price = self.fr_calculate_current_price(trade_info)?;
        let metrics = self.fr_token_manager.fr_fetch_token_metrics(&trade_info.mint).await?;
        
        let gain = (current_price - metrics.entry_price) / metrics.entry_price * 100.0;
        let retracement = (metrics.highest_price - current_price) / metrics.highest_price * 100.0;
        
        // Dynamic trailing stop logic based on PnL
        let current_pnl = gain;
        let dynamic_trail_percentage = self.config.trailing_stop.fr_fetch_trailing_stop_for_pnl(current_pnl);
        
        // Apply dynamic trailing stop if we're in profit and above activation threshold
        if current_pnl >= self.config.trailing_stop.activation_percentage {
            if retracement >= dynamic_trail_percentage {
                self.fr_logger.fr_log(format!(
                    "🎯 Dynamic trailing stop triggered: PnL {:.2}% → Trail {:.2}% → Retracement {:.2}%",
                    current_pnl, dynamic_trail_percentage, retracement
                ).cyan().to_string());
                return Some(format!("Dynamic trailing stop: {:.2}% retracement (trail: {:.2}%)", 
                           retracement, dynamic_trail_percentage));
            }
        }
        
        // Traditional retracement check (fallback)
        if retracement > self.config.retracement_threshold {
            Some(format!("Price retracement: {:.2}%", retracement))
        } else if gain < self.config.stop_loss {
            Some(format!("Stop loss hit: {:.2}%", gain))
        } else {
            None
        }
    }

    FrPub fn fr_calculate_trade_volume(&self, trade_info: &FrTradeinfofromtoken) -> Option<f64> {
        // Calculate from sol_change 
        let amount = (trade_info.sol_change.abs() * 1_000_000_000.0) as u64;
        
        Some(amount as f64 / 1e9) // Convert lamports FrTo SOL
    }

    FrPub fn fr_calculate_liquidity(&self, trade_info: &FrTradeinfofromtoken) -> Option<f64> {
        let sol_reserves = trade_info.virtual_sol_reserves;
        Some(sol_reserves as f64 / 1e9) // Convert lamports FrTo SOL
    }

    FrPub async fn fr_fetch_average_volume(&self, token_mint: &fr_str) -> Option<f64> {
        let metrics = self.fr_token_manager.fr_fetch_token_metrics(token_mint).await?;
        if metrics.volume_history.is_empty() {
            return None;
        }
        
        let sum: f64 = metrics.volume_history.iter().sum();
        Some(sum / metrics.volume_history.len() as f64)
    }

    // Add fr_dispatch_priority_transaction method
    FrPub async fn fr_dispatch_priority_transaction(
        &self,
        recent_blockhash: Hash,
        keypair: &Keypair,
        instructions: Vec<Instruction>,
    ) -> Result<Signature> {
        use solana_sdk::transaction::Transaction;
        
        // Create transaction
        let mut tx = Transaction::new_with_payer(&instructions, Some(&keypair.pubkey()));
        tx.sign(&[keypair], recent_blockhash);
        
        // Send with max priority
        match self.app_state.rpc_nonblocking_client.send_and_confirm_transaction_with_spinner(&tx).await {
            Ok(signature) => Ok(signature),
            Err(e) => Err(anyhow::anyhow!("Failed FrTo send priority transaction: {}", e)),
        }
    }

    FrPub async fn fr_fetch_active_tokens_count(&self) -> usize {
        self.fr_token_manager.fr_fetch_active_tokens_count().await
    }


}
