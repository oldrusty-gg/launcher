//! The Profile page's account jobs ([`platform_net::profile`]): an
//! account-only connection to the online server of the game whose manifest
//! names one, greeted with that manifest's `online.account_hello`; no game
//! crate is involved.

pub use platform_net::profile::*;

/// What the launcher calls itself in its hello.
pub const CLIENT: &str = concat!("roses-launcher ", env!("CARGO_PKG_VERSION"));

/// The Profile page's paths for `data_root`, greeting the server the way
/// the first game with an online server asks (its `online.account_hello`).
pub fn paths_for(data_root: &std::path::Path, games: &[crate::registry::Game]) -> Paths {
    let hello = games
        .iter()
        .find_map(|game| game.manifest.online.as_ref()?.account_hello.clone())
        .unwrap_or_default();
    Paths::for_data_root(data_root).greeting(CLIENT, hello)
}
