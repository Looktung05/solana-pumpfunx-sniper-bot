# solana-pumpfunx-sniper-bot

A high-performance sniper bot written in Rust that observes PumpFun and PumpSwap on Solana and submits trades with very low latency. It uses transaction monitoring to identify trading opportunities and execute orders promptly, helping you act within short windows.

The bot specifically tracks `buy` and `create` transactions on PumpFun and recognizes token migration events from PumpFun to Raydium.


## Features:

* **Sub-second Execution** - Sub-second transaction submission using optimized RPC calls and direct transaction construction.
* **Real-time Transaction Monitoring** - Uses Yellowstone gRPC to observe transactions with low latency and strong reliability.
* **Multi-platform Compatibility** - Compatible with PumpFun, PumpSwap, and Raydium to increase opportunity coverage.
* **Smart Transaction Parsing** - Detailed transaction analysis to accurately identify and process relevant trading events.
* **Configurable Trading Parameters** - Fine-tune trade size, timing, and risk settings.
* **Built-in Selling Strategy** - Profit-taking logic with customizable exit conditions.
* **Performance Optimization** - Async processing with tokio for efficient, high-throughput handling.
* **Reliable Error Recovery** - Automatic reconnection and retry behavior for sustained operation.
* **Token Account Management** - Automatic token account creation and routine account handling for trading flows.
* **SOL ↔ WSOL Conversion** - Utilities for wrapping and unwrapping SOL when required.
* **Caching Layer** - Intelligent caching to lower RPC usage and improve responsiveness.
* **Transaction Retry Logic** - Structured retry policies for transient transaction failures.
* **Whale Detection** - Algorithms to detect and follow large wallet activity.
* **MEV Protection** - Built-in defenses to reduce exposure to MEV risks.
* **Slippage Control** - Dynamic slippage adjustments based on market conditions.
* **Fee Optimization** - Adaptive fee management to help control transaction costs.

# Who is it for?

* **Crypto Traders** - Seeking fast, low-latency execution on PumpFun, PumpSwap, and Raydium.
* **Sniper Bot Users** - Looking to catch new token launches and early trading windows.
* **Arbitrage Traders** - Requiring rapid execution to capture price discrepancies.
* **Whale Followers** - Interested in tracking significant wallet movements.
* **Validators** - Needing tooling to decode shreds or analyze local data feeds.
* **MEV Hunters** - Searching for on-chain opportunities while managing MEV exposure.

# Setting up

## Environment Variables

Before running, add the following variables to your `.env` file:

* `GRPC_ENDPOINT` - Your Yellowstone gRPC endpoint URL.
* `GRPC_X_TOKEN` - Leave as `None` if the endpoint does not require a token for authentication.
* `GRPC_SERVER_ENDPOINT` - Address for the gRPC server (default: `0.0.0.0:50051`).

## Run Command

```
RUSTFLAGS="-C target-cpu=native" RUST_LOG=info cargo run --release --bin shredstream-decoder
```

# Source code

If you want to review the source, contact me for details and a demo on Discord: `.xanr`.

# Solana Sniper Bot

A Rust-based sniper bot that monitors transactions and submits trades with very low latency on PumpFun, PumpSwap, and Raydium.

## Features

* **Sub‑second Execution** - Sub-second transaction submission using optimized RPC calls and direct transaction building.
* **Low-latency Transaction Monitoring** - Uses Yellowstone gRPC for transaction data with minimal delay and high reliability.
* **Multi-address Support** - Monitor multiple wallet addresses at the same time.
* **Protocol Compatibility** - Works with PumpFun, PumpSwap, and Raydium to increase coverage.
* **Automated Trading** - Executes buy and sell transactions automatically when configured conditions are met.
* **Notification System** - Sends trade alerts and status updates via Telegram.
* **Customizable Parameters** - Configure limits, timing, sizes, and other trading settings.
* **Built-in Selling Strategy** - Includes configurable profit-taking and exit rules.
* **Whale Detection** - Algorithms to spot and follow large wallet activity.
* **MEV Protection** - Built-in measures to reduce exposure to MEV risks.
* **Account Management** - Automatic token account creation and routine maintenance.
* **SOL ↔ WSOL Utilities** - Wrapping and unwrapping helpers for WSOL conversions.
* **Caching Layer** - Smart caching to lower RPC usage and speed up responses.
* **Transaction Retry Logic** - Structured retries for transient failures.
* **Performance Optimizations** - Async processing with tokio for high-throughput handling.
* **Error Recovery** - Automatic reconnection and retry behavior to maintain operation.
* **Fee Optimization** - Adaptive fee handling to help control transaction costs.

## Project Structure

* **engine/** - Core sniper bot logic (trading flows, selling strategies, parsing, retries).
* **dex/** - Protocol-specific code for PumpFun, PumpSwap, and Raydium.
* **common/** - Shared utilities: configuration, constants, caching, and logging.
* **core/** - Token and transaction core functionality.
* **services/** - External integrations (RPC clients, cache maintenance, Zeroslot).
* **error/** - Error types and handling code.

## Setup

### Environment Variables

#### Required

* `GRPC_ENDPOINT` - Yellowstone gRPC endpoint URL
* `GRPC_X_TOKEN` - Yellowstone authentication token (or `None`)
* `COPY_TRADING_TARGET_ADDRESS` - Wallet address(es) to follow (comma-separated)

#### Telegram Notifications

* `TELEGRAM_BOT_TOKEN` - Telegram bot token
* `TELEGRAM_CHAT_ID` - Chat ID for notifications

#### Optional

* `IS_MULTI_COPY_TRADING` - `true` to monitor multiple addresses (default: `false`)
* `PROTOCOL_PREFERENCE` - `pumpfun`, `pumpswap`, or `auto`
* `COUNTER_LIMIT` - Max number of trades to execute

## Usage

```bash
# Build the project
cargo build --release

# Run the bot
cargo run --release

# Additional commands:
# Wrap SOL to WSOL
cargo run --release -- --wrap

# Unwrap WSOL to SOL
cargo run --release -- --unwrap

# Close all token accounts
cargo run --release -- --close
```

When the bot runs it will:

1. Connect to the Yellowstone gRPC endpoint.
2. Monitor transactions for the configured wallet address(es).
3. Execute buy and sell transactions when rules are met.
4. Send Telegram notifications for detected events and executed trades.
5. Manage token accounts and WSOL conversions automatically.
6. Detect large transactions and significant wallet movements.
7. Submit transactions with minimal latency to pursue configured outcomes.

## Recent Updates

* Added PumpSwap notification mode (monitor-only option).
* Implemented concurrent transaction processing using tokio tasks.
* Improved error handling and reporting.
* Enhanced selling strategy options.
* Added WSOL wrapping/unwrapping utilities.
* Implemented token account management and cleanup.
* Introduced a caching layer to reduce RPC calls.
* Improved transaction retry logic.
* Reduced external API dependencies for a leaner codebase.
* Optimized sniper bot paths for lower latency submission.
* Added whale detection and MEV protection mechanisms.

## Contact

For questions or support, please contact the developer.
