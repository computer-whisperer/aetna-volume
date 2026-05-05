use anyhow::Result;
use pipewire as pw;
use std::{
    sync::{Arc, Condvar, Mutex, Once},
    thread,
    time::Duration,
};

use crate::backend::AudioBackend;
use crate::model::{AudioCard, AudioClass, AudioNode, AudioSnapshot, Direction};

/// Native PipeWire backend.
///
/// Holds a long-lived registry connection on a dedicated thread and
/// publishes every `global` / `global_remove` event into a shared
/// `AudioSnapshot`. The main thread reads via [`refresh`], which is just
/// a mutex-guarded clone — there is no per-call PipeWire round-trip.
pub struct PipeWireBackend {
    snapshot: Arc<Mutex<AudioSnapshot>>,
    _thread: thread::JoinHandle<()>,
}

impl PipeWireBackend {
    pub fn new() -> Self {
        let snapshot = Arc::new(Mutex::new(AudioSnapshot {
            server_name: Some("PipeWire".into()),
            ..AudioSnapshot::default()
        }));
        let ready = Arc::new((Mutex::new(false), Condvar::new()));

        let snapshot_for_thread = snapshot.clone();
        let ready_for_thread = ready.clone();
        let thread = thread::Builder::new()
            .name("aetna-volume-pipewire".into())
            .spawn(move || {
                if let Err(err) = run_backend_loop(snapshot_for_thread.clone(), &ready_for_thread)
                {
                    eprintln!("aetna-volume: PipeWire backend stopped: {err}");
                    if let Ok(mut snap) = snapshot_for_thread.lock() {
                        snap.error = Some(err.to_string());
                    }
                    signal_ready(&ready_for_thread);
                }
            })
            .expect("spawn PipeWire backend thread");

        // Block briefly for the initial registry walk so the first
        // frame after construction renders against a populated graph
        // rather than an empty placeholder. If PipeWire is hung or
        // unreachable we time out and let the UI render whatever
        // partial state arrived.
        wait_for_ready(&ready, Duration::from_millis(500));

        Self {
            snapshot,
            _thread: thread,
        }
    }
}

impl Default for PipeWireBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioBackend for PipeWireBackend {
    fn refresh(&self) -> AudioSnapshot {
        self.snapshot
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }
}

fn run_backend_loop(
    snapshot: Arc<Mutex<AudioSnapshot>>,
    ready: &Arc<(Mutex<bool>, Condvar)>,
) -> Result<()> {
    pipewire_init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry()?;

    let snapshot_for_global = snapshot.clone();
    let snapshot_for_remove = snapshot.clone();

    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            let Ok(mut snap) = snapshot_for_global.lock() else {
                return;
            };
            if let Some(node) = audio_node_from_global(global) {
                if !snap.nodes.iter().any(|existing| existing.id == node.id) {
                    snap.nodes.push(node);
                }
            } else if let Some(card) = audio_card_from_global(global) {
                if !snap.cards.iter().any(|existing| existing.id == card.id) {
                    snap.cards.push(card);
                }
            }
        })
        .global_remove(move |id| {
            let Ok(mut snap) = snapshot_for_remove.lock() else {
                return;
            };
            snap.nodes.retain(|n| n.id != id);
            snap.cards.retain(|c| c.id != id);
        })
        .register();

    // Sync the core to know when the initial registry walk is complete,
    // then unblock the main thread waiting in `new()`.
    let pending = core.sync(0)?;
    let ready_for_done = ready.clone();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                signal_ready(&ready_for_done);
            }
        })
        .register();

    mainloop.run();
    Ok(())
}

fn signal_ready(ready: &Arc<(Mutex<bool>, Condvar)>) {
    let (lock, cvar) = &**ready;
    if let Ok(mut flag) = lock.lock() {
        *flag = true;
        cvar.notify_all();
    }
}

fn wait_for_ready(ready: &Arc<(Mutex<bool>, Condvar)>, timeout: Duration) {
    let (lock, cvar) = &**ready;
    let Ok(flag) = lock.lock() else {
        return;
    };
    let _ = cvar.wait_timeout_while(flag, timeout, |ready| !*ready);
}

fn pipewire_init() {
    static INIT: Once = Once::new();
    INIT.call_once(pw::init);
}

fn audio_node_from_global<P>(global: &pw::registry::GlobalObject<P>) -> Option<AudioNode>
where
    P: AsRef<pw::spa::utils::dict::DictRef>,
{
    if global.type_ != pw::types::ObjectType::Node {
        return None;
    }
    let props = global.props.as_ref()?.as_ref();
    if is_internal_aetna_node(props) {
        return None;
    }
    let media_class = prop(props, "media.class")?;
    let class = match media_class {
        "Audio/Sink" => AudioClass::Device {
            direction: Direction::Output,
        },
        "Audio/Source" => AudioClass::Device {
            direction: Direction::Input,
        },
        "Stream/Output/Audio" => AudioClass::Stream {
            direction: Direction::Output,
        },
        "Stream/Input/Audio" => AudioClass::Stream {
            direction: Direction::Input,
        },
        other => AudioClass::Other(other.to_string()),
    };

    if matches!(class, AudioClass::Other(_)) {
        return None;
    }

    let name = prop(props, "node.name").unwrap_or("unnamed").to_string();
    let description = prop(props, "node.description")
        .or_else(|| prop(props, "node.nick"))
        .or_else(|| prop(props, "application.name"))
        .or_else(|| prop(props, "media.name"))
        .unwrap_or(&name)
        .to_string();

    Some(AudioNode {
        id: global.id,
        class,
        name,
        description,
        application: prop(props, "application.name").map(str::to_string),
        media_name: prop(props, "media.name").map(str::to_string),
        target: prop(props, "target.object")
            .or_else(|| prop(props, "node.target"))
            .map(str::to_string),
        volume: None,
        is_default: false,
    })
}

fn audio_card_from_global<P>(global: &pw::registry::GlobalObject<P>) -> Option<AudioCard>
where
    P: AsRef<pw::spa::utils::dict::DictRef>,
{
    if global.type_ != pw::types::ObjectType::Device {
        return None;
    }
    let props = global.props.as_ref()?.as_ref();
    let media_class = prop(props, "media.class").unwrap_or_default();
    if media_class != "Audio/Device" {
        return None;
    }

    let name = prop(props, "device.name").unwrap_or("unnamed").to_string();
    let description = prop(props, "device.description")
        .or_else(|| prop(props, "device.nick"))
        .unwrap_or(&name)
        .to_string();

    Some(AudioCard {
        id: global.id,
        name,
        description,
        active_profile: None,
        profiles: Vec::new(),
    })
}

fn prop<'a>(props: &'a pw::spa::utils::dict::DictRef, key: &str) -> Option<&'a str> {
    props
        .iter()
        .find_map(|(k, v)| if k == key { Some(v) } else { None })
}

fn is_internal_aetna_node(props: &pw::spa::utils::dict::DictRef) -> bool {
    prop(props, "node.name")
        .map(|name| name.starts_with("aetna-volume.meter."))
        .unwrap_or(false)
        || prop(props, "application.name")
            .map(|name| name == "aetna-volume")
            .unwrap_or(false)
}
