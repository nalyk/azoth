//! TUI scaffold. A `biased` `tokio::select!` loop over bounded channels
//! feeding a ratatui frame builder. The active provider is selected via
//! `AZOTH_PROFILE` (default: `ollama-qwen-anthropic`).

pub mod app;
pub mod config;
pub mod render;
pub mod widgets;
pub mod input;

pub fn run(resume: Option<String>) -> std::io::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(app::run_app(resume))
}
