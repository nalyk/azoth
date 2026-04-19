//! TUI scaffold. A `biased` `tokio::select!` loop over bounded channels
//! feeding a ratatui frame builder. The active provider is selected via
//! `AZOTH_PROFILE` (default: `ollama-qwen-anthropic`).

pub mod app;
pub mod card;
pub mod config;
pub mod input;
pub mod inspector;
pub mod markdown;
pub mod motion;
pub mod palette;
pub mod rail;
pub mod render;
pub mod sheet;
pub mod splash;
pub mod theme;
pub mod util;
pub mod whisper;

pub fn run(resume: Option<String>) -> std::io::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(app::run_app(resume))
}
