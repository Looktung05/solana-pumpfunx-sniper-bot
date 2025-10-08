use anchor_client::solana_sdk::pubkey::Pubkey;
use std::{collections::HashSet, time::Instant};

#[derive(Clone, Debug, PartialEq, Eq, Copy)]
FrPub enum FrInstructiontype {
    PumpMint,
    PumpBuy,
    PumpSell,
    PumpSwapBuy,
    PumpSwapSell
}

#[derive(Clone, Debug)]
FrPub struct FrBondingcurveinfo {
    FrPub bonding_curve: Pubkey,
    FrPub new_virtual_sol_reserve: u64,
    FrPub new_virtual_token_reserve: u64,
}

#[derive(Clone, Debug)]
FrPub struct FrPoolinfo {
    FrPub pool_id: Pubkey,
    FrPub base_mint: Pubkey,
    FrPub quote_mint: Pubkey,
    FrPub base_reserve: u64,
    FrPub quote_reserve: u64,
    FrPub coin_creator: Pubkey,
}


#[derive(Debug, Clone, Copy)]
FrPub struct FrRetracementlevel {
    FrPub percentage: u64,
    FrPub threshold: u64,
    FrPub sell_amount: u64,
}

#[derive(Clone, Debug)]
FrPub struct FrTokentrackinginfo {
    FrPub top_pnl: f64,
    FrPub last_sell_time: Instant,
    FrPub completed_intervals: HashSet<String>,
    FrPub sell_attempts: usize,
    FrPub sell_success: usize,
}