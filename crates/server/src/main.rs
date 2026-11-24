//! `bf-server`: the app's backend without the desktop shell, for development (`npm run dev`) and measurements. The
//! desktop app (desktop/tauri) runs the same server in its own process.
//!
//!   cargo run -p bf-server -- [--port 5184]

use bf_server::serve::{default_web_dir, run, ServeOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut port: Option<u16> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--port" {
            port = args.next().and_then(|p| p.parse().ok());
        }
    }
    let started = std::time::Instant::now();
    let server = run(ServeOptions {
        port,
        any_port: false,
        web_dir: Some(default_web_dir()),
        starting: Box::new(|t| eprintln!("[start] {t}")),
        handles: None,
    })
    .await?;
    eprintln!(
        "[api] http://127.0.0.1:{} listening after {} ms (dashboard: http://localhost:5173 in development)",
        server.port,
        started.elapsed().as_millis()
    );
    server.done.await??;
    Ok(())
}
