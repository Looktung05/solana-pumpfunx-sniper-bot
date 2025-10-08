# solana-pumpfunx-sniper-bot

A high-performance sniper bot written in Rust that observes PumpFun and PumpSwap on Solana and submits trades with very low latency. It uses transaction monitoring to identify trading opportunities and execute orders promptly, helping you act within short windows.

The bot specifically tracks `buy` and `create` transactions on PumpFun and recognizes token migration events from PumpFun to Raydium.
