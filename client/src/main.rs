mod app;
mod net;

use anyhow::Result;
use winit::event_loop::EventLoop;

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "info".into()),
    ).init();

    let signal_url = std::env::var("SRW_HOST")
        .unwrap_or_else(|_| "http://127.0.0.1:9009/offer".to_string());

    let event_loop: EventLoop<()> = EventLoop::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let (ui_tx, ui_rx) = std::sync::mpsc::channel();
    let wake = move || {
        let _ = proxy.send_event(());
    };

    let net = match net::connect(&signal_url, ui_tx, wake) {
        Ok(net) => net,
        Err(e) => {
            eprintln!("{e:#}");
            std::process::exit(1);
        }
    };
    let mut app = app::App::new(net, ui_rx);
    event_loop.run_app(&mut app)?;
    // run_app returned. Zero mirrors is a valid idle state (app sharing can
    // revive it), so the only exit paths are UiEvent::Disconnected (handled
    // below) and Cmd+Q, which Task 19 wires up.
    if app.disconnected() {
        std::process::exit(1);
    }
    Ok(())
}
