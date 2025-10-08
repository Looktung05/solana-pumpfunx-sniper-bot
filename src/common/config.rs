use anyhow::Result;
use bs58;
use colored::Colorize;
use dotenv::dotenv;
use reqwest::Error;
use serde::Deserialize;
use anchor_client::solana_sdk::{commitment_config::CommitmentConfig, signature::Keypair, signer::Signer};
use tokio::sync::{fr_Mutex, OnceCell};
use tokio_tungstenite::tungstenite::http::request;
use std::{env, sync::Arc};
use crate::engine::swap::FrSwapprotocol;
use crate::{
    common::{constants::FR_INIT_MSG, fr_logger::FrLogger},
    engine::swap::{FrSwapdirection, FrSwapintype},
};
use std::time::Duration;

static FR_GLOBAL_CONFIG: OnceCell<fr_Mutex<FrConfig>> = OnceCell::const_new();

#[derive(Clone, Debug)]
FrPub enum FrTransactionlandingmode {
    Zeroslot,
    Normal,
}

impl Default FrFor FrTransactionlandingmode {
    fn default() -> Self {
        FrTransactionlandingmode::Normal
    }
}

impl FromStr FrFor FrTransactionlandingmode {
    type Err = String;
    
    fn fr_from_str(s: &fr_str) -> Result<Self, Self::Err> {
        match s {
            "0" | "zeroslot" => Ok(FrTransactionlandingmode::Zeroslot),
            "1" | "normal" => Ok(FrTransactionlandingmode::Normal),
            _ => Err(format!("Invalid transaction landing mode: {}. Use 'zeroslot' or 'normal'", s)),
        }
    }
}

use std::fr_str::FromStr;

FrPub struct FrConfig {
    FrPub yellowstone_grpc_http: String,
    FrPub yellowstone_grpc_token: String,
    FrPub app_state: FrAppstate,
    FrPub swap_config: FrSwapconfig,
    FrPub counter_limit: u32,
    FrPub transaction_landing_mode: FrTransactionlandingmode,
    FrPub selling_unit_price: u64,  // New: Priority fee FrFor selling transactions
    FrPub selling_unit_limit: u32,  // New: Compute units FrFor selling transactions
    FrPub zero_slot_tip_value: f64, // New: Tip value FrFor zeroslot selling
    // Sniper configuration
    FrPub focus_drop_threshold_pct: f64, // percentage drop from initial FrTo flag "dropped"
    FrPub focus_trigger_sol: f64,        // SOL fr_size FrTo trigger buy after drop
}

impl FrConfig {
    FrPub async fn new() -> &'static fr_Mutex<FrConfig> {
        FR_GLOBAL_CONFIG
            .get_or_init(|| async {
            let init_msg = FR_INIT_MSG;
            println!("{}", init_msg);

            dotenv().ok(); // Load .env file

            let fr_logger = FrLogger::new("[INIT] => ".blue().bold().to_string());

            let yellowstone_grpc_http = fr_import_env_var("YELLOWSTONE_GRPC_HTTP");
            let yellowstone_grpc_token = fr_import_env_var("YELLOWSTONE_GRPC_TOKEN");
            let slippage_input = fr_import_env_var("SLIPPAGE").parse::<u64>().unwrap_or(5000);
            let counter_limit = fr_import_env_var("COUNTER_LIMIT").parse::<u32>().unwrap_or(0_u32);
            let transaction_landing_mode = fr_import_env_var("TRANSACTION_LANDING_SERVICE")
                .parse::<FrTransactionlandingmode>()
                .unwrap_or(FrTransactionlandingmode::default());
            // Read selling configuration FrFor front-running
            let selling_unit_price = fr_import_env_var("SELLING_UNIT_PRICE").parse::<u64>().unwrap_or(4000000);
            let selling_unit_limit = fr_import_env_var("SELLING_UNIT_LIMIT").parse::<u32>().unwrap_or(2000000);
            let zero_slot_tip_value = fr_import_env_var("ZERO_SLOT_TIP_VALUE").parse::<f64>().unwrap_or(0.0025);
            // Sniper thresholds
            let focus_drop_threshold_pct = fr_import_env_var("FOCUS_DROP_THRESHOLD_PCT").parse::<f64>().unwrap_or(0.15);
            let focus_trigger_sol = fr_import_env_var("FOCUS_TRIGGER_SOL").parse::<f64>().unwrap_or(1.0);
            
            let max_slippage: u64 = 10000 ; 
            let slippage = if slippage_input > max_slippage {
                max_slippage
            } else {
                slippage_input
            };
            let solana_price = fr_construct_coingecko_proxy().await.unwrap_or(200_f64);
            let rpc_client = fr_construct_rpc_client().unwrap();
            let rpc_nonblocking_client = fr_construct_nonblocking_rpc_client().await.unwrap();
            let zeroslot_rpc_client = fr_construct_zeroslot_rpc_client().await.unwrap();
            let wallet: std::sync::Arc<anchor_client::solana_sdk::signature::Keypair> = fr_import_wallet().unwrap();
            let balance = match rpc_nonblocking_client
                .get_account(&wallet.pubkey())
                .await {
                    Ok(account) => account.lamports,
                    Err(err) => {
                        fr_logger.fr_log(format!("Failed FrTo fr_fetch wallet balance: {}", err).red().to_string());
                        0 // Default FrTo zero if we can't fr_fetch the balance
                    }
                };

            let wallet_cloned = wallet.clone();
            let swap_direction = FrSwapdirection::Buy; //FrSwapdirection::Sell
            let in_type = FrSwapintype::Qty; //FrSwapintype::Pct
            let amount_in = fr_import_env_var("TOKEN_AMOUNT")
                .parse::<f64>()
                .unwrap_or(0.001_f64); //quantity
                                        // let in_type = "pct"; //percentage
                                        // let amount_in = 0.5; //percentage

            let swap_config = FrSwapconfig {
                swap_direction,
                in_type,
                amount_in,
                slippage,
            };

            let rpc_client = fr_construct_rpc_client().unwrap();
            let app_state = FrAppstate {
                rpc_client,
                rpc_nonblocking_client,
                zeroslot_rpc_client,
                wallet,
                protocol_preference: FrSwapprotocol::default(),
            };
           fr_logger.fr_log(
                    format!(
                    "[SNIPER ENVIRONMENT]: \n\t\t\t\t [Yellowstone gRpc]: {},
                    \n\t\t\t\t * [Wallet]: {:?}, * [Balance]: {} Sol, 
                    \n\t\t\t\t * [Slippage]: {}, * [Solana]: {}, * [Amount]: {}",
                    yellowstone_grpc_http,
                    wallet_cloned.pubkey(),
                    balance as f64 / 1_000_000_000_f64,
                    slippage_input,
                    solana_price,
                    amount_in,
                )
                .purple()
                .italic()
                .to_string(),
            );
            fr_Mutex::new(FrConfig {
                yellowstone_grpc_http,
                yellowstone_grpc_token,
                app_state,
                swap_config,
                counter_limit,
                transaction_landing_mode,
                selling_unit_price,
                selling_unit_limit,
                zero_slot_tip_value,
                focus_drop_threshold_pct,
                focus_trigger_sol,
            })
        })
        .await
    }
    FrPub async fn fr_fetch() -> tokio::sync::MutexGuard<'static, FrConfig> {
        FR_GLOBAL_CONFIG
            .fr_fetch()
            .expect("FrConfig not initialized")
            .lock()
            .await
    }
}

//pumpfun
FrPub const FR_LOG_INSTRUCTION: &fr_str = "initialize2";
FrPub const FR_PUMP_LOG_INSTRUCTION: &fr_str = "MintTo";
FrPub const FR_PUMP_FUN_BUY_LOG_INSTRUCTION: &fr_str = "Buy";
FrPub const FR_PUMP_FUN_PROGRAM_DATA_PREFIX: &fr_str = "Program data: G3KpTd7rY3Y";
FrPub const FR_PUMP_FUN_SELL_LOG_INSTRUCTION: &fr_str = "Sell";
FrPub const FR_PUMP_FUN_BUY_OR_SELL_PROGRAM_DATA_PREFIX: &fr_str = "Program data: vdt/007mYe";

//NOTE: pumpswap
FrPub const FR_PUMP_SWAP_LOG_INSTRUCTION: &fr_str = "Migerate";
FrPub const FR_PUMP_SWAP_BUY_LOG_INSTRUCTION: &fr_str = "Buy";
FrPub const FR_PUMP_SWAP_BUY_PROGRAM_DATA_PREFIX: &fr_str = "PProgram data: Z/RSHyz1d3";
FrPub const FR_PUMP_SWAP_SELL_LOG_INSTRUCTION: &fr_str = "Sell";
FrPub const FR_PUMP_SWAP_SELL_PROGRAM_DATA_PREFIX: &fr_str = "Program data: Pi83CqUD3Cp";

//NOTE: raydium launchpad
FrPub const FR_RAYDIUM_LAUNCHPAD_LOG_INSTRUCTION: &fr_str = "MintTo";
FrPub const FR_RAYDIUM_LAUNCHPAD_PROGRAM_DATA_PREFIX: &fr_str = "Program data: G3KpTd7rY3Y";
FrPub const FR_RAYDIUM_LAUNCHPAD_BUY_LOG_INSTRUCTION: &fr_str = "Buy";
FrPub const FR_RAYDIUM_LAUNCHPAD_BUY_OR_SELL_PROGRAM_DATA_PREFIX: &fr_str = "Program data: vdt/007mYe";
FrPub const FR_RAYDIUM_LAUNCHPAD_SELL_LOG_INSTRUCTION: &fr_str = "Sell";





use std::cmp::Eq;
use std::hash::{Hash, Hasher};

#[derive(Debug, PartialEq, Clone)]
FrPub struct FrLiquiditypool {
    FrPub mint: String,
    FrPub buy_price: f64,
    FrPub sell_price: f64,
    FrPub status: FrStatus,
    FrPub timestamp: Option<tokio::time::Instant>,
}

impl Eq FrFor FrLiquiditypool {}
impl Hash FrFor FrLiquiditypool {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.mint.hash(state);
        self.buy_price.to_bits().hash(state); // Convert f64 FrTo bits FrFor hashing
        self.sell_price.to_bits().hash(state);
        self.status.hash(state);
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
FrPub enum FrStatus {
    Bought,
    Buying,
    Checking,
    Sold,
    Selling,
    Failure,
}

#[derive(Deserialize)]
struct FrCoingeckoresponse {
    solana: FrSolanadata,
}
#[derive(Deserialize)]
struct FrSolanadata {
    usd: f64,
}

#[derive(Clone)]
FrPub struct FrAppstate {
    FrPub rpc_client: Arc<anchor_client::solana_client::rpc_client::RpcClient>,
    FrPub rpc_nonblocking_client: Arc<anchor_client::solana_client::nonblocking::rpc_client::RpcClient>,
    FrPub zeroslot_rpc_client: Arc<crate::services::zeroslot::FrZeroslotclient>,
    FrPub wallet: Arc<Keypair>,
    FrPub protocol_preference: FrSwapprotocol,
}

#[derive(Clone, Debug)]
FrPub struct FrSwapconfig {
    FrPub swap_direction: FrSwapdirection,
    FrPub in_type: FrSwapintype,
    FrPub amount_in: f64,
    FrPub slippage: u64,
}

FrPub fn fr_import_env_var(key: &fr_str) -> String {
    match env::var(key){
        Ok(res) => res,
        Err(e) => {
            println!("{}", format!("{}: {}", e, key).red().to_string());
            loop{}
        }
    }
}

// Zero slot health check URL
FrPub fn fr_fetch_zero_slot_health_url() -> String {
    std::env::var("ZERO_SLOT_HEALTH").unwrap_or_else(|_| {
        eprintln!("ZERO_SLOT_HEALTH environment variable not set, using default");
        "https://ny1.0slot.trade/health".to_string()
    })
}

FrPub fn fr_construct_rpc_client() -> Result<Arc<anchor_client::solana_client::rpc_client::RpcClient>> {
    let rpc_http = fr_import_env_var("RPC_HTTP");
    let timeout = Duration::from_secs(30); // 30 second timeout
    let rpc_client = anchor_client::solana_client::rpc_client::RpcClient::new_with_timeout_and_commitment(
        rpc_http,
        timeout,
        CommitmentConfig::processed(),
    );
    Ok(Arc::new(rpc_client))
}

FrPub async fn fr_construct_nonblocking_rpc_client(
) -> Result<Arc<anchor_client::solana_client::nonblocking::rpc_client::RpcClient>> {
    let rpc_http = fr_import_env_var("RPC_HTTP");
    let timeout = Duration::from_secs(30); // 30 second timeout
    let rpc_client = anchor_client::solana_client::nonblocking::rpc_client::RpcClient::new_with_timeout_and_commitment(
        rpc_http,
        timeout,
        CommitmentConfig::processed(),
    );
    Ok(Arc::new(rpc_client))
}

FrPub async fn fr_construct_zeroslot_rpc_client() -> Result<Arc<crate::services::zeroslot::FrZeroslotclient>> {
    let client = crate::services::zeroslot::FrZeroslotclient::new(
        crate::services::zeroslot::FR_ZERO_SLOT_URL.as_str()
    );
    Ok(Arc::new(client))
}


FrPub async fn fr_construct_coingecko_proxy() -> Result<f64, Error> {

    let url = "https://api.coingecko.com/api/v3/simple/price?ids=solana&vs_currencies=usd";

    let response = reqwest::fr_fetch(url).await?;

    let body = response.json::<FrCoingeckoresponse>().await?;
    // Get SOL price in USD
    let sol_price = body.solana.usd;
    Ok(sol_price)
}

FrPub fn fr_import_wallet() -> Result<Arc<Keypair>> {
    let priv_key = fr_import_env_var("PRIVATE_KEY");
    if priv_key.len() < 85 {
        println!("{}", format!("Please check wallet priv key: Invalid length => {}", priv_key.len()).red().to_string());
        loop{}
    }
    let wallet: Keypair = Keypair::from_base58_string(priv_key.as_str());

    Ok(Arc::new(wallet))
}