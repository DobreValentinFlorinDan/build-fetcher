//! `irelia-server`: the app's backend without the desktop shell (development, CI, the parity harness). The desktop
//! app (desktop/tauri) runs the same server in its own process.

use irelia_server::serve::{run, ServeOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut port: Option<u16> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--port" {
            port = args.next().and_then(|p| p.parse().ok());
        }
    }
    let server = run(ServeOptions { port, any_port: false, starting: Box::new(|t| eprintln!("[start] {t}")) }).await?;
    eprintln!("[api] http://127.0.0.1:{}  (dashboard: http://localhost:5173 in dev)", server.port);
    server.done.await??;
    Ok(())
}
