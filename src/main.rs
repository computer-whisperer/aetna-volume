use aetna_core::Rect;
use aetna_volume::{app::VolumeApp, backend::pipewire_native::PipeWireBackend};

mod host;
use host::run_volume_app;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Default to a 50% slice of a 1080p panel — the typical placement on the
    // user's secondary monitor. Window managers reflow this freely, but it's
    // what we polish against.
    let viewport = Rect::new(0.0, 0.0, 960.0, 1080.0);
    run_volume_app(
        "Aetna Volume",
        viewport,
        VolumeApp::new(Box::new(PipeWireBackend::new())),
    )
}
