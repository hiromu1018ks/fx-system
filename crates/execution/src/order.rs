use fx_core::types::Direction;

pub struct OrderCommand {
    pub direction: Direction,
    pub lots: u64,
    pub symbol: String,
}
