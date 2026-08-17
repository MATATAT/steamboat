pub mod fsx;
pub mod protocol;

pub type SteamboatResult<T> = std::result::Result<T, anyhow::Error>;
