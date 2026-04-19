pub struct PolicyDecision {
    pub action: Action,
    pub q_value: f64,
    pub confidence: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Buy(u64),
    Sell(u64),
    Hold,
}
