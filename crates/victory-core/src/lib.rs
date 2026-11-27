//! Shared plumbing for every VIctory crate and tool.
//!
//! * [`config`]: where the user's files live (never inside the repository).
//! * [`policy`]: the runtime half of rule #1, refusing to write
//!   game-derived data into the source tree.
//! * [`logging`]: one tracing setup for all binaries.

pub mod config;
pub mod logging;
pub mod policy;

pub use config::Config;
pub use policy::{PolicyError, ensure_outside_repo};

/// Set once the title is through its loading (no bake of character skin
/// for a while: `victory-gpu`'s frame gate); the loader then gives the
/// title its playing frame rate (`VICTORY_LOAD_FPS`).
pub static LOADING_OVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The title's optional loading frame rate. The loader and GPU frame gate
/// must agree whether the requested rate is valid before either acts on it.
pub fn loading_frame_rate() -> Option<u32> {
    std::env::var("VICTORY_LOAD_FPS").ok().as_deref().and_then(parse_loading_frame_rate)
}

fn parse_loading_frame_rate(value: &str) -> Option<u32> {
    value.trim().parse::<u32>().ok().filter(|rate| (1..1000).contains(rate))
}

#[cfg(test)]
mod loading_frame_rate_tests {
    use super::parse_loading_frame_rate;

    #[test]
    fn accepts_only_rates_the_loader_can_apply() {
        for invalid in ["", "0", "1000", "-1", "5.5", "off"] {
            assert_eq!(parse_loading_frame_rate(invalid), None);
        }
        for (input, rate) in [("1", 1), (" 5 ", 5), ("999", 999)] {
            assert_eq!(parse_loading_frame_rate(input), Some(rate));
        }
    }
}
