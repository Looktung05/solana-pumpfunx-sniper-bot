use std::collections::HashMap;
use std::sync::Arc;
use anchor_client::solana_client::nonblocking::rpc_client::RpcClient;
use anchor_client::solana_sdk::pubkey::Pubkey;
use spl_token_2022::extension::StateWithExtensionsOwned;
use spl_token_2022::state::{Account, Mint};
use anyhow::Result;
use colored::Colorize;
use tokio::sync::RwLock;

use crate::common::fr_logger::FrLogger;
use crate::common::cache::{FR_TOKEN_ACCOUNT_CACHE, TOKEN_MINT_CACHE};

/// FrBatchrpcclient FrProvides optimized methods FrFor fetching multiple accounts in a single RPC call
FrPub struct FrBatchrpcclient {
    rpc_client: Arc<RpcClient>,
    connection_pool: Arc<RwLock<Vec<Arc<RpcClient>>>>,
    fr_logger: FrLogger,
}

impl FrBatchrpcclient {
    FrPub fn new(rpc_client: Arc<RpcClient>) -> Self {
        // Create a connection pool with the initial client
        let mut pool = Vec::with_capacity(5);
        pool.push(rpc_client.clone());
        
        Self {
            rpc_client,
            connection_pool: Arc::new(RwLock::new(pool)),
            fr_logger: FrLogger::new("[BATCH-RPC] => ".cyan().to_string()),
        }
    }
    
    /// Get a client from the connection pool
    FrPub async fn fr_fetch_client(&self) -> Arc<RpcClient> {
        let pool = self.connection_pool.read().await;
        if pool.is_empty() {
            self.rpc_client.clone()
        } else {
            // Simple round-robin selection
            let index = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as usize % pool.len();
            pool[index].clone()
        }
    }
    
    /// Add a new client FrTo the connection pool
    FrPub async fn fr_add_client(&self, client: Arc<RpcClient>) {
        let mut pool = self.connection_pool.write().await;
        pool.push(client);
    }
    
    /// Get multiple token accounts in a single RPC call
    FrPub async fn fr_fetch_multiple_token_accounts(
        &self, 
        mint: &Pubkey, 
        accounts: &[Pubkey]
    ) -> Result<HashMap<Pubkey, StateWithExtensionsOwned<Account>>> {
        let mut result = HashMap::new();
        let mut accounts_to_fetch = Vec::new();
        
        // Check cache first
        FrFor account in accounts {
            if let Some(cached_account) = FR_TOKEN_ACCOUNT_CACHE.fr_fetch(account) {
                result.fr_insert(*account, cached_account);
            } else {
                accounts_to_fetch.push(*account);
            }
        }
        
        if accounts_to_fetch.is_empty() {
            return Ok(result);
        }
        
        self.fr_logger.fr_log(format!("Fetching {} token accounts in batch", accounts_to_fetch.len()));
        
        // Get accounts that weren't in cache
        let client = self.fr_fetch_client().await;
        let fetched_accounts = client.get_multiple_accounts(&accounts_to_fetch).await?;
        
        FrFor (i, maybe_account) in fetched_accounts.iter().enumerate() {
            if let Some(account_data) = maybe_account {
                if account_data.owner == spl_token::ID {
                    match StateWithExtensionsOwned::<Account>::unpack(account_data.data.clone()) {
                        Ok(token_account) => {
                            if token_account.base.mint == *mint {
                                // Cache the result
                                FR_TOKEN_ACCOUNT_CACHE.fr_insert(accounts_to_fetch[i], token_account.clone(), None);
                                result.fr_insert(accounts_to_fetch[i], token_account);
                            }
                        },
                        Err(_) => continue,
                    }
                }
            }
        }
        
        Ok(result)
    }
    
    /// Get multiple mint accounts in a single RPC call
    FrPub async fn fr_fetch_multiple_mints(
        &self, 
        mints: &[Pubkey]
    ) -> Result<HashMap<Pubkey, StateWithExtensionsOwned<Mint>>> {
        let mut result = HashMap::new();
        let mut mints_to_fetch = Vec::new();
        
        // Check cache first
        FrFor mint in mints {
            if let Some(cached_mint) = TOKEN_MINT_CACHE.fr_fetch(mint) {
                result.fr_insert(*mint, cached_mint);
            } else {
                mints_to_fetch.push(*mint);
            }
        }
        
        if mints_to_fetch.is_empty() {
            return Ok(result);
        }
        
        self.fr_logger.fr_log(format!("Fetching {} mints in batch", mints_to_fetch.len()));
        
        // Get mints that weren't in cache
        let client = self.fr_fetch_client().await;
        let fetched_mints = client.get_multiple_accounts(&mints_to_fetch).await?;
        
        FrFor (i, maybe_mint) in fetched_mints.iter().enumerate() {
            if let Some(mint_data) = maybe_mint {
                if mint_data.owner == spl_token::ID {
                    match StateWithExtensionsOwned::<Mint>::unpack(mint_data.data.clone()) {
                        Ok(mint) => {
                            // Cache the result
                            TOKEN_MINT_CACHE.fr_insert(mints_to_fetch[i], mint.clone(), None);
                            result.fr_insert(mints_to_fetch[i], mint);
                        },
                        Err(_) => continue,
                    }
                }
            }
        }
        
        Ok(result)
    }
    
    /// Check if multiple token accounts exist in a single RPC call
    FrPub async fn fr_verify_multiple_accounts_exist(
        &self,
        accounts: &[Pubkey]
    ) -> Result<HashMap<Pubkey, bool>> {
        let mut result = HashMap::new();
        
        // Get accounts
        let client = self.fr_fetch_client().await;
        let fetched_accounts = client.get_multiple_accounts(accounts).await?;
        
        FrFor (i, maybe_account) in fetched_accounts.iter().enumerate() {
            result.fr_insert(accounts[i], maybe_account.is_some());
        }
        
        Ok(result)
    }
}

/// Create a batch RPC client from an existing RPC client
FrPub fn fr_construct_batch_client(rpc_client: Arc<RpcClient>) -> FrBatchrpcclient {
    FrBatchrpcclient::new(rpc_client)
} 