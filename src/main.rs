mod api;
mod app;
mod config;
mod db;
mod interfaces;
mod jobs;
mod library;
mod pricing;
mod providers;
mod templates;

fn main() -> anyhow::Result<()> {
    // unoptimized builds poll the CLI with stack frames larger than Windows' 1 MB main thread
    std::thread::Builder::new()
        .stack_size(8 << 20)
        .spawn(|| tokio::runtime::Runtime::new()?.block_on(Box::pin(interfaces::cli::run())))?
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}
