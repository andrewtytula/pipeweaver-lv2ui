mod api;
mod lv2;
mod pw;
mod shadow;
mod ui;

use crate::api::{
    Api, ApiEvent, FilterEntry, Param, PatchEffect, builtin_filters, filter_params, lv2_filters,
    patch_effect,
};
use crate::lv2::{PortKind, Urids, World};
use crate::pw::{PwCommand, PwEvent, TapTarget};
use crate::shadow::Shadow;
use crate::ui::{Tick, UiHost};
use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// How often the UI is serviced and changes are exchanged with the daemon
const TICK: Duration = Duration::from_millis(33);

/// After the user moves a control, ignore the daemon's view of it for this
/// long. Its echoes of our own earlier writes would otherwise drag the knob
/// backwards mid-drag.
const ECHO_GUARD: Duration = Duration::from_millis(500);

/// The tap comes up one link at a time; wait for it to settle before reporting on it.
const TAP_SETTLE: Duration = Duration::from_secs(1);

/// Set by Ctrl-C or a termination request, so we can shut down in order.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn interrupted(_signal: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

#[derive(Parser)]
#[command(
    version,
    about = "Open the native LV2 UI of a filter running inside Pipeweaver"
)]
struct Cli {
    /// Websocket address of the Pipeweaver daemon
    #[arg(long, global = true, default_value = api::DEFAULT_URL)]
    url: String,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List the LV2 filters Pipeweaver is running
    List,
    /// Open a filter's native UI
    Open {
        /// Number from `list`, a filter id, or part of its device or plugin name
        filter: String,

        /// Don't feed live audio to the UI (meters and graphs stay still)
        #[arg(long)]
        no_tap: bool,

        /// Don't give the UI direct access to the plugin instance
        #[arg(long)]
        no_instance_access: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Cmd::List) => {
            let status = Api::connect(&cli.url)?.status()?;
            print_filters(&lv2_filters(&status), &status);
            Ok(())
        }
        Some(Cmd::Open {
            filter,
            no_tap,
            no_instance_access,
        }) => {
            let api = Api::connect(&cli.url)?;
            let status = api.status()?;
            let filters = lv2_filters(&status);
            let entry = pick(&filters, &filter)?;
            let world = World::new()?;
            open(
                api,
                &status,
                entry,
                &world,
                Urids::new(),
                !no_tap,
                !no_instance_access,
            )
        }
        None => browse(&cli.url),
    }
}

fn print_filters(filters: &[FilterEntry], status: &Value) {
    if filters.is_empty() {
        println!("Pipeweaver has no LV2 filters. Add one in its web UI first.");
    }
    for (number, filter) in filters.iter().enumerate() {
        let state = if filter.running {
            String::new()
        } else {
            format!("  [{}]", filter.state)
        };
        println!(
            "{:>2}  {} / {}{}",
            number + 1,
            filter.device,
            filter.name,
            state
        );
        println!("    {}  {}", filter.id, filter.plugin_uri);
    }

    // Say what was left out, so the list doesn't look like it lost filters
    let builtin = builtin_filters(status);
    if !builtin.is_empty() {
        println!(
            "\nNot listed, built into Pipeweaver with no LV2 UI: {}",
            builtin.join(", ")
        );
    }
}

fn pick<'a>(filters: &'a [FilterEntry], wanted: &str) -> Result<&'a FilterEntry> {
    if let Ok(number) = wanted.parse::<usize>() {
        return filters
            .get(number.wrapping_sub(1))
            .ok_or_else(|| anyhow!("there is no filter number {number} (see `list`)"));
    }
    if let Some(exact) = filters.iter().find(|f| f.id.eq_ignore_ascii_case(wanted)) {
        return Ok(exact);
    }

    let needle = wanted.to_lowercase();
    let matches: Vec<&FilterEntry> = filters
        .iter()
        .filter(|f| {
            format!("{}/{} {}", f.device, f.name, f.plugin_uri)
                .to_lowercase()
                .contains(&needle)
        })
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => bail!("no filter matches '{wanted}' (see `list`)"),
        many => bail!(
            "'{wanted}' matches {} filters, be more specific or use its number:\n{}",
            many.len(),
            many.iter()
                .map(|f| format!("  {} / {}", f.device, f.name))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

/// Ask which filter to open, and again each time its window closes, until told to quit.
fn browse(url: &str) -> Result<()> {
    // Loaded on the first open and kept: scanning the installed plugins is slow
    let mut lv2: Option<(World, &'static Urids)> = None;

    loop {
        // Not kept open while the prompt waits: the daemon's updates would pile up unread
        let status = Api::connect(url)?.status()?;
        let filters = lv2_filters(&status);
        print_filters(&filters, &status);
        if filters.is_empty() {
            bail!("nothing to open");
        }

        let chosen = loop {
            print!("\nOpen which filter (q to quit)? ");
            std::io::stdout().flush()?;
            let mut answer = String::new();
            if std::io::stdin().lock().read_line(&mut answer)? == 0 {
                println!();
                return Ok(());
            }
            let answer = answer.trim();
            if answer.eq_ignore_ascii_case("q") || answer.eq_ignore_ascii_case("quit") {
                return Ok(());
            }
            if answer.is_empty() {
                continue;
            }
            match pick(&filters, answer) {
                Ok(entry) => break entry.id.clone(),
                Err(e) => eprintln!("{e:#}"),
            }
        };

        // The filter as it is now, which may not be how it was when listed
        let api = Api::connect(url)?;
        let status = api.status()?;
        let filters = lv2_filters(&status);
        let Some(entry) = filters.iter().find(|f| f.id == chosen) else {
            eprintln!("That filter is no longer in Pipeweaver.\n");
            continue;
        };

        if lv2.is_none() {
            lv2 = Some((World::new()?, Urids::new()));
        }
        let (world, urids) = lv2.as_ref().unwrap();
        if let Err(e) = open(api, &status, entry, world, urids, true, true) {
            eprintln!("{e:#}");
        }
        if INTERRUPTED.load(Ordering::Relaxed) {
            return Ok(());
        }
        println!();
    }
}

/// A parameter Pipeweaver exposes, tied to the plugin port it controls.
struct Binding {
    port: u32,
    param: Param,
}

fn open(
    api: Api,
    status: &Value,
    entry: &FilterEntry,
    world: &World,
    urids: &'static Urids,
    tap: bool,
    instance_access: bool,
) -> Result<()> {
    if !entry.running {
        bail!(
            "{} / {} is not running in Pipeweaver ({})",
            entry.device,
            entry.name,
            entry.state
        );
    }

    let info = world.plugin(&entry.plugin_uri)?;
    let ui_info = info
        .ui
        .clone()
        .ok_or_else(|| anyhow!("{} has no X11 UI this tool can host", info.name))?;

    let rate = graph_rate();
    let (shadow, link) = Shadow::new(&info, rate, urids)?;

    // Start the shadow from the real filter's current values
    let bindings = bind(&info, filter_params(status, &entry.id)?);
    for binding in bindings.iter().filter(|b| b.param.is_input) {
        shadow::store(&link.controls, binding.port, binding.param.value);
    }

    let node_name = entry
        .plugin_uri
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string();
    let target = tap.then(|| TapTarget {
        description_prefix: format!("pipeweaver/{}-{}-", entry.device, node_name)
            .to_lowercase()
            .replace(' ', "-"),
        node_name,
        ordinal: entry.ordinal,
    });

    let (pw_commands, pw_command_rx) = pipewire::channel::channel();
    let (pw_event_tx, pw_events) = mpsc::channel();
    let label = format!("{}-{}", entry.device, info.name);
    let pw_thread = thread::Builder::new()
        .name("pipewire".into())
        .spawn(move || {
            if let Err(e) = pw::run(shadow, label, target, pw_command_rx, pw_event_tx.clone()) {
                let _ = pw_event_tx.send(PwEvent::Stopped(format!("{e:#}")));
            }
        })?;

    // From here on every path reaches the end of this function, which undoes this
    let handler = interrupted as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }

    let title = format!("{} - {} (Pipeweaver)", info.name, entry.device);
    let opened = UiHost::open(
        &info,
        &ui_info,
        urids,
        link.controls.clone(),
        link.to_dsp,
        instance_access.then_some(link.handle),
        rate,
        &title,
    );

    let result = opened.and_then(|ui| {
        println!(
            "Opened {} on {}. Close its window to finish.",
            info.name, entry.device
        );
        let mut session = Session {
            api,
            filter: entry.id.clone(),
            ui,
            controls: link.controls,
            from_dsp: link.from_dsp,
            pw_events,
            by_port: bindings
                .iter()
                .enumerate()
                .map(|(i, b)| (b.port, i))
                .collect(),
            by_position: bindings
                .iter()
                .enumerate()
                .map(|(i, b)| (b.param.position, i))
                .collect(),
            bindings,
            outputs: info
                .ports
                .iter()
                .filter(|p| p.kind == PortKind::ControlOut)
                .map(|p| (p.index, f32::NAN))
                .collect(),
            last_write: HashMap::new(),
            atom: vec![0u64; 1 << 17],
            tap: tap.then_some(TapStatus {
                links: 0,
                changed: Instant::now(),
                reported: None,
            }),
        };
        session.announce_controls(&info);
        session.run()
        // The UI is torn down here, before the plugin instance it may point into
    });

    let _ = pw_commands.send(PwCommand::Quit);
    let _ = pw_thread.join();
    // Neither the UI nor the audio thread is left to touch the plugin instance
    drop(link.instance);

    // Back to the default, so Ctrl-C still works at a prompt that follows
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
    }
    result
}

fn bind(info: &lv2::PluginInfo, params: Vec<Param>) -> Vec<Binding> {
    params
        .into_iter()
        .filter_map(|param| {
            let port = info.port_by_symbol(&param.symbol)?;
            matches!(port.kind, PortKind::ControlIn | PortKind::ControlOut).then_some(Binding {
                port: port.index,
                param,
            })
        })
        .collect()
}

struct Session {
    api: Api,
    filter: String,
    ui: UiHost,
    controls: std::sync::Arc<[std::sync::atomic::AtomicU32]>,
    from_dsp: rtrb::Consumer<u8>,
    pw_events: mpsc::Receiver<PwEvent>,
    bindings: Vec<Binding>,
    by_port: HashMap<u32, usize>,
    by_position: HashMap<usize, usize>,
    /// Control output ports and the value the UI last saw for each
    outputs: Vec<(u32, f32)>,
    last_write: HashMap<u32, Instant>,
    /// Aligned scratch space for atoms on their way to the UI
    atom: Vec<u64>,
    /// None when the tap was switched off
    tap: Option<TapStatus>,
}

struct TapStatus {
    links: usize,
    changed: Instant,
    reported: Option<usize>,
}

impl Session {
    /// A freshly opened UI knows nothing: tell it every control's value.
    fn announce_controls(&mut self, info: &lv2::PluginInfo) {
        for port in &info.ports {
            if port.kind == PortKind::ControlIn {
                self.ui
                    .control_changed(port.index, shadow::load(&self.controls, port.index));
            }
        }
    }

    fn run(&mut self) -> Result<()> {
        loop {
            let started = Instant::now();

            if INTERRUPTED.load(Ordering::Relaxed) || !self.handle_daemon()? {
                return Ok(());
            }
            self.handle_pipewire()?;
            self.forward_plugin_output();

            if self.ui.tick()? == Tick::Closed {
                return Ok(());
            }
            self.send_user_changes();

            thread::sleep(TICK.saturating_sub(started.elapsed()));
        }
    }

    /// Apply changes made elsewhere (the web UI, another client). Returns false when it is time to stop.
    fn handle_daemon(&mut self) -> Result<bool> {
        while let Ok(event) = self.api.events.try_recv() {
            match event {
                ApiEvent::Patch(operations) => {
                    for operation in &operations {
                        match patch_effect(operation, &self.filter) {
                            PatchEffect::Value(position, value) => {
                                if let Some(&index) = self.by_position.get(&position) {
                                    self.apply_remote(index, value);
                                }
                            }
                            PatchEffect::Resync => self.api.request_status(),
                            PatchEffect::Unrelated => {}
                        }
                    }
                }
                ApiEvent::Status(status) => {
                    let still_running = lv2_filters(&status)
                        .iter()
                        .any(|f| f.id == self.filter && f.running);
                    if !still_running {
                        println!("The filter was removed or stopped in Pipeweaver.");
                        return Ok(false);
                    }
                    // The list may have changed shape: match by symbol, take the new positions
                    self.by_position.clear();
                    for param in filter_params(&status, &self.filter)? {
                        let found = self
                            .bindings
                            .iter()
                            .position(|b| b.param.symbol == param.symbol);
                        if let Some(index) = found {
                            self.bindings[index].param.position = param.position;
                            self.bindings[index].param.id = param.id;
                            self.by_position.insert(param.position, index);
                            self.apply_remote(index, param.value);
                        }
                    }
                }
                ApiEvent::Rejected(reason) => eprintln!("Pipeweaver rejected a change: {reason}"),
                ApiEvent::Closed(reason) => {
                    println!("Lost the connection to Pipeweaver: {reason}");
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    fn apply_remote(&mut self, index: usize, value: f32) {
        let binding = &mut self.bindings[index];
        binding.param.value = value;
        if !binding.param.is_input {
            return;
        }
        let port = binding.port;

        if self
            .last_write
            .get(&port)
            .is_some_and(|at| at.elapsed() < ECHO_GUARD)
        {
            return;
        }
        if same(shadow::load(&self.controls, port), value) {
            return;
        }
        shadow::store(&self.controls, port, value);
        self.ui.control_changed(port, value);
    }

    fn handle_pipewire(&mut self) -> Result<()> {
        while let Ok(event) = self.pw_events.try_recv() {
            match event {
                PwEvent::Tapped(links) => {
                    if let Some(tap) = &mut self.tap {
                        tap.links = links;
                        tap.changed = Instant::now();
                    }
                }
                PwEvent::Failed(reason) => eprintln!("PipeWire: {reason}"),
                PwEvent::Stopped(reason) => bail!("PipeWire: {reason}"),
            }
        }

        if let Some(tap) = &mut self.tap
            && tap.reported != Some(tap.links)
            && tap.changed.elapsed() >= TAP_SETTLE
        {
            tap.reported = Some(tap.links);
            match tap.links {
                0 => println!(
                    "Not receiving audio: could not find the filter's input in PipeWire, so meters and graphs will stay still."
                ),
                links => println!("Receiving the filter's live audio ({links} channels)."),
            }
        }
        Ok(())
    }

    /// Meters and graph data from the shadow instance, on to the UI.
    fn forward_plugin_output(&mut self) {
        for (port, seen) in &mut self.outputs {
            let value = shadow::load(&self.controls, *port);
            if value.to_bits() != seen.to_bits() {
                *seen = value;
                self.ui.control_changed(*port, value);
            }
        }

        loop {
            // SAFETY: plain bytes viewed through a u64 buffer, which keeps atoms aligned
            let bytes = unsafe {
                std::slice::from_raw_parts_mut(
                    self.atom.as_mut_ptr() as *mut u8,
                    self.atom.len() * 8,
                )
            };
            let Some((port, len)) = shadow::read_message(&mut self.from_dsp, bytes) else {
                break;
            };
            self.ui
                .atom_received(port, self.atom.as_ptr() as *const u8, len);
        }
    }

    /// Controls the user moved in the native UI, on to the real filter.
    fn send_user_changes(&mut self) {
        let writes = self.ui.take_writes();
        if writes.is_empty() {
            return;
        }

        // Only the last value per control matters within one tick
        let mut latest: Vec<(usize, f32)> = Vec::new();
        let now = Instant::now();
        for (port, value) in writes {
            let Some(&index) = self.by_port.get(&port) else {
                continue;
            };
            if !self.bindings[index].param.is_input || !value.is_finite() {
                continue;
            }
            self.last_write.insert(port, now);
            match latest.iter_mut().find(|(i, _)| *i == index) {
                Some(entry) => entry.1 = value,
                None => latest.push((index, value)),
            }
        }

        let mut values: Vec<(u32, Value)> = Vec::new();
        for (index, value) in latest {
            let param = &mut self.bindings[index].param;
            if !same(param.value, value) {
                param.value = value;
                values.push((param.id, param.kind.encode(value)));
            }
        }
        if !values.is_empty() {
            self.api.set_filter_values(&self.filter, &values);
        }
    }
}

/// Float comparison loose enough to survive the JSON round trip.
fn same(a: f32, b: f32) -> bool {
    (a - b).abs() <= 1e-6 * a.abs().max(b.abs()).max(1.0)
}

/// The sample rate the PipeWire graph runs at, which the shadow must match.
fn graph_rate() -> u32 {
    let read = || -> Result<u32> {
        let output = Command::new("pw-metadata")
            .args(["-n", "settings", "0"])
            .output()
            .context("running pw-metadata")?;
        let text = String::from_utf8_lossy(&output.stdout);
        let setting = |key: &str| {
            text.lines()
                .find(|line| line.contains(&format!("key:'{key}'")))
                .and_then(|line| line.split("value:'").nth(1))
                .and_then(|rest| rest.split('\'').next())
                .and_then(|value| value.parse::<u32>().ok())
        };
        setting("clock.force-rate")
            .filter(|rate| *rate > 0)
            .or_else(|| setting("clock.rate"))
            .ok_or_else(|| anyhow!("no clock.rate in the PipeWire settings"))
    };
    read().unwrap_or_else(|e| {
        eprintln!("Could not read the PipeWire sample rate ({e:#}), assuming 48000.");
        48000
    })
}
