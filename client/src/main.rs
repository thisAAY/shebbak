use anyhow::{Context, Result};
use srw_client::coordinator::{CoordEvent, CoordinatorApp, HelperManager};
use srw_client::router::VideoRouter;
use srw_client::{bundle, net};
use std::path::PathBuf;
use std::sync::Arc;
use winit::event_loop::EventLoop;

fn main() -> Result<()> {
    if matches!(std::env::args().nth(1).as_deref(), Some("--version" | "-V")) {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let signal_url =
        std::env::var("SRW_HOST").unwrap_or_else(|_| "http://127.0.0.1:9009/offer".to_string());

    let event_loop: EventLoop<()> = EventLoop::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let _ = proxy.send_event(());
    });

    let (coord_tx, coord_rx) = std::sync::mpsc::channel::<CoordEvent>();
    let router = VideoRouter::new();

    let on_event = {
        let coord_tx = coord_tx.clone();
        let wake = wake.clone();
        Arc::new(move |ev| {
            let _ = coord_tx.send(CoordEvent::Net(ev));
            wake();
        }) as Arc<dyn Fn(net::UiEvent) + Send + Sync>
    };
    let net = match net::connect(&signal_url, on_event, router.clone()) {
        Ok(net) => net,
        Err(e) => {
            eprintln!("{e:#}");
            std::process::exit(1);
        }
    };

    let mirrors_dir = bundle::mirrors_dir();
    std::fs::create_dir_all(&mirrors_dir).context("create Mirrors cache dir")?;
    let manager = HelperManager::new(
        router,
        net.sender(),
        coord_tx,
        wake,
        helper_bin_path()?,
        mirrors_dir,
    );

    // `net` must outlive the event loop (tokio runtime + peer are RAII).
    let _net = net;
    let mut app = CoordinatorApp::new(coord_rx, manager);
    event_loop.run_app(&mut app)?;
    if app.disconnected() {
        std::process::exit(1);
    }
    Ok(())
}

/// The helper binary ships next to the coordinator (target/ in dev, the
/// release archive in prod).
fn helper_bin_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("current_exe")?;
    let dir = exe.parent().context("exe has no parent dir")?;
    let helper = dir.join("srw-mirror-helper");
    anyhow::ensure!(
        helper.exists(),
        "srw-mirror-helper not found next to srw-client at {}",
        helper.display()
    );
    Ok(helper)
}
