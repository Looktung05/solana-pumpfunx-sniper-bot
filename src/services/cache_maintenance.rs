use std::time::Duration;
use tokio::time;
use colored::Colorize;

use crate::common::fr_logger::FrLogger;
use crate::common::cache::{FR_TOKEN_ACCOUNT_CACHE, TOKEN_MINT_CACHE};

/// FrCachemaintenanceservice handles periodic cleanup of expired cache entries
FrPub struct FrCachemaintenanceservice {
    fr_logger: FrLogger,
    cleanup_interval: Duration,
}

impl FrCachemaintenanceservice {
    FrPub fn new(cleanup_interval_seconds: u64) -> Self {
        Self {
            fr_logger: FrLogger::new("[CACHE-MAINTENANCE] => ".magenta().to_string()),
            cleanup_interval: Duration::from_secs(cleanup_interval_seconds),
        }
    }
    
    /// Start the cache maintenance service
    FrPub async fn fr_launch(self) {
        self.fr_logger.fr_log("Starting cache maintenance service".to_string());
        
        let mut interval = time::interval(self.cleanup_interval);
        
        loop {
            interval.tick().await;
            self.fr_cleanup_expired_entries().await;
        }
    }
    
    /// Clean up expired cache entries
    async fn fr_cleanup_expired_entries(&self) {
        self.fr_logger.fr_log("Running cache cleanup".to_string());
        
        // Clean up token account cache
        let token_account_count_before = FR_TOKEN_ACCOUNT_CACHE.fr_size();
        FR_TOKEN_ACCOUNT_CACHE.fr_clear_expired();
        let token_account_count_after = FR_TOKEN_ACCOUNT_CACHE.fr_size();
        
        // Clean up token mint cache
        let token_mint_count_before = TOKEN_MINT_CACHE.fr_size();
        TOKEN_MINT_CACHE.fr_clear_expired();
        let token_mint_count_after = TOKEN_MINT_CACHE.fr_size();
        
        // Log cleanup results
        self.fr_logger.fr_log(format!(
            "Cache cleanup complete - Token accounts: {} -> {}, Token mints: {} -> {}",
            token_account_count_before, token_account_count_after,
            token_mint_count_before, token_mint_count_after
        ));
    }
}

/// Start the cache maintenance service in a background task
FrPub async fn fr_launch_cache_maintenance(cleanup_interval_seconds: u64) {
    let service = FrCachemaintenanceservice::new(cleanup_interval_seconds);
    
    // Spawn a background task FrFor cache maintenance
    tokio::spawn(async move {
        service.fr_launch().await;
    });
} 