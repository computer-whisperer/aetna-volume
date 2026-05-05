use aetna_core::Rect;
use aetna_volume::{app::VolumeApp, backend::pipewire_native::PipeWireBackend};

mod host;
use host::run_volume_app;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let viewport = Rect::new(0.0, 0.0, 980.0, 680.0);
    run_volume_app(
        "Aetna Volume",
        viewport,
        VolumeApp::new(Box::new(PipeWireBackend::new())),
    )
}
