pub mod fsx;
pub mod protocol;
pub mod receiver;

pub type SteamboatResult<T> = std::result::Result<T, anyhow::Error>;

pub trait Progress: Send + Sync {
    fn transfer_start(&self, file_count: u64, total_bytes: u64);
    fn file_start(&self, wire_path: &str);
    fn chunk(&self, bytes: u64);
}

pub struct NoProgress;

impl Progress for NoProgress {
    fn transfer_start(&self, _: u64, _: u64) {}
    fn file_start(&self, _: &str) {}
    fn chunk(&self, _: u64) {}
}
