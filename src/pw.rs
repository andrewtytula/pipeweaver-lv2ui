//! The PipeWire side: a filter node that runs the shadow instance, and the
//! "tap" that links it to whatever currently feeds the real filter inside
//! Pipeweaver. The tap links belong to this process and are not lingering,
//! so PipeWire removes them by itself when we exit, however we exit.

use crate::shadow::Shadow;
use anyhow::{Result, anyhow};
use pipewire::context::ContextRc;
use pipewire::core::CoreRc;
use pipewire::filter::{FilterFlags, FilterPort, FilterRc, FilterState, PortFlags};
use pipewire::keys::{
    AUDIO_CHANNEL, FORMAT_DSP, LINK_INPUT_NODE, LINK_INPUT_PORT, LINK_OUTPUT_NODE,
    LINK_OUTPUT_PORT, MEDIA_CATEGORY, MEDIA_ROLE, MEDIA_TYPE, NODE_ALWAYS_PROCESS,
    NODE_DESCRIPTION, NODE_ID, NODE_NAME, OBJECT_LINGER, PORT_DIRECTION, PORT_NAME,
};
use pipewire::link::Link;
use pipewire::main_loop::MainLoopRc;
use pipewire::properties::properties;
use pipewire::spa::utils::Direction;
use pipewire::types::ObjectType;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc;

const CHANNELS: [&str; 2] = ["FL", "FR"];

/// How to recognise the real filter's node among Pipeweaver's nodes.
#[derive(Debug, Clone)]
pub struct TapTarget {
    /// `node.name`: the last segment of the plugin URI
    pub node_name: String,
    /// `node.description` starts with this: `pipeweaver/<device>-<plugin>-`
    pub description_prefix: String,
    /// Which match to use if the same plugin sits on the device more than once
    pub ordinal: usize,
}

pub enum PwCommand {
    Quit,
}

#[derive(Debug)]
pub enum PwEvent {
    /// Number of tap links now in place (0 = the real filter was not found, or has no input)
    Tapped(usize),
    /// Something went wrong, but the shadow is still running
    Failed(String),
    /// The shadow is not running and won't be
    Stopped(String),
}

struct Graph {
    core: CoreRc,
    target: Option<TapTarget>,
    events: mpsc::Sender<PwEvent>,
    shadow_node: Option<u32>,
    nodes: HashMap<u32, (String, String)>,
    /// port id -> (node id, is input, port name)
    ports: HashMap<u32, (u32, bool, String)>,
    /// link id -> (output node, output port, input node, input port)
    links: HashMap<u32, (u32, u32, u32, u32)>,
    /// (source output port, our input port) -> the link we made
    taps: HashMap<(u32, u32), Link>,
    reported: Option<usize>,
}

impl Graph {
    fn real_node(&self) -> Option<u32> {
        let target = self.target.as_ref()?;
        let mut matches: Vec<(u32, u32)> = self
            .nodes
            .iter()
            .filter(|(_, (name, description))| {
                *name == target.node_name && description.starts_with(&target.description_prefix)
            })
            .map(|(id, (_, description))| {
                let index = description[target.description_prefix.len()..]
                    .parse()
                    .unwrap_or(u32::MAX);
                (index, *id)
            })
            .collect();
        matches.sort();
        matches
            .get(target.ordinal.min(matches.len().saturating_sub(1)))
            .map(|(_, id)| *id)
    }

    /// Make our input ports mirror the real filter's input links.
    fn reconcile(&mut self) {
        let mut wanted: HashMap<(u32, u32), u32> = HashMap::new();

        if let (Some(real), Some(shadow)) = (self.real_node(), self.shadow_node) {
            for &(out_node, out_port, in_node, in_port) in self.links.values() {
                if in_node != real {
                    continue;
                }
                let Some((_, _, port_name)) = self.ports.get(&in_port) else {
                    continue;
                };
                let ours = self.ports.iter().find(|(_, (node, is_input, name))| {
                    *node == shadow && *is_input && name == port_name
                });
                if let Some((&our_port, _)) = ours {
                    wanted.insert((out_port, our_port), out_node);
                }
            }
        }

        // Dropping a link proxy destroys the (non-lingering) link
        self.taps.retain(|key, _| wanted.contains_key(key));

        let shadow = self.shadow_node.unwrap_or_default();
        for ((out_port, our_port), out_node) in wanted {
            if self.taps.contains_key(&(out_port, our_port)) {
                continue;
            }
            let link = self.core.create_object::<Link>(
                "link-factory",
                &properties! {
                    *LINK_OUTPUT_NODE => out_node.to_string(),
                    *LINK_OUTPUT_PORT => out_port.to_string(),
                    *LINK_INPUT_NODE => shadow.to_string(),
                    *LINK_INPUT_PORT => our_port.to_string(),
                    *OBJECT_LINGER => "false",
                },
            );
            match link {
                Ok(link) => {
                    self.taps.insert((out_port, our_port), link);
                }
                Err(e) => {
                    let _ = self
                        .events
                        .send(PwEvent::Failed(format!("could not create a tap link: {e}")));
                }
            }
        }

        if self.shadow_node.is_some() && self.reported != Some(self.taps.len()) {
            self.reported = Some(self.taps.len());
            let _ = self.events.send(PwEvent::Tapped(self.taps.len()));
        }
    }
}

struct FilterData {
    shadow: Shadow,
    inputs: Vec<FilterPort>,
    outputs: Vec<FilterPort>,
}

/// Runs until `PwCommand::Quit` arrives. Blocks the calling thread.
pub fn run(
    shadow: Shadow,
    node_label: String,
    target: Option<TapTarget>,
    commands: pipewire::channel::Receiver<PwCommand>,
    events: mpsc::Sender<PwEvent>,
) -> Result<()> {
    pipewire::init();

    let mainloop =
        MainLoopRc::new(None).map_err(|e| anyhow!("creating the PipeWire main loop: {e}"))?;
    let context = ContextRc::new(&mainloop, None)
        .map_err(|e| anyhow!("creating the PipeWire context: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| anyhow!("connecting to PipeWire: {e}"))?;
    let registry = core
        .get_registry_rc()
        .map_err(|e| anyhow!("fetching the PipeWire registry: {e}"))?;

    // No media.class and no autoconnect: the session manager leaves the node
    // alone and Pipeweaver (which only lists `Stream/*` nodes) does not show it.
    let filter = FilterRc::new(
        core.clone(),
        "pipeweaver-lv2ui",
        properties! {
            *NODE_NAME => "pipeweaver-lv2ui",
            *NODE_DESCRIPTION => format!("pipeweaver-lv2ui/{node_label}"),
            *NODE_ALWAYS_PROCESS => "true",
            *MEDIA_TYPE => "Audio",
            *MEDIA_CATEGORY => "Filter",
            *MEDIA_ROLE => "DSP",
            *OBJECT_LINGER => "false",
            "node.autoconnect" => "false",
        },
    )
    .map_err(|e| anyhow!("creating the shadow filter node: {e}"))?;

    let mut data = FilterData {
        shadow,
        inputs: Vec::new(),
        outputs: Vec::new(),
    };
    for (direction, prefix, ports) in [
        (Direction::Input, "input", &mut data.inputs),
        (Direction::Output, "output", &mut data.outputs),
    ] {
        for channel in CHANNELS {
            let port = filter
                .add_port(
                    direction,
                    PortFlags::MAP_BUFFERS,
                    properties! {
                        *FORMAT_DSP => "32 bit float mono audio",
                        *PORT_NAME => format!("{prefix}_{channel}"),
                        *AUDIO_CHANNEL => channel,
                    },
                    &mut [],
                )
                .map_err(|e| anyhow!("adding a port to the shadow filter: {e}"))?;
            ports.push(port);
        }
    }

    let graph = Rc::new(RefCell::new(Graph {
        core: core.clone(),
        target,
        events: events.clone(),
        shadow_node: None,
        nodes: HashMap::new(),
        ports: HashMap::new(),
        links: HashMap::new(),
        taps: HashMap::new(),
        reported: None,
    }));

    let state_graph = graph.clone();
    let state_events = events.clone();
    let filter_listener = filter
        .add_local_listener_with_user_data(data)
        .state_changed(move |filter, _data, _old, new| match new {
            FilterState::Streaming | FilterState::Paused => {
                let id = filter.node_id();
                if id != u32::MAX {
                    let mut graph = state_graph.borrow_mut();
                    graph.shadow_node = Some(id);
                    graph.reconcile();
                }
            }
            FilterState::Error(e) => {
                let _ = state_events.send(PwEvent::Failed(format!("shadow filter error: {e}")));
            }
            _ => {}
        })
        .process(|filter, data, position| {
            let samples = position.clock.duration as u32;
            let mut inputs = [std::ptr::null_mut(); 2];
            let mut outputs = [std::ptr::null_mut(); 2];
            for (slot, port) in inputs.iter_mut().zip(&data.inputs) {
                if let Ok(buffer) = filter.get_dsp_buffer::<f32>(port, samples)
                    && !buffer.is_empty()
                {
                    *slot = buffer.as_mut_ptr();
                }
            }
            for (slot, port) in outputs.iter_mut().zip(&data.outputs) {
                if let Ok(buffer) = filter.get_dsp_buffer::<f32>(port, samples)
                    && !buffer.is_empty()
                {
                    *slot = buffer.as_mut_ptr();
                }
            }
            data.shadow.process(inputs, outputs, samples as usize);
        })
        .register()
        .map_err(|e| anyhow!("registering the shadow filter: {e}"))?;

    filter
        .connect(FilterFlags::RT_PROCESS, &mut [])
        .map_err(|e| anyhow!("connecting the shadow filter: {e}"))?;

    let added = graph.clone();
    let removed = graph.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            let Some(props) = global.props else { return };
            let number = |key: &str| props.get(key).and_then(|v| v.parse::<u32>().ok());
            let mut graph = added.borrow_mut();

            match global.type_ {
                ObjectType::Node => {
                    let name = props.get(*NODE_NAME).unwrap_or_default().to_string();
                    let description = props.get(*NODE_DESCRIPTION).unwrap_or_default().to_string();
                    graph.nodes.insert(global.id, (name, description));
                }
                ObjectType::Port => {
                    if let (Some(node), Some(direction), Some(name)) = (
                        number(*NODE_ID),
                        props.get(*PORT_DIRECTION),
                        props.get(*PORT_NAME),
                    ) {
                        graph
                            .ports
                            .insert(global.id, (node, direction == "in", name.to_string()));
                    }
                }
                ObjectType::Link => {
                    if let (Some(out_node), Some(out_port), Some(in_node), Some(in_port)) = (
                        number(*LINK_OUTPUT_NODE),
                        number(*LINK_OUTPUT_PORT),
                        number(*LINK_INPUT_NODE),
                        number(*LINK_INPUT_PORT),
                    ) {
                        graph
                            .links
                            .insert(global.id, (out_node, out_port, in_node, in_port));
                    }
                }
                _ => return,
            }
            graph.reconcile();
        })
        .global_remove(move |id| {
            let mut graph = removed.borrow_mut();
            let known = graph.nodes.remove(&id).is_some()
                | graph.ports.remove(&id).is_some()
                | graph.links.remove(&id).is_some();
            if known {
                graph.reconcile();
            }
        })
        .register();

    let quit_loop = mainloop.clone();
    let _commands = commands.attach(mainloop.loop_(), move |command| match command {
        PwCommand::Quit => quit_loop.quit(),
    });

    mainloop.run();

    // Take the tap down first, then the node, and only then the plugin instance
    graph.borrow_mut().taps.clear();
    let _ = filter.disconnect();
    drop(filter_listener);
    drop(filter);
    Ok(())
}
