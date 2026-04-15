//! TUI scaffold. A `biased` `tokio::select!` loop over bounded channels
//! feeding a ratatui frame builder. v1 runs against the MockAdapter so the
//! render + input paths can be smoke-tested without API keys.

pub mod app;
pub mod render;
pub mod widgets;
pub mod input;

pub fn run(resume: Option<String>) -> std::io::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(app::run_app(resume))
}
