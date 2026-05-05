use anyhow::Result;
use pipewire as pw;
use pw::spa::pod::{Object, Property, Value};
use pw::spa::{
    param::ParamType,
    utils::SpaTypes,
};
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::{Arc, Condvar, Mutex, Once},
    thread,
    time::Duration,
};

use crate::backend::AudioBackend;
use crate::model::{AudioCard, AudioClass, AudioNode, AudioSnapshot, Direction};

/// Commands sent from the main thread to the PipeWire backend thread
/// over [`pw::channel`] (loop-integrated, fires the receiver callback
/// the next time the mainloop wakes).
enum BackendCommand {
    SetMute { node_id: u32, muted: bool },
    SetVolume { node_id: u32, scalar: f32 },
    Quit,
}

/// Native PipeWire backend.
///
/// Holds a long-lived registry connection on a dedicated thread and
/// publishes every `global` / `global_remove` event into a shared
/// `AudioSnapshot`. The main thread reads via [`refresh`], which is just
/// a mutex-guarded clone — there is no per-call PipeWire round-trip.
pub struct PipeWireBackend {
    snapshot: Arc<Mutex<AudioSnapshot>>,
    commands: pw::channel::Sender<BackendCommand>,
    _thread: thread::JoinHandle<()>,
}

impl PipeWireBackend {
    pub fn new() -> Self {
        let snapshot = Arc::new(Mutex::new(AudioSnapshot {
            server_name: Some("PipeWire".into()),
            ..AudioSnapshot::default()
        }));
        let ready = Arc::new((Mutex::new(false), Condvar::new()));
        let (commands_tx, commands_rx) = pw::channel::channel::<BackendCommand>();

        let snapshot_for_thread = snapshot.clone();
        let ready_for_thread = ready.clone();
        let thread = thread::Builder::new()
            .name("aetna-volume-pipewire".into())
            .spawn(move || {
                if let Err(err) = run_backend_loop(
                    snapshot_for_thread.clone(),
                    commands_rx,
                    &ready_for_thread,
                ) {
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
            commands: commands_tx,
            _thread: thread,
        }
    }
}

impl Default for PipeWireBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PipeWireBackend {
    fn drop(&mut self) {
        // Best-effort: ask the backend thread to stop so the OS doesn't
        // have to reap a still-running mainloop on process exit. If the
        // send fails the thread is already gone.
        let _ = self.commands.send(BackendCommand::Quit);
    }
}

impl AudioBackend for PipeWireBackend {
    fn refresh(&self) -> AudioSnapshot {
        self.snapshot
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    fn set_mute(&self, node_id: u32, muted: bool) {
        let _ = self
            .commands
            .send(BackendCommand::SetMute { node_id, muted });
    }

    fn set_volume(&self, node_id: u32, scalar: f32) {
        let _ = self
            .commands
            .send(BackendCommand::SetVolume { node_id, scalar });
    }
}

fn run_backend_loop(
    snapshot: Arc<Mutex<AudioSnapshot>>,
    commands_rx: pw::channel::Receiver<BackendCommand>,
    ready: &Arc<(Mutex<bool>, Condvar)>,
) -> Result<()> {
    pipewire_init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    // Per-node Node proxies, populated as we see globals and dropped
    // as they go away. Used by the command receiver to issue
    // `set_param` calls for mute / volume.
    let proxies: Rc<RefCell<HashMap<u32, pw::node::Node>>> =
        Rc::new(RefCell::new(HashMap::new()));

    let snapshot_for_global = snapshot.clone();
    let snapshot_for_remove = snapshot.clone();
    let proxies_for_global = proxies.clone();
    let proxies_for_remove = proxies.clone();
    let registry_for_bind = registry.clone();

    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if let Ok(mut snap) = snapshot_for_global.lock() {
                if let Some(node) = audio_node_from_global(global) {
                    if !snap.nodes.iter().any(|existing| existing.id == node.id) {
                        snap.nodes.push(node);
                    }
                } else if let Some(card) = audio_card_from_global(global) {
                    if !snap.cards.iter().any(|existing| existing.id == card.id) {
                        snap.cards.push(card);
                    }
                }
            }

            if global.type_ == pw::types::ObjectType::Node {
                if let Some(props) = global.props.as_ref() {
                    if is_internal_aetna_node(props.as_ref()) {
                        return;
                    }
                }
                match registry_for_bind.bind::<pw::node::Node, _>(global) {
                    Ok(node) => {
                        proxies_for_global.borrow_mut().insert(global.id, node);
                    }
                    Err(err) => eprintln!(
                        "aetna-volume: failed to bind node {}: {err}",
                        global.id
                    ),
                }
            }
        })
        .global_remove(move |id| {
            if let Ok(mut snap) = snapshot_for_remove.lock() {
                snap.nodes.retain(|n| n.id != id);
                snap.cards.retain(|c| c.id != id);
            }
            proxies_for_remove.borrow_mut().remove(&id);
        })
        .register();

    let proxies_for_commands = proxies.clone();
    let mainloop_for_quit = mainloop.clone();
    let _commands_attached = commands_rx.attach(mainloop.loop_(), move |cmd| match cmd {
        BackendCommand::Quit => mainloop_for_quit.quit(),
        BackendCommand::SetMute { node_id, muted } => {
            apply_mute(&proxies_for_commands.borrow(), node_id, muted);
        }
        BackendCommand::SetVolume { node_id, scalar } => {
            apply_volume(&proxies_for_commands.borrow(), node_id, scalar);
        }
    });

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

fn apply_mute(proxies: &HashMap<u32, pw::node::Node>, node_id: u32, muted: bool) {
    let Some(node) = proxies.get(&node_id) else {
        return;
    };
    let pod = match build_props_pod(vec![Property::new(
        pw::spa::sys::SPA_PROP_mute,
        Value::Bool(muted),
    )]) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("aetna-volume: failed to build mute pod for {node_id}: {err}");
            return;
        }
    };
    let Some(pod) = pw::spa::pod::Pod::from_bytes(&pod) else {
        eprintln!("aetna-volume: built invalid mute pod for {node_id}");
        return;
    };
    node.set_param(ParamType::Props, 0, pod);
}

fn apply_volume(proxies: &HashMap<u32, pw::node::Node>, node_id: u32, scalar: f32) {
    let Some(node) = proxies.get(&node_id) else {
        return;
    };
    let pod = match build_props_pod(vec![Property::new(
        pw::spa::sys::SPA_PROP_volume,
        Value::Float(scalar.clamp(0.0, 1.5)),
    )]) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("aetna-volume: failed to build volume pod for {node_id}: {err}");
            return;
        }
    };
    let Some(pod) = pw::spa::pod::Pod::from_bytes(&pod) else {
        eprintln!("aetna-volume: built invalid volume pod for {node_id}");
        return;
    };
    node.set_param(ParamType::Props, 0, pod);
}

fn build_props_pod(properties: Vec<Property>) -> Result<Vec<u8>> {
    let obj = Object {
        type_: SpaTypes::ObjectParamProps.as_raw(),
        id: ParamType::Props.as_raw(),
        properties,
    };
    let (cursor, _) = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(obj),
    )?;
    Ok(cursor.into_inner())
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
