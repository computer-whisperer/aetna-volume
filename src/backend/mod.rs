use crate::model::AudioSnapshot;

pub mod pipewire_native;

/// Read-only access to the latest known PipeWire graph snapshot.
///
/// Implementations are expected to maintain the snapshot reactively
/// (a background thread driving a PipeWire registry listener), so
/// `refresh` is a cheap clone of shared state — safe to call once per
/// redraw.
pub trait AudioBackend {
    fn refresh(&self) -> AudioSnapshot;
}

#[derive(Default)]
#[allow(dead_code)]
pub struct DemoBackend;

impl AudioBackend for DemoBackend {
    fn refresh(&self) -> AudioSnapshot {
        AudioSnapshot::demo()
    }
}
