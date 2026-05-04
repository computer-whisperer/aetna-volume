use pipewire as pw;
use pw::{properties::properties, spa};
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::pod::Pod;
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    mem,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, Once,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use crate::model::{AudioClass, AudioNode, AudioSnapshot, Direction};

#[derive(Debug, Clone, Default)]
pub struct NodeLevels {
    pub peaks: Vec<f32>,
    pub rms: Vec<f32>,
}

impl NodeLevels {
    pub fn channel_count(&self) -> usize {
        self.peaks.len().max(self.rms.len())
    }

    pub fn peak(&self, channel: usize) -> f32 {
        self.peaks.get(channel).copied().unwrap_or(0.0)
    }

    pub fn rms(&self, channel: usize) -> f32 {
        self.rms.get(channel).copied().unwrap_or(0.0)
    }
}

#[derive(Default)]
pub struct LevelService {
    levels: Arc<Mutex<HashMap<u32, NodeLevels>>>,
    meters: HashMap<u32, MeterHandle>,
}

impl LevelService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn ensure_snapshot(&mut self, snapshot: &AudioSnapshot) {
        let wanted = snapshot
            .nodes
            .iter()
            .filter(|node| meter_route_for(node).is_some())
            .map(|node| node.id)
            .collect::<HashSet<_>>();
        self.meters.retain(|node_id, meter| {
            let keep = wanted.contains(node_id);
            if !keep {
                meter.stop.store(true, Ordering::Relaxed);
            }
            keep
        });
        if let Ok(mut levels) = self.levels.lock() {
            levels.retain(|node_id, _| wanted.contains(node_id));
        }
        for node in &snapshot.nodes {
            self.ensure_node(node);
        }
    }

    pub fn level_for(&self, node_id: u32) -> Option<NodeLevels> {
        self.levels
            .lock()
            .ok()
            .and_then(|levels| levels.get(&node_id).cloned())
    }

    pub fn active_meter_count(&self) -> usize {
        self.meters.len()
    }

    fn ensure_node(&mut self, node: &AudioNode) {
        if self.meters.contains_key(&node.id) {
            return;
        }
        let Some(route) = meter_route_for(node) else {
            return;
        };
        let stop = Arc::new(AtomicBool::new(false));
        let child = spawn_meter(node.id, route, self.levels.clone(), stop.clone());
        self.meters.insert(node.id, MeterHandle { stop, child });
    }
}

impl Drop for LevelService {
    fn drop(&mut self) {
        for meter in self.meters.values() {
            meter.stop.store(true, Ordering::Relaxed);
            if let Some(child) = &meter.child
                && let Ok(mut child) = child.lock()
            {
                let _ = child.kill();
            }
        }
    }
}

struct MeterHandle {
    stop: Arc<AtomicBool>,
    child: Option<Arc<Mutex<Child>>>,
}

struct MeterData {
    node_id: u32,
    format: spa::param::audio::AudioInfoRaw,
    mainloop: pw::main_loop::MainLoopRc,
    levels: Arc<Mutex<HashMap<u32, NodeLevels>>>,
    stop: Arc<AtomicBool>,
    smooth_peaks: Vec<f32>,
    smooth_rms: Vec<f32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MeterRoute {
    PipeWire { capture_sink: bool },
    PulseMonitor { stream_id: u32 },
}

fn meter_route_for(node: &AudioNode) -> Option<MeterRoute> {
    match node.class {
        AudioClass::Device {
            direction: Direction::Output,
        } => Some(MeterRoute::PipeWire { capture_sink: true }),
        AudioClass::Device {
            direction: Direction::Input,
        } => Some(MeterRoute::PipeWire {
            capture_sink: false,
        }),
        AudioClass::Stream {
            direction: Direction::Output,
        } => node
            .monitor_stream_id
            .map(|stream_id| MeterRoute::PulseMonitor { stream_id }),
        AudioClass::Stream {
            direction: Direction::Input,
        } => Some(MeterRoute::PipeWire {
            capture_sink: false,
        }),
        _ => None,
    }
}

fn spawn_meter(
    node_id: u32,
    route: MeterRoute,
    levels: Arc<Mutex<HashMap<u32, NodeLevels>>>,
    stop: Arc<AtomicBool>,
) -> Option<Arc<Mutex<Child>>> {
    match route {
        MeterRoute::PipeWire { capture_sink } => {
            thread::Builder::new()
                .name(format!("aetna-volume-meter-{node_id}"))
                .spawn(move || {
                    if let Err(err) = run_pipewire_meter(node_id, capture_sink, levels, stop) {
                        eprintln!("aetna-volume: level meter for node {node_id} stopped: {err}");
                    }
                })
                .expect("spawn PipeWire level meter");
            None
        }
        MeterRoute::PulseMonitor { stream_id } => {
            let child = spawn_pulse_meter_process(stream_id)
                .map(|child| Arc::new(Mutex::new(child)))
                .ok();
            let child_for_thread = child.clone();
            thread::Builder::new()
                .name(format!("aetna-volume-pulse-meter-{node_id}"))
                .spawn(move || {
                    if let Some(child) = child_for_thread {
                        run_pulse_meter(node_id, child, levels, stop);
                    }
                })
                .expect("spawn Pulse monitor meter");
            child
        }
    }
}

fn run_pipewire_meter(
    node_id: u32,
    capture_sink: bool,
    levels: Arc<Mutex<HashMap<u32, NodeLevels>>>,
    stop: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    pipewire_init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let mut props = properties! {
        *pw::keys::APP_NAME => "aetna-volume",
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::NODE_NAME => format!("aetna-volume.meter.{node_id}"),
        "target.object" => node_id.to_string(),
    };
    if capture_sink {
        props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    }

    let stream = pw::stream::StreamBox::new(&core, "aetna-volume-meter", props)?;
    let data = MeterData {
        node_id,
        format: Default::default(),
        mainloop: mainloop.clone(),
        levels,
        stop,
        smooth_peaks: Vec::new(),
        smooth_rms: Vec::new(),
    };

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, data, _, state| {
            if let pw::stream::StreamState::Error(err) = state {
                eprintln!(
                    "aetna-volume: meter stream error for node {}: {err}",
                    data.node_id
                );
                data.mainloop.quit();
            }
        })
        .param_changed(|_, data, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
                return;
            };
            if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                return;
            }
            let _ = data.format.parse(param);
        })
        .process(|stream, data| {
            if data.stop.load(Ordering::Relaxed) {
                data.mainloop.quit();
                return;
            }
            process_meter_buffer(stream, data);
        })
        .register()?;

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).ok_or("failed to build PipeWire format pod")?];

    stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    mainloop.run();
    Ok(())
}

fn spawn_pulse_meter_process(stream_id: u32) -> std::io::Result<Child> {
    Command::new("parec")
        .args([
            "--raw",
            "--format=float32le",
            "--channels=2",
            "--rate=48000",
            "--device=@DEFAULT_MONITOR@",
            "--latency-msec=50",
            "--process-time-msec=25",
            "--client-name=aetna-volume",
            "--stream-name=aetna-volume-meter",
            &format!("--monitor-stream={stream_id}"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

fn run_pulse_meter(
    node_id: u32,
    child: Arc<Mutex<Child>>,
    levels: Arc<Mutex<HashMap<u32, NodeLevels>>>,
    stop: Arc<AtomicBool>,
) {
    let Some(mut stdout) = child.lock().ok().and_then(|mut child| child.stdout.take()) else {
        return;
    };
    let mut smooth_peaks = Vec::new();
    let mut smooth_rms = Vec::new();
    let mut buf = vec![0_u8; 4096];
    while !stop.load(Ordering::Relaxed) {
        let Ok(read) = stdout.read(&mut buf) else {
            break;
        };
        if read == 0 {
            break;
        }
        publish_level_samples(
            node_id,
            &buf[..read],
            2,
            &levels,
            &mut smooth_peaks,
            &mut smooth_rms,
        );
    }
}

fn process_meter_buffer(stream: &pw::stream::Stream, data: &mut MeterData) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let datas = buffer.datas_mut();
    if datas.is_empty() {
        return;
    }
    let chunk = datas[0].chunk();
    let bytes = chunk.size() as usize;
    let channels = data.format.channels() as usize;
    if channels == 0 || bytes == 0 {
        return;
    }
    let Some(samples) = datas[0].data() else {
        return;
    };
    let sample_count = (bytes / mem::size_of::<f32>()).min(samples.len() / 4);
    if sample_count == 0 {
        return;
    }

    publish_level_samples(
        data.node_id,
        &samples[..bytes.min(samples.len())],
        channels,
        &data.levels,
        &mut data.smooth_peaks,
        &mut data.smooth_rms,
    );
}

fn publish_level_samples(
    node_id: u32,
    samples: &[u8],
    channels: usize,
    levels: &Arc<Mutex<HashMap<u32, NodeLevels>>>,
    smooth_peaks: &mut Vec<f32>,
    smooth_rms: &mut Vec<f32>,
) {
    if channels == 0 || samples.len() < 4 {
        return;
    }
    let sample_count = samples.len() / 4;
    if sample_count == 0 {
        return;
    }

    let mut peaks = vec![0.0_f32; channels];
    let mut sums = vec![0.0_f32; channels];
    let mut counts = vec![0_u32; channels];
    for sample_index in 0..sample_count {
        let start = sample_index * 4;
        let Ok(bytes) = samples[start..start + 4].try_into() else {
            continue;
        };
        let value = f32::from_le_bytes(bytes);
        let channel = sample_index % channels;
        let abs = value.abs();
        peaks[channel] = peaks[channel].max(abs);
        sums[channel] += value * value;
        counts[channel] += 1;
    }

    resize_smoothing(smooth_peaks, smooth_rms, channels);
    for channel in 0..channels {
        let rms = if counts[channel] == 0 {
            0.0
        } else {
            (sums[channel] / counts[channel] as f32).sqrt()
        };
        smooth_peaks[channel] = smooth(smooth_peaks[channel], peaks[channel], 0.70);
        smooth_rms[channel] = smooth(smooth_rms[channel], rms, 0.82);
    }

    if let Ok(mut levels) = levels.try_lock() {
        levels.insert(
            node_id,
            NodeLevels {
                peaks: smooth_peaks.clone(),
                rms: smooth_rms.clone(),
            },
        );
    }
}

fn resize_smoothing(smooth_peaks: &mut Vec<f32>, smooth_rms: &mut Vec<f32>, channels: usize) {
    if smooth_peaks.len() != channels {
        smooth_peaks.resize(channels, 0.0);
    }
    if smooth_rms.len() != channels {
        smooth_rms.resize(channels, 0.0);
    }
}

fn smooth(previous: f32, next: f32, release: f32) -> f32 {
    if next >= previous {
        next
    } else {
        previous * release + next * (1.0 - release)
    }
}

fn pipewire_init() {
    static INIT: Once = Once::new();
    INIT.call_once(pw::init);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoothing_attacks_immediately_and_releases_slowly() {
        assert_eq!(smooth(0.2, 0.8, 0.7), 0.8);
        let released = smooth(0.8, 0.2, 0.7);
        assert!(released > 0.2);
        assert!(released < 0.8);
    }

    #[test]
    fn routes_use_sink_mix_only_for_output_devices() {
        let output_device = test_node(AudioClass::Device {
            direction: Direction::Output,
        });
        let mut output_stream = test_node(AudioClass::Stream {
            direction: Direction::Output,
        });
        output_stream.monitor_stream_id = Some(42);
        let input_device = test_node(AudioClass::Device {
            direction: Direction::Input,
        });
        assert_eq!(
            meter_route_for(&output_device),
            Some(MeterRoute::PipeWire { capture_sink: true })
        );
        assert_eq!(
            meter_route_for(&output_stream),
            Some(MeterRoute::PulseMonitor { stream_id: 42 })
        );
        assert_eq!(
            meter_route_for(&input_device),
            Some(MeterRoute::PipeWire {
                capture_sink: false
            })
        );
    }

    #[test]
    fn output_stream_without_pulse_monitor_id_is_not_metered_from_sink_mix() {
        let output_stream = test_node(AudioClass::Stream {
            direction: Direction::Output,
        });
        assert_eq!(meter_route_for(&output_stream), None);
    }

    fn test_node(class: AudioClass) -> AudioNode {
        AudioNode {
            id: 1,
            class,
            name: "test".into(),
            description: "test".into(),
            application: None,
            media_name: None,
            target: None,
            volume: None,
            is_default: false,
            monitor_stream_id: None,
        }
    }
}
