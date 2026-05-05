use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tab {
    Playback,
    Recording,
    Outputs,
    Inputs,
    Configuration,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Playback,
        Tab::Recording,
        Tab::Outputs,
        Tab::Inputs,
        Tab::Configuration,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Playback => "Playback",
            Tab::Recording => "Recording",
            Tab::Outputs => "Output Devices",
            Tab::Inputs => "Input Devices",
            Tab::Configuration => "Configuration",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Tab::Playback => "tab:playback",
            Tab::Recording => "tab:recording",
            Tab::Outputs => "tab:outputs",
            Tab::Inputs => "tab:inputs",
            Tab::Configuration => "tab:configuration",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Input,
    Output,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioClass {
    Stream { direction: Direction },
    Device { direction: Direction },
    Card,
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Volume {
    /// Linear PipeWire volume scalar. `1.0` is nominal 100%.
    pub scalar: f32,
    pub muted: bool,
}

impl Volume {
    pub fn percent(&self) -> u32 {
        (self.scalar * 100.0).round().clamp(0.0, 999.0) as u32
    }

    pub fn from_percent(percent: u32, muted: bool) -> Self {
        Self {
            scalar: (percent as f32 / 100.0).clamp(0.0, 1.5),
            muted,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioNode {
    pub id: u32,
    pub class: AudioClass,
    pub name: String,
    pub description: String,
    pub application: Option<String>,
    pub media_name: Option<String>,
    pub target: Option<String>,
    pub volume: Option<Volume>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioCard {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub active_profile: Option<String>,
    pub profiles: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AudioSnapshot {
    pub nodes: Vec<AudioNode>,
    pub cards: Vec<AudioCard>,
    pub server_name: Option<String>,
    pub error: Option<String>,
    /// `node.name` of the current default audio sink, as published by
    /// the PipeWire `default` metadata. Compared against
    /// `AudioNode.name` to derive per-row default badges.
    pub default_sink_name: Option<String>,
    /// `node.name` of the current default audio source.
    pub default_source_name: Option<String>,
}

impl AudioSnapshot {
    pub fn is_default(&self, node: &AudioNode) -> bool {
        match node.class {
            AudioClass::Device {
                direction: Direction::Output,
            } => self.default_sink_name.as_deref() == Some(node.name.as_str()),
            AudioClass::Device {
                direction: Direction::Input,
            } => self.default_source_name.as_deref() == Some(node.name.as_str()),
            _ => false,
        }
    }
}

impl AudioSnapshot {
    pub fn demo() -> Self {
        Self {
            server_name: Some("PipeWire".into()),
            default_sink_name: Some("alsa_output.pci-0000_0b_00.4.analog-stereo".into()),
            default_source_name: Some("alsa_input.usb-mic.mono-fallback".into()),
            nodes: vec![
                AudioNode {
                    id: 42,
                    class: AudioClass::Device {
                        direction: Direction::Output,
                    },
                    name: "alsa_output.pci-0000_0b_00.4.analog-stereo".into(),
                    description: "Starship Speakers".into(),
                    application: None,
                    media_name: None,
                    target: Some("Analog Stereo".into()),
                    volume: Some(Volume {
                        scalar: 0.64,
                        muted: false,
                    }),
                },
                AudioNode {
                    id: 56,
                    class: AudioClass::Stream {
                        direction: Direction::Output,
                    },
                    name: "Firefox".into(),
                    description: "Firefox".into(),
                    application: Some("Firefox".into()),
                    media_name: Some("Video playback".into()),
                    target: Some("Starship Speakers".into()),
                    volume: Some(Volume {
                        scalar: 0.82,
                        muted: false,
                    }),
                },
                AudioNode {
                    id: 61,
                    class: AudioClass::Stream {
                        direction: Direction::Output,
                    },
                    name: "Discord".into(),
                    description: "Discord".into(),
                    application: Some("Discord".into()),
                    media_name: Some("Voice call".into()),
                    target: Some("Starship Speakers".into()),
                    volume: Some(Volume {
                        scalar: 0.48,
                        muted: true,
                    }),
                },
                AudioNode {
                    id: 77,
                    class: AudioClass::Device {
                        direction: Direction::Input,
                    },
                    name: "alsa_input.usb-mic.mono-fallback".into(),
                    description: "USB Microphone".into(),
                    application: None,
                    media_name: None,
                    target: Some("Mono Input".into()),
                    volume: Some(Volume {
                        scalar: 0.71,
                        muted: false,
                    }),
                },
            ],
            cards: vec![AudioCard {
                id: 12,
                name: "alsa_card.pci-0000_0b_00.4".into(),
                description: "Built-in Audio".into(),
                active_profile: Some("Analog Stereo Duplex".into()),
                profiles: vec![
                    "Analog Stereo Duplex".into(),
                    "Analog Stereo Output".into(),
                    "Off".into(),
                ],
            }],
            error: None,
        }
    }

    pub fn nodes_for_tab(&self, tab: Tab) -> Vec<&AudioNode> {
        self.nodes
            .iter()
            .filter(|node| match (tab, &node.class) {
                (
                    Tab::Playback,
                    AudioClass::Stream {
                        direction: Direction::Output,
                    },
                ) => true,
                (
                    Tab::Recording,
                    AudioClass::Stream {
                        direction: Direction::Input,
                    },
                ) => true,
                (
                    Tab::Outputs,
                    AudioClass::Device {
                        direction: Direction::Output,
                    },
                ) => true,
                (
                    Tab::Inputs,
                    AudioClass::Device {
                        direction: Direction::Input,
                    },
                ) => true,
                _ => false,
            })
            .collect()
    }

    pub fn node_mut(&mut self, id: u32) -> Option<&mut AudioNode> {
        self.nodes.iter_mut().find(|node| node.id == id)
    }
}
