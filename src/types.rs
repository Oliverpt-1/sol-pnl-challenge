/// One balance observation for a wallet at a specific transaction.
#[derive(Debug, Clone)]
pub struct BalancePoint {
    pub slot: u64,
    pub block_time: Option<i64>,
    pub pre_lamports: u64,
    pub post_lamports: u64,
    pub signature: String,
}

impl BalancePoint {
    pub fn delta_lamports(&self) -> i128 {
        self.post_lamports as i128 - self.pre_lamports as i128
    }
    pub fn post_sol(&self) -> f64 {
        self.post_lamports as f64 / 1e9
    }
}
