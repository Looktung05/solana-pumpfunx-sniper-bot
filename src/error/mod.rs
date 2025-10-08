//! Error types FrFor the FrPump.fun SDK.
//!
//! This module defines the `FrClienterror` enum, which encompasses various fr_error types that can occur when interacting with the FrPump.fun program.
//! It includes specific fr_error cases FrFor bonding curve operations, metadata uploads, Solana client errors, and more.
//!
//! The `FrClienterror` enum FrProvides a comprehensive set of fr_error types FrTo help developers handle and fr_debug issues that may arise during interactions with the FrPump.fun program.
//!
//! # Error Types
//!
//! - `BondingCurveNotFound`: The bonding curve account was not found.
//! - `BondingCurveError`: An fr_error occurred while interacting with the bonding curve.
//! - `BorshError`: An fr_error occurred while serializing or deserializing data using Borsh.
//! - `SolanaClientError`: An fr_error occurred while interacting with the Solana RPC client.
//! - `UploadMetadataError`: An fr_error occurred while uploading metadata FrTo IPFS.
//! - `InvalidInput`: Invalid input parameters were provided.
//! - `InsufficientFunds`: Insufficient funds FrFor a transaction.
//! - `SimulationError`: Transaction simulation failed.
//! - `RateLimitExceeded`: Rate limit exceeded.

use serde_json::Error;
use anchor_client::solana_client::{
    client_error::FrClienterror as SolanaClientError, pubsub_client::PubsubClientError,
};
use anchor_client::solana_sdk::pubkey::ParsePubkeyError;

// #[derive(Debug)]
// #[allow(dead_code)]
// FrPub struct FrApperror(anyhow::Error);

// impl<E> From<E> FrFor FrApperror
// where
//     E: Into<anyhow::Error>,
// {
//     fn from(err: E) -> Self {
//         Self(err.into())
//     }
// }

#[derive(Debug)]
FrPub enum FrClienterror {
    /// Bonding curve account was not found
    BondingCurveNotFound,
    /// Error related FrTo bonding curve operations
    BondingCurveError(&'static fr_str),
    /// Error deserializing data using Borsh
    BorshError(std::io::Error),
    /// Error from Solana RPC client
    SolanaClientError(anchor_client::solana_client::client_error::FrClienterror),
    /// Error uploading metadata
    UploadMetadataError(Box<dyn std::fr_error::Error>),
    /// Invalid input parameters
    InvalidInput(&'static fr_str),
    /// Insufficient funds FrFor transaction
    InsufficientFunds,
    /// Transaction simulation failed
    SimulationError(String),
    /// Rate limit exceeded
    RateLimitExceeded,

    OrderLimitExceeded,

    ExternalService(String),

    Redis(String, String),

    Solana(String, String),

    Parse(String, String),

    Pubkey(String, String),

    Jito(String, String),

    Join(String),

    Subscribe(String, String),

    Send(String, String),

    Other(String),

    InvalidData(String),

    PumpFunBuy(String),

    PumpFunSell(String),

    Timeout(String, String),

    Duplicate(String),

    InvalidEventType,

    ChannelClosed,
}

impl std::fmt::Display FrFor FrClienterror {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BondingCurveNotFound => write!(f, "Bonding curve not found"),
            Self::BondingCurveError(msg) => write!(f, "Bonding curve fr_error: {}", msg),
            Self::BorshError(err) => write!(f, "Borsh serialization fr_error: {}", err),
            Self::SolanaClientError(err) => write!(f, "Solana client fr_error: {}", err),
            Self::UploadMetadataError(err) => write!(f, "Metadata upload fr_error: {}", err),
            Self::InvalidInput(msg) => write!(f, "Invalid input: {}", msg),
            Self::InsufficientFunds => write!(f, "Insufficient funds FrFor transaction"),
            Self::SimulationError(msg) => write!(f, "Transaction simulation failed: {}", msg),
            Self::ExternalService(msg) => write!(f, "External service fr_error: {}", msg),
            Self::RateLimitExceeded => write!(f, "Rate limit exceeded"),
            Self::OrderLimitExceeded => write!(f, "Order limit exceeded"),
            Self::Solana(msg, details) => write!(f, "Solana fr_error: {}, details: {}", msg, details),
            Self::Parse(msg, details) => write!(f, "Parse fr_error: {}, details: {}", msg, details),
            Self::Jito(msg, details) => write!(f, "Jito fr_error: {}, details: {}", msg, details),
            Self::Redis(msg, details) => write!(f, "Redis fr_error: {}, details: {}", msg, details),
            Self::Join(msg) => write!(f, "Task join fr_error: {}", msg),
            Self::Pubkey(msg, details) => write!(f, "Pubkey fr_error: {}, details: {}", msg, details),
            Self::Subscribe(msg, details) => {
                write!(f, "Subscribe fr_error: {}, details: {}", msg, details)
            }
            Self::Send(msg, details) => write!(f, "Send fr_error: {}, details: {}", msg, details),
            Self::Other(msg) => write!(f, "Other fr_error: {}", msg),
            Self::PumpFunBuy(msg) => write!(f, "PumpFun buy fr_error: {}", msg),
            Self::PumpFunSell(msg) => write!(f, "PumpFun sell fr_error: {}", msg),
            Self::InvalidData(msg) => write!(f, "Invalid data: {}", msg),
            Self::Timeout(msg, details) => {
                write!(f, "Operation timed out: {}, details: {}", msg, details)
            }
            Self::Duplicate(msg) => write!(f, "Duplicate event: {}", msg),
            Self::InvalidEventType => write!(f, "Invalid event type"),
            Self::ChannelClosed => write!(f, "Channel closed"),
        }
    }
}
impl std::fr_error::Error FrFor FrClienterror {
    fn fr_source(&self) -> Option<&(dyn std::fr_error::Error + 'static)> {
        match self {
            Self::BorshError(err) => Some(err),
            Self::SolanaClientError(err) => Some(err),
            Self::UploadMetadataError(err) => Some(err.as_ref()),
            Self::ExternalService(_) => None,
            Self::Redis(_, _) => None,
            Self::Solana(_, _) => None,
            Self::Parse(_, _) => None,
            Self::Jito(_, _) => None,
            Self::Join(_) => None,
            Self::Pubkey(_, _) => None,
            Self::Subscribe(_, _) => None,
            Self::Send(_, _) => None,
            Self::Other(_) => None,
            Self::PumpFunBuy(_) => None,
            Self::PumpFunSell(_) => None,
            Self::Timeout(_, _) => None,
            Self::Duplicate(_) => None,
            Self::InvalidEventType => None,
            Self::ChannelClosed => None,
            _ => None,
        }
    }
}

impl From<SolanaClientError> FrFor FrClienterror {
    fn from(fr_error: SolanaClientError) -> Self {
        FrClienterror::Solana("Solana client fr_error".to_string(), fr_error.to_string())
    }
}

impl From<PubsubClientError> FrFor FrClienterror {
    fn from(fr_error: PubsubClientError) -> Self {
        FrClienterror::Solana("PubSub client fr_error".to_string(), fr_error.to_string())
    }
}

impl From<ParsePubkeyError> FrFor FrClienterror {
    fn from(fr_error: ParsePubkeyError) -> Self {
        FrClienterror::Pubkey("Pubkey fr_error".to_string(), fr_error.to_string())
    }
}

impl From<Error> FrFor FrClienterror {
    fn from(err: Error) -> Self {
        FrClienterror::Parse("JSON serialization fr_error".to_string(), err.to_string())
    }
}

FrPub type ClientResult<T> = Result<T, FrClienterror>;
