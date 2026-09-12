use serde::{Deserialize, Serialize};

pub const ARB_MAX_LEVELS: usize = 64;
pub const ARB_MAX_TRACKED: usize = 32;
pub const IPC_VERSION: u8 = 2;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Side {
    Bid,
    Ask,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Venue {
    Poly,
    Kalshi,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct PriceLevel {
    pub price: f64,
    pub size: f64,
}

/// Taker-venue order book (always in **US dollar** prices per outcome share, 0–1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TakerFullBookPayload {
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TakerLevelVolUpdatePayload {
    pub side: Side,
    pub levels: Vec<(i16, f64)>,
    #[serde(default, alias = "poly_book")]
    pub taker_book: Option<TakerFullBookPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MakerLevelsDonePayload {
    pub bid_levels: Vec<(i16, f64)>,
    pub ask_levels: Vec<(i16, f64)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MakerBookSnapshot {
    pub version: u8,
    pub market: String,
    pub ts: u64,
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MakerBookDelta {
    pub version: u8,
    pub ts: u64,
    pub changes: Vec<BookChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookChange {
    pub price_cents: i16,
    pub side: Side,
    pub new_size: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MakerFillPayload {
    pub ts: u64,
    pub order_id: String,
    pub side: Side,
    pub price_cents: i16,
    pub filled_count: u32,
    pub market: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorEventPayload {
    pub version: u8,
    pub ts: u64,
    pub venue: Venue,
    pub kind: String,
    pub http_status: Option<u16>,
    pub message: String,
    pub retryable: bool,
    pub context: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbortFatalPayload {
    pub version: u8,
    pub ts: u64,
    pub reason_code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CascadeOrder {
    pub level_price_cents: i16,
    pub side: Side,
    pub limit_price_cents: i16,
    pub qty: u32,
    pub initial_taker_vol: f64,
    pub initial_maker_vol: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CascadeOrders {
    pub version: u8,
    pub ts: u64,
    pub market: String,
    pub orders: Vec<CascadeOrder>,
    #[serde(default, alias = "poly_book")]
    pub taker_book: Option<TakerFullBookPayload>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum LevelAction {
    Amend,
    Cancel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LevelUpdateItem {
    pub level_price_cents: i16,
    pub action: LevelAction,
    pub new_qty: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LevelUpdate {
    pub version: u8,
    pub ts: u64,
    pub market: String,
    pub updates: Vec<LevelUpdateItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MakerTouchChanged {
    pub version: u8,
    pub ts: u64,
    pub side: Side,
    pub new_k_cents: i16,
    pub new_k_qty: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TouchCascadePivot {
    pub version: u8,
    pub ts: u64,
    pub market: String,
    pub side: Side,
    pub drop_level_price_cents: i16,
    pub new_order: CascadeOrder,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ArbMsg {
    MakerBookSnapshot(MakerBookSnapshot),
    MakerBookDelta(MakerBookDelta),
    CascadeOrders(CascadeOrders),
    LevelUpdate(LevelUpdate),
    MakerLevelsDone(MakerLevelsDonePayload),
    MakerFill(MakerFillPayload),
    MakerTouchChanged(MakerTouchChanged),
    TouchCascadePivot(TouchCascadePivot),
    ErrorEvent(ErrorEventPayload),
    AbortFatal(AbortFatalPayload),
    TakerFullBook(TakerFullBookPayload),
    TakerLevelVolUpdate(TakerLevelVolUpdatePayload),
    Abort { reason: String },
    MakerAbort { reason: String },
}

// Legacy names used in a few modules during migration.
pub type PolyFullBookPayload = TakerFullBookPayload;
pub type PolyLevelVolUpdatePayload = TakerLevelVolUpdatePayload;
