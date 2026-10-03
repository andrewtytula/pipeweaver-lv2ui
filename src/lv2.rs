//! The small slice of the LV2 C ABI this tool needs, plus plugin discovery
//! through lilv. The structs here mirror the LV2 headers exactly; they are
//! declared by hand because we only need a handful of them.

use anyhow::{Result, anyhow, bail};
use lilv_sys::*;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::PathBuf;
use std::ptr;
use std::sync::Mutex;

pub const URID_MAP: &CStr = c"http://lv2plug.in/ns/ext/urid#map";
pub const URID_UNMAP: &CStr = c"http://lv2plug.in/ns/ext/urid#unmap";
pub const OPTIONS: &CStr = c"http://lv2plug.in/ns/ext/options#options";
pub const INSTANCE_ACCESS: &CStr = c"http://lv2plug.in/ns/ext/instance-access";
pub const UI_PARENT: &CStr = c"http://lv2plug.in/ns/extensions/ui#parent";
pub const UI_RESIZE: &CStr = c"http://lv2plug.in/ns/extensions/ui#resize";
pub const UI_IDLE: &CStr = c"http://lv2plug.in/ns/extensions/ui#idleInterface";

const ATOM_SEQUENCE: &CStr = c"http://lv2plug.in/ns/ext/atom#Sequence";
const ATOM_CHUNK: &CStr = c"http://lv2plug.in/ns/ext/atom#Chunk";
const ATOM_FLOAT: &CStr = c"http://lv2plug.in/ns/ext/atom#Float";
const ATOM_INT: &CStr = c"http://lv2plug.in/ns/ext/atom#Int";
const EVENT_TRANSFER: &CStr = c"http://lv2plug.in/ns/ext/atom#eventTransfer";
const PARAM_SAMPLE_RATE: &CStr = c"http://lv2plug.in/ns/ext/parameters#sampleRate";
const BUF_MAX_BLOCK: &CStr = c"http://lv2plug.in/ns/ext/buf-size#maxBlockLength";

#[repr(C)]
pub struct Feature {
    pub uri: *const c_char,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct UridMap {
    pub handle: *mut c_void,
    pub map: unsafe extern "C" fn(handle: *mut c_void, uri: *const c_char) -> u32,
}

#[repr(C)]
pub struct UridUnmap {
    pub handle: *mut c_void,
    pub unmap: unsafe extern "C" fn(handle: *mut c_void, urid: u32) -> *const c_char,
}

#[repr(C)]
pub struct OptionsOption {
    pub context: u32,
    pub subject: u32,
    pub key: u32,
    pub size: u32,
    pub type_: u32,
    pub value: *const c_void,
}

pub type UiWriteFunction = unsafe extern "C" fn(
    controller: *mut c_void,
    port_index: u32,
    buffer_size: u32,
    port_protocol: u32,
    buffer: *const c_void,
);

#[repr(C)]
pub struct UiDescriptor {
    pub uri: *const c_char,
    pub instantiate: Option<
        unsafe extern "C" fn(
            descriptor: *const UiDescriptor,
            plugin_uri: *const c_char,
            bundle_path: *const c_char,
            write_function: UiWriteFunction,
            controller: *mut c_void,
            widget: *mut *mut c_void,
            features: *const *const Feature,
        ) -> *mut c_void,
    >,
    pub cleanup: Option<unsafe extern "C" fn(ui: *mut c_void)>,
    pub port_event: Option<
        unsafe extern "C" fn(
            ui: *mut c_void,
            port_index: u32,
            buffer_size: u32,
            format: u32,
            buffer: *const c_void,
        ),
    >,
    pub extension_data: Option<unsafe extern "C" fn(uri: *const c_char) -> *const c_void>,
}

#[repr(C)]
pub struct UiIdleInterface {
    pub idle: Option<unsafe extern "C" fn(ui: *mut c_void) -> c_int>,
}

#[repr(C)]
pub struct UiResize {
    pub handle: *mut c_void,
    pub ui_resize:
        Option<unsafe extern "C" fn(handle: *mut c_void, width: c_int, height: c_int) -> c_int>,
}

/// URI <-> integer mapping shared by the plugin instance and its UI. Both
/// must see the same numbers, since atoms passed between them carry URIDs.
pub struct Urids {
    inner: Mutex<UridTable>,
    pub sequence: u32,
    pub chunk: u32,
    pub float: u32,
    pub int: u32,
    pub event_transfer: u32,
    pub sample_rate: u32,
    pub max_block: u32,
}

#[derive(Default)]
struct UridTable {
    by_uri: HashMap<CString, u32>,
    // The CString heap buffers never move, so pointers handed out by `unmap` stay valid.
    by_id: Vec<CString>,
}

impl Urids {
    /// Leaked on purpose: plugin and UI hold raw pointers to it until exit.
    pub fn new() -> &'static Urids {
        let mut urids = Urids {
            inner: Mutex::new(UridTable::default()),
            sequence: 0,
            chunk: 0,
            float: 0,
            int: 0,
            event_transfer: 0,
            sample_rate: 0,
            max_block: 0,
        };
        urids.sequence = urids.map(ATOM_SEQUENCE);
        urids.chunk = urids.map(ATOM_CHUNK);
        urids.float = urids.map(ATOM_FLOAT);
        urids.int = urids.map(ATOM_INT);
        urids.event_transfer = urids.map(EVENT_TRANSFER);
        urids.sample_rate = urids.map(PARAM_SAMPLE_RATE);
        urids.max_block = urids.map(BUF_MAX_BLOCK);
        Box::leak(Box::new(urids))
    }

    pub fn map(&self, uri: &CStr) -> u32 {
        let mut table = self.inner.lock().unwrap();
        if let Some(id) = table.by_uri.get(uri) {
            return *id;
        }
        table.by_id.push(uri.to_owned());
        let id = table.by_id.len() as u32;
        table.by_uri.insert(uri.to_owned(), id);
        id
    }

    fn unmap(&self, id: u32) -> *const c_char {
        let table = self.inner.lock().unwrap();
        match table.by_id.get((id as usize).wrapping_sub(1)) {
            Some(uri) => uri.as_ptr(),
            None => ptr::null(),
        }
    }

    pub fn map_feature(&'static self) -> UridMap {
        UridMap {
            handle: self as *const Urids as *mut c_void,
            map: urid_map_callback,
        }
    }

    pub fn unmap_feature(&'static self) -> UridUnmap {
        UridUnmap {
            handle: self as *const Urids as *mut c_void,
            unmap: urid_unmap_callback,
        }
    }
}

unsafe extern "C" fn urid_map_callback(handle: *mut c_void, uri: *const c_char) -> u32 {
    if handle.is_null() || uri.is_null() {
        return 0;
    }
    unsafe { (*(handle as *const Urids)).map(CStr::from_ptr(uri)) }
}

unsafe extern "C" fn urid_unmap_callback(handle: *mut c_void, urid: u32) -> *const c_char {
    if handle.is_null() {
        return ptr::null();
    }
    unsafe { (*(handle as *const Urids)).unmap(urid) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortKind {
    AudioIn,
    AudioOut,
    ControlIn,
    ControlOut,
    AtomIn,
    AtomOut,
    Other,
}

#[derive(Debug, Clone)]
pub struct PortInfo {
    pub index: u32,
    pub symbol: String,
    pub kind: PortKind,
    pub default: f32,
    /// `rsz:minimumSize` in bytes, for atom ports
    pub minimum_size: usize,
}

#[derive(Debug, Clone)]
pub struct UiInfo {
    pub uri: CString,
    pub binary: PathBuf,
    pub bundle: CString,
}

pub struct PluginInfo {
    pub uri: CString,
    pub name: String,
    pub ports: Vec<PortInfo>,
    pub ui: Option<UiInfo>,
    pub(crate) plugin: *const LilvPlugin,
}

impl PluginInfo {
    pub fn port_by_symbol(&self, symbol: &str) -> Option<&PortInfo> {
        self.ports.iter().find(|p| p.symbol == symbol)
    }
}

/// The lilv world. Kept alive for the whole process, as instances point into it.
pub struct World {
    world: *mut LilvWorld,
}

impl World {
    pub fn new() -> Result<World> {
        unsafe {
            let world = lilv_world_new();
            if world.is_null() {
                bail!("lilv could not create an LV2 world");
            }
            lilv_world_load_all(world);
            Ok(World { world })
        }
    }

    fn uri(&self, uri: &str) -> Result<Node> {
        let uri = CString::new(uri)?;
        let node = unsafe { lilv_new_uri(self.world, uri.as_ptr()) };
        if node.is_null() {
            bail!("lilv rejected the URI {uri:?}");
        }
        Ok(Node(node))
    }

    pub fn plugin(&self, plugin_uri: &str) -> Result<PluginInfo> {
        const LV2: &str = "http://lv2plug.in/ns/lv2core#";
        const ATOM: &str = "http://lv2plug.in/ns/ext/atom#";

        let uri_node = self.uri(plugin_uri)?;
        let audio = self.uri(&format!("{LV2}AudioPort"))?;
        let control = self.uri(&format!("{LV2}ControlPort"))?;
        let input = self.uri(&format!("{LV2}InputPort"))?;
        let output = self.uri(&format!("{LV2}OutputPort"))?;
        let atom = self.uri(&format!("{ATOM}AtomPort"))?;
        let min_size = self.uri("http://lv2plug.in/ns/ext/resize-port#minimumSize")?;
        let x11_ui = self.uri("http://lv2plug.in/ns/extensions/ui#X11UI")?;

        unsafe {
            let plugins = lilv_world_get_all_plugins(self.world);
            let plugin = lilv_plugins_get_by_uri(plugins, uri_node.0);
            if plugin.is_null() {
                bail!("LV2 plugin {plugin_uri} is not installed");
            }

            let name_node = lilv_plugin_get_name(plugin);
            let name = if name_node.is_null() {
                plugin_uri.to_string()
            } else {
                let name = node_string(name_node);
                lilv_node_free(name_node);
                name
            };

            let mut ports = Vec::new();
            for index in 0..lilv_plugin_get_num_ports(plugin) {
                let port = lilv_plugin_get_port_by_index(plugin, index);
                if port.is_null() {
                    continue;
                }

                let is_input = lilv_port_is_a(plugin, port, input.0);
                let is_output = lilv_port_is_a(plugin, port, output.0);
                let kind = if lilv_port_is_a(plugin, port, audio.0) {
                    if is_input {
                        PortKind::AudioIn
                    } else {
                        PortKind::AudioOut
                    }
                } else if lilv_port_is_a(plugin, port, control.0) {
                    if is_input {
                        PortKind::ControlIn
                    } else {
                        PortKind::ControlOut
                    }
                } else if lilv_port_is_a(plugin, port, atom.0) && (is_input || is_output) {
                    if is_input {
                        PortKind::AtomIn
                    } else {
                        PortKind::AtomOut
                    }
                } else {
                    PortKind::Other
                };

                // Owned by the port, must not be freed
                let symbol = node_string(lilv_port_get_symbol(plugin, port));

                let mut default: *mut LilvNode = ptr::null_mut();
                lilv_port_get_range(plugin, port, &mut default, ptr::null_mut(), ptr::null_mut());
                let default_value = if default.is_null() {
                    0.0
                } else {
                    let value = lilv_node_as_float(default);
                    lilv_node_free(default);
                    value
                };

                let size_node = lilv_port_get(plugin, port, min_size.0);
                let minimum_size = if size_node.is_null() {
                    0
                } else {
                    let size = lilv_node_as_int(size_node).max(0) as usize;
                    lilv_node_free(size_node);
                    size
                };

                ports.push(PortInfo {
                    index,
                    symbol,
                    kind,
                    default: default_value,
                    minimum_size,
                });
            }

            let mut ui = None;
            let uis = lilv_plugin_get_uis(plugin);
            if !uis.is_null() {
                let mut iter = lilv_uis_begin(uis);
                while !lilv_uis_is_end(uis, iter) {
                    let candidate = lilv_uis_get(uis, iter);
                    if lilv_ui_is_a(candidate, x11_ui.0) {
                        ui = Some(UiInfo {
                            uri: CString::new(node_string(lilv_ui_get_uri(candidate)))?,
                            binary: PathBuf::from(file_path(lilv_ui_get_binary_uri(candidate))?),
                            bundle: CString::new(file_path(lilv_ui_get_bundle_uri(candidate))?)?,
                        });
                        break;
                    }
                    iter = lilv_uis_next(uis, iter);
                }
                lilv_uis_free(uis);
            }

            Ok(PluginInfo {
                uri: CString::new(plugin_uri)?,
                name,
                ports,
                ui,
                plugin,
            })
        }
    }
}

struct Node(*mut LilvNode);

impl Drop for Node {
    fn drop(&mut self) {
        unsafe { lilv_node_free(self.0) }
    }
}

unsafe fn node_string(node: *const LilvNode) -> String {
    if node.is_null() {
        return String::new();
    }
    unsafe {
        CStr::from_ptr(lilv_node_as_string(node))
            .to_string_lossy()
            .into_owned()
    }
}

/// Turn a `file://` URI node into a filesystem path.
unsafe fn file_path(node: *const LilvNode) -> Result<String> {
    if node.is_null() {
        return Err(anyhow!("the plugin UI has no binary or bundle path"));
    }
    unsafe {
        let parsed = lilv_file_uri_parse(lilv_node_as_string(node), ptr::null_mut());
        if parsed.is_null() {
            bail!("could not parse UI path {}", node_string(node));
        }
        let path = CStr::from_ptr(parsed).to_string_lossy().into_owned();
        lilv_free(parsed as *mut c_void);
        Ok(path)
    }
}
