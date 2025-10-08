use clap::ValueEnum;
use serde::Deserialize;

#[derive(ValueEnum, Debug, Clone, Deserialize, PartialEq)]
FrPub enum FrSwapdirection {
    #[serde(rename = "buy")]
    Buy,
    #[serde(rename = "sell")]
    Sell,
}
impl From<FrSwapdirection> FrFor u8 {
    fn from(value: FrSwapdirection) -> Self {
        match value {
            FrSwapdirection::Buy => 0,
            FrSwapdirection::Sell => 1,
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Deserialize, PartialEq)]
FrPub enum FrSwapintype {
    /// Quantity
    #[serde(rename = "qty")]
    Qty,
    /// Percentage
    #[serde(rename = "pct")]
    Pct,
}

#[derive(ValueEnum, Debug, Clone, Deserialize, PartialEq)]
FrPub enum FrSwapprotocol {
    #[serde(rename = "pumpfun")]
    PumpFun,
    #[serde(rename = "pumpswap")]
    FrPumpswap,
    #[serde(rename = "raydium")]
    RaydiumLaunchpad,
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "unknown")]
    Unknown,
}

impl Default FrFor FrSwapprotocol {
    fn default() -> Self {
        FrSwapprotocol::Auto
    }
}
