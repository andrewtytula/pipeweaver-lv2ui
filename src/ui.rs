//! Hosts the plugin's native UI: loads the UI library, gives it an X11 window
//! to embed itself in, and pumps its idle callback. Everything here runs on
//! the main thread, including every callback the UI makes into us.

use crate::lv2::{
    Feature, INSTANCE_ACCESS, OPTIONS, OptionsOption, PluginInfo, UI_IDLE, UI_PARENT, UI_RESIZE,
    URID_MAP, URID_UNMAP, UiDescriptor, UiIdleInterface, UiInfo, UiResize, UridMap, UridUnmap,
    Urids,
};
use crate::shadow::{self, write_message};
use anyhow::{Context, Result, anyhow, bail};
use rtrb::Producer;
use std::ffi::{c_char, c_int, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::properties::WmSizeHints;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ConfigureWindowAux, ConnectionExt, CreateWindowAux, EventMask, PropMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// The port protocol and buffer format LV2 uses for a plain float control value
const FLOAT_PROTOCOL: u32 = 0;

/// How long a resize from either side is left alone before we react to it
const RESIZE_SETTLE: Duration = Duration::from_millis(300);

/// What the UI's callbacks write into. Lives in a Box so its address is stable.
struct Controller {
    controls: Arc<[AtomicU32]>,
    to_dsp: Producer<u8>,
    event_transfer: u32,
    /// Control values the user changed since the last `take_writes`
    writes: Vec<(u32, f32)>,
    /// Size the UI asked its window to be
    resize_request: Option<(u16, u16)>,
}

pub struct UiHost {
    descriptor: *const UiDescriptor,
    handle: *mut c_void,
    idle: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
    resize: *const UiResize,
    controller: *mut Controller,
    connection: RustConnection,
    window: u32,
    /// The UI's own window, embedded in ours
    child: u32,
    protocols_atom: u32,
    delete_atom: u32,
    /// Our window's size, as last confirmed by the X server
    size: (u16, u16),
    /// The size the UI's own window has, or has asked for
    ui_size: Option<(u16, u16)>,
    /// When the window manager last resized us, and when we last asked it to
    resized_at: Option<Instant>,
    requested_at: Option<Instant>,
    event_transfer: u32,

    _library: libloading::Library,
    _storage: Box<FeatureStorage>,
}

struct FeatureStorage {
    _map: UridMap,
    _unmap: UridUnmap,
    _resize: UiResize,
    _options: [OptionsOption; 2],
    _sample_rate: f32,
}

#[derive(PartialEq, Eq)]
pub enum Tick {
    Open,
    Closed,
}

impl UiHost {
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        info: &PluginInfo,
        ui: &UiInfo,
        urids: &'static Urids,
        controls: Arc<[AtomicU32]>,
        to_dsp: Producer<u8>,
        instance: Option<*mut c_void>,
        rate: u32,
        title: &str,
    ) -> Result<UiHost> {
        let (connection, screen_number) = x11rb::connect(None).context(
            "connecting to the X server (the plugin UI is X11; on Wayland this needs XWayland)",
        )?;
        let screen = &connection.setup().roots[screen_number];
        let window = connection.generate_id()?;
        let size = (640u16, 480u16);

        connection.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            screen.root,
            0,
            0,
            size.0,
            size.1,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new()
                .background_pixel(screen.black_pixel)
                .event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_NOTIFY),
        )?;

        let protocols = connection
            .intern_atom(false, b"WM_PROTOCOLS")?
            .reply()?
            .atom;
        let delete_atom = connection
            .intern_atom(false, b"WM_DELETE_WINDOW")?
            .reply()?
            .atom;
        let net_name = connection
            .intern_atom(false, b"_NET_WM_NAME")?
            .reply()?
            .atom;
        let utf8 = connection.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        connection.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            title.as_bytes(),
        )?;
        connection.change_property8(PropMode::REPLACE, window, net_name, utf8, title.as_bytes())?;
        connection.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"pipeweaver-lv2ui\0pipeweaver-lv2ui\0",
        )?;
        connection.change_property32(
            PropMode::REPLACE,
            window,
            protocols,
            AtomEnum::ATOM,
            &[delete_atom],
        )?;
        connection.flush()?;

        let library = unsafe { libloading::Library::new(&ui.binary) }
            .with_context(|| format!("loading the UI library {}", ui.binary.display()))?;
        let descriptor = unsafe { find_descriptor(&library, ui)? };

        let controller = Box::into_raw(Box::new(Controller {
            controls,
            to_dsp,
            event_transfer: urids.event_transfer,
            writes: Vec::new(),
            resize_request: None,
        }));

        let mut storage = Box::new(FeatureStorage {
            _map: urids.map_feature(),
            _unmap: urids.unmap_feature(),
            _resize: UiResize {
                handle: controller as *mut c_void,
                ui_resize: Some(host_resize),
            },
            _options: [
                OptionsOption {
                    context: 0,
                    subject: 0,
                    key: urids.sample_rate,
                    size: 4,
                    type_: urids.float,
                    value: ptr::null(),
                },
                OptionsOption {
                    context: 0,
                    subject: 0,
                    key: 0,
                    size: 0,
                    type_: 0,
                    value: ptr::null(),
                },
            ],
            _sample_rate: rate as f32,
        });
        storage._options[0].value = &storage._sample_rate as *const f32 as *const c_void;

        let feature = |uri: &std::ffi::CStr, data: *mut c_void| Feature {
            uri: uri.as_ptr(),
            data,
        };
        let mut features = vec![
            feature(URID_MAP, &storage._map as *const UridMap as *mut c_void),
            feature(
                URID_UNMAP,
                &storage._unmap as *const UridUnmap as *mut c_void,
            ),
            feature(OPTIONS, storage._options.as_ptr() as *mut c_void),
            feature(UI_PARENT, window as usize as *mut c_void),
            feature(
                UI_RESIZE,
                &storage._resize as *const UiResize as *mut c_void,
            ),
            feature(UI_IDLE, ptr::null_mut()),
        ];
        if let Some(instance) = instance {
            features.push(feature(INSTANCE_ACCESS, instance));
        }
        let mut feature_pointers: Vec<*const Feature> =
            features.iter().map(|f| f as *const Feature).collect();
        feature_pointers.push(ptr::null());

        let mut widget: *mut c_void = ptr::null_mut();
        let handle = unsafe {
            let instantiate = (*descriptor)
                .instantiate
                .ok_or_else(|| anyhow!("the UI has no instantiate function"))?;
            instantiate(
                descriptor,
                info.uri.as_ptr(),
                ui.bundle.as_ptr(),
                write_function,
                controller as *mut c_void,
                &mut widget,
                feature_pointers.as_ptr(),
            )
        };
        if handle.is_null() {
            unsafe { drop(Box::from_raw(controller)) };
            let _ = connection.destroy_window(window);
            let _ = connection.flush();
            bail!("the plugin UI failed to open");
        }

        let (idle, resize) = unsafe {
            match (*descriptor).extension_data {
                Some(extension_data) => {
                    let idle = extension_data(UI_IDLE.as_ptr()) as *const UiIdleInterface;
                    let resize = extension_data(UI_RESIZE.as_ptr()) as *const UiResize;
                    (if idle.is_null() { None } else { (*idle).idle }, resize)
                }
                None => (None, ptr::null()),
            }
        };

        let mut host = UiHost {
            descriptor,
            handle,
            idle,
            resize,
            controller,
            connection,
            window,
            child: widget as usize as u32,
            protocols_atom: protocols,
            delete_atom,
            size,
            ui_size: None,
            resized_at: None,
            requested_at: None,
            event_transfer: urids.event_transfer,
            _library: library,
            _storage: storage,
        };

        // Size our window to the UI: either it already asked, or we measure its window
        host.ui_size = unsafe { (*host.controller).resize_request.take() };
        if host.ui_size.is_none() && host.child != 0 {
            let geometry = host.connection.get_geometry(host.child)?.reply();
            host.ui_size = geometry.ok().map(|g| (g.width, g.height));
        }
        if let Some(size) = host.ui_size.filter(|s| s.0 > 0 && s.1 > 0) {
            // Not mapped yet, so this takes effect directly
            host.request_size(size)?;
            host.size = size;
        }
        host.adopt_size_limits();
        host.connection.map_subwindows(host.window)?;
        host.connection.map_window(host.window)?;
        host.connection.flush()?;
        Ok(host)
    }

    /// Pump window events and the UI's idle callback. Call ~30 times a second.
    pub fn tick(&mut self) -> Result<Tick> {
        while let Some(event) = self.connection.poll_for_event()? {
            match event {
                Event::ClientMessage(message)
                    if message.window == self.window
                        && message.type_ == self.protocols_atom
                        && message.format == 32
                        && message.data.as_data32()[0] == self.delete_atom =>
                {
                    return Ok(Tick::Closed);
                }
                Event::DestroyNotify(destroyed) if destroyed.window == self.window => {
                    return Ok(Tick::Closed);
                }
                Event::ConfigureNotify(configure) if configure.window == self.window => {
                    let size = (configure.width, configure.height);
                    if size != self.size {
                        self.size = size;
                        self.resized_at = Some(Instant::now());
                        // We were resized: ask the UI to fill the new size
                        if self.ui_size != Some(size) {
                            unsafe {
                                if !self.resize.is_null()
                                    && let Some(ui_resize) = (*self.resize).ui_resize
                                {
                                    ui_resize(self.handle, size.0 as c_int, size.1 as c_int);
                                }
                            }
                        }
                    }
                }
                // The UI resized its own window (it often grows once its layout settles)
                Event::ConfigureNotify(configure) if configure.window == self.child => {
                    self.ui_size = Some((configure.width, configure.height));
                    self.adopt_size_limits();
                }
                _ => {}
            }
        }

        if let Some(idle) = self.idle
            && unsafe { idle(self.handle) } != 0
        {
            return Ok(Tick::Closed);
        }

        if let Some(size) = unsafe { (*self.controller).resize_request.take() } {
            self.ui_size = Some(size);
        }
        self.follow_ui_size()?;
        Ok(Tick::Open)
    }

    /// Keep our window the size of the UI inside it. The UI has the last
    /// word: if it would not take the size the window manager gave us, we
    /// go back to the size it does want. Both sides get a moment to settle
    /// first, so the two never chase each other.
    fn follow_ui_size(&mut self) -> Result<()> {
        let Some(wanted) = self.ui_size else {
            return Ok(());
        };
        let settled = |at: Option<Instant>| at.is_none_or(|at| at.elapsed() >= RESIZE_SETTLE);
        if wanted != self.size
            && wanted.0 > 0
            && wanted.1 > 0
            && settled(self.resized_at)
            && settled(self.requested_at)
        {
            self.requested_at = Some(Instant::now());
            self.request_size(wanted)?;
        }
        Ok(())
    }

    /// Copy the UI's minimum/maximum size onto our window, so the window
    /// manager won't let it be dragged smaller than the UI can draw.
    fn adopt_size_limits(&mut self) {
        if self.child == 0 {
            return;
        }
        let hints = WmSizeHints::get_normal_hints(&self.connection, self.child)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .flatten();
        if let Some(hints) = hints {
            let limits = WmSizeHints {
                min_size: hints.min_size,
                max_size: hints.max_size,
                ..WmSizeHints::new()
            };
            let _ = limits.set_normal_hints(&self.connection, self.window);
        }
    }

    fn request_size(&mut self, size: (u16, u16)) -> Result<()> {
        self.connection.configure_window(
            self.window,
            &ConfigureWindowAux::new()
                .width(size.0 as u32)
                .height(size.1 as u32),
        )?;
        self.connection.flush()?;
        Ok(())
    }

    /// Tell the UI a control port has a new value.
    pub fn control_changed(&mut self, port: u32, value: f32) {
        unsafe {
            if let Some(port_event) = (*self.descriptor).port_event {
                port_event(
                    self.handle,
                    port,
                    4,
                    FLOAT_PROTOCOL,
                    &value as *const f32 as *const c_void,
                );
            }
        }
    }

    /// Hand the UI an atom the plugin emitted. `atom` must be 8-byte aligned.
    pub fn atom_received(&mut self, port: u32, atom: *const u8, len: usize) {
        unsafe {
            if let Some(port_event) = (*self.descriptor).port_event {
                port_event(
                    self.handle,
                    port,
                    len as u32,
                    self.event_transfer,
                    atom as *const c_void,
                );
            }
        }
    }

    /// Control values the user changed in the UI since the last call.
    pub fn take_writes(&mut self) -> Vec<(u32, f32)> {
        unsafe { std::mem::take(&mut (*self.controller).writes) }
    }
}

impl Drop for UiHost {
    fn drop(&mut self) {
        unsafe {
            if let Some(cleanup) = (*self.descriptor).cleanup {
                cleanup(self.handle);
            }
            drop(Box::from_raw(self.controller));
        }
        let _ = self.connection.destroy_window(self.window);
        let _ = self.connection.flush();
    }
}

unsafe fn find_descriptor(
    library: &libloading::Library,
    ui: &UiInfo,
) -> Result<*const UiDescriptor> {
    unsafe {
        let lookup: libloading::Symbol<unsafe extern "C" fn(u32) -> *const UiDescriptor> = library
            .get(b"lv2ui_descriptor\0")
            .context("the UI library has no lv2ui_descriptor symbol")?;

        // One library can hold many UIs (LSP ships all of its UIs in one)
        for index in 0.. {
            let descriptor = lookup(index);
            if descriptor.is_null() {
                break;
            }
            let uri: *const c_char = (*descriptor).uri;
            if !uri.is_null() && std::ffi::CStr::from_ptr(uri) == ui.uri.as_c_str() {
                return Ok(descriptor);
            }
        }
        bail!("UI {:?} was not found in {}", ui.uri, ui.binary.display())
    }
}

/// Called by the UI when the user moves a control, or to send an atom to the plugin.
unsafe extern "C" fn write_function(
    controller: *mut c_void,
    port_index: u32,
    buffer_size: u32,
    port_protocol: u32,
    buffer: *const c_void,
) {
    if controller.is_null() || buffer.is_null() {
        return;
    }
    let controller = unsafe { &mut *(controller as *mut Controller) };

    if port_protocol == FLOAT_PROTOCOL {
        if buffer_size as usize != size_of::<f32>()
            || port_index as usize >= controller.controls.len()
        {
            return;
        }
        let value = unsafe { (buffer as *const f32).read_unaligned() };
        shadow::store(&controller.controls, port_index, value);
        controller.writes.push((port_index, value));
    } else if port_protocol == controller.event_transfer {
        let atom = unsafe { std::slice::from_raw_parts(buffer as *const u8, buffer_size as usize) };
        write_message(&mut controller.to_dsp, port_index, atom);
    }
}

/// Called by the UI when it wants its window to be a different size.
unsafe extern "C" fn host_resize(handle: *mut c_void, width: c_int, height: c_int) -> c_int {
    if handle.is_null() || width <= 0 || height <= 0 {
        return 1;
    }
    let controller = unsafe { &mut *(handle as *mut Controller) };
    controller.resize_request = Some((
        width.min(u16::MAX as c_int) as u16,
        height.min(u16::MAX as c_int) as u16,
    ));
    0
}
