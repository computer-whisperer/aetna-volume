use aetna_core::Rect;
use aetna_volume::{app::VolumeApp, backend::pipewire_native::PipeWireBackend};
use aetna_winit_wgpu::{HostConfig, run_with_config};
use std::time::Duration;

const METER_FRAME_INTERVAL: Duration = Duration::from_millis(33);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Default to a 50% slice of a 1080p panel — the typical placement on the
    // user's secondary monitor. Window managers reflow this freely, but it's
    // what we polish against.
    let viewport = Rect::new(0.0, 0.0, 960.0, 1080.0);
    run_with_config(
        "Aetna Volume",
        viewport,
        VolumeApp::new(Box::new(PipeWireBackend::new())),
        HostConfig::default().with_redraw_interval(METER_FRAME_INTERVAL),
    )
}
