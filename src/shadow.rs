//! The shadow instance: a second copy of the plugin that the native UI talks
//! to. It is fed the same audio as the real filter inside Pipeweaver and its
//! output is discarded, so its meters and graphs match what you hear, while
//! control changes are mirrored to the real filter over the daemon API.
//!
//! Control ports are plain floats behind atomics (the UI thread writes
//! inputs, the audio thread writes outputs). Atom messages travel through two
//! lock-free byte rings, each message framed as `[port: u32][len: u32][atom]`.

use crate::lv2::{Feature, OptionsOption, PluginInfo, PortKind, UridMap, Urids};
use anyhow::{Result, bail};
use lilv_sys::{
    LilvInstance, lilv_instance_activate, lilv_instance_connect_port, lilv_instance_deactivate,
    lilv_instance_free, lilv_instance_run, lilv_plugin_instantiate,
};
use rtrb::{Consumer, Producer, RingBuffer};
use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// Largest PipeWire quantum we are prepared to process
pub const MAX_BLOCK: usize = 8192;

const RING_BYTES: usize = 1 << 20;
const MIN_ATOM_BYTES: usize = 1 << 16;
const FRAME_HEADER: usize = 8;
const ATOM_HEADER: usize = 8;
const SEQUENCE_BODY: usize = 8;

struct AtomPort {
    index: u32,
    is_input: bool,
    /// u64 backing keeps the atom buffer 8-byte aligned
    buffer: Box<[u64]>,
    /// Bytes used in an input sequence this cycle (header and body included)
    fill: usize,
}

impl AtomPort {
    fn bytes(&self) -> usize {
        self.buffer.len() * 8
    }

    fn ptr(&mut self) -> *mut u8 {
        self.buffer.as_mut_ptr() as *mut u8
    }
}

/// Owns the plugin instance. It is shared, so the instance outlives whichever
/// of the audio thread and the UI lets go of it first.
pub struct Instance(*mut LilvInstance);

// Only `Shadow` ever runs the instance; every other holder just keeps it alive.
unsafe impl Send for Instance {}
unsafe impl Sync for Instance {}

impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            lilv_instance_deactivate(self.0);
            lilv_instance_free(self.0);
        }
    }
}

pub struct Shadow {
    instance: Arc<Instance>,
    audio_in: Vec<u32>,
    audio_out: Vec<u32>,
    atom_ports: Vec<AtomPort>,
    silence: Box<[f32]>,
    discard: Box<[f32]>,
    message: Box<[u8]>,
    to_dsp: Consumer<u8>,
    from_dsp: Producer<u8>,
    sequence: u32,
    chunk: u32,

    // Referenced by the plugin for as long as the instance lives
    _controls: Arc<[AtomicU32]>,
    _features: Box<FeatureStorage>,
}

struct FeatureStorage {
    _urid_map: UridMap,
    _options: [OptionsOption; 3],
    _sample_rate: f32,
    _max_block: i32,
}

/// The UI thread's side of the shadow instance.
pub struct ShadowLink {
    /// One slot per plugin port index, holding f32 bits. Only control ports are meaningful.
    pub controls: Arc<[AtomicU32]>,
    pub to_dsp: Producer<u8>,
    pub from_dsp: Consumer<u8>,
    /// The plugin's `LV2_Handle`, for the instance-access feature
    pub handle: *mut c_void,
    /// Hold this for as long as `handle` is in use
    pub instance: Arc<Instance>,
}

// The instance is created on the main thread and then only ever run from
// PipeWire's data thread; the UI thread reaches it through `ShadowLink` only.
unsafe impl Send for Shadow {}

impl Shadow {
    pub fn new(
        info: &PluginInfo,
        rate: u32,
        urids: &'static Urids,
    ) -> Result<(Shadow, ShadowLink)> {
        let controls: Arc<[AtomicU32]> = info
            .ports
            .iter()
            .map(|p| AtomicU32::new(p.default.to_bits()))
            .collect();

        let mut storage = Box::new(FeatureStorage {
            _urid_map: urids.map_feature(),
            _options: [const { empty_option() }; 3],
            _sample_rate: rate as f32,
            _max_block: MAX_BLOCK as i32,
        });
        storage._options[0] = OptionsOption {
            context: 0,
            subject: 0,
            key: urids.sample_rate,
            size: 4,
            type_: urids.float,
            value: &storage._sample_rate as *const f32 as *const c_void,
        };
        storage._options[1] = OptionsOption {
            context: 0,
            subject: 0,
            key: urids.max_block,
            size: 4,
            type_: urids.int,
            value: &storage._max_block as *const i32 as *const c_void,
        };

        let map_feature = Feature {
            uri: crate::lv2::URID_MAP.as_ptr(),
            data: &storage._urid_map as *const UridMap as *mut c_void,
        };
        let options_feature = Feature {
            uri: crate::lv2::OPTIONS.as_ptr(),
            data: storage._options.as_ptr() as *mut c_void,
        };
        let features = [
            &map_feature as *const Feature,
            &options_feature as *const Feature,
            ptr::null(),
        ];

        let instance = unsafe {
            lilv_plugin_instantiate(info.plugin, rate as f64, features.as_ptr() as *const _)
        };
        if instance.is_null() {
            bail!("the plugin refused to instantiate");
        }
        let owner = Arc::new(Instance(instance));

        let (to_dsp_tx, to_dsp_rx) = RingBuffer::new(RING_BYTES);
        let (from_dsp_tx, from_dsp_rx) = RingBuffer::new(RING_BYTES);

        let mut shadow = Shadow {
            instance: owner.clone(),
            audio_in: Vec::new(),
            audio_out: Vec::new(),
            atom_ports: Vec::new(),
            silence: vec![0.0; MAX_BLOCK].into_boxed_slice(),
            discard: vec![0.0; MAX_BLOCK].into_boxed_slice(),
            message: vec![0; RING_BYTES].into_boxed_slice(),
            to_dsp: to_dsp_rx,
            from_dsp: from_dsp_tx,
            sequence: urids.sequence,
            chunk: urids.chunk,
            _controls: controls.clone(),
            _features: storage,
        };

        for port in &info.ports {
            match port.kind {
                PortKind::AudioIn => shadow.audio_in.push(port.index),
                PortKind::AudioOut => shadow.audio_out.push(port.index),
                PortKind::ControlIn | PortKind::ControlOut => unsafe {
                    let slot = controls[port.index as usize].as_ptr();
                    lilv_instance_connect_port(instance, port.index, slot as *mut c_void);
                },
                PortKind::AtomIn | PortKind::AtomOut => {
                    let bytes = port.minimum_size.max(MIN_ATOM_BYTES);
                    shadow.atom_ports.push(AtomPort {
                        index: port.index,
                        is_input: port.kind == PortKind::AtomIn,
                        buffer: vec![0u64; bytes.div_ceil(8)].into_boxed_slice(),
                        fill: 0,
                    });
                }
                // Unknown port types (CV and the like) are left unconnected
                PortKind::Other => {}
            }
        }

        // The buffers are boxed, so these pointers stay valid when `shadow` moves
        for port in &mut shadow.atom_ports {
            unsafe { lilv_instance_connect_port(instance, port.index, port.ptr() as *mut c_void) };
        }

        unsafe { lilv_instance_activate(instance) };

        let link = ShadowLink {
            controls,
            to_dsp: to_dsp_tx,
            from_dsp: from_dsp_rx,
            handle: unsafe { (*instance).lv2_handle },
            instance: owner,
        };
        Ok((shadow, link))
    }

    /// Run one cycle. Null pointers mean "port not linked": inputs then read
    /// silence and outputs are thrown away. Called from the realtime thread.
    pub fn process(&mut self, inputs: [*mut f32; 2], outputs: [*mut f32; 2], samples: usize) {
        if samples == 0 || samples > MAX_BLOCK {
            return;
        }

        unsafe {
            for (i, &port) in self.audio_in.iter().enumerate() {
                let buffer = match inputs.get(i) {
                    Some(p) if !p.is_null() => *p,
                    // Sidechain inputs and unlinked ports hear silence
                    _ => self.silence.as_mut_ptr(),
                };
                lilv_instance_connect_port(self.instance.0, port, buffer as *mut c_void);
            }
            for (i, &port) in self.audio_out.iter().enumerate() {
                let buffer = match outputs.get(i) {
                    Some(p) if !p.is_null() => *p,
                    _ => self.discard.as_mut_ptr(),
                };
                lilv_instance_connect_port(self.instance.0, port, buffer as *mut c_void);
            }
        }

        self.prepare_atom_ports();
        self.deliver_ui_messages();

        unsafe { lilv_instance_run(self.instance.0, samples as u32) };

        self.collect_plugin_messages();
    }

    fn prepare_atom_ports(&mut self) {
        let (sequence, chunk) = (self.sequence, self.chunk);
        for port in &mut self.atom_ports {
            let bytes = port.bytes();
            let header = port.buffer.as_mut_ptr() as *mut u32;
            unsafe {
                if port.is_input {
                    // An empty sequence: atom header, then { unit, pad }
                    header.write(SEQUENCE_BODY as u32);
                    header.add(1).write(sequence);
                    header.add(2).write(0);
                    header.add(3).write(0);
                    port.fill = ATOM_HEADER + SEQUENCE_BODY;
                } else {
                    // Tell the plugin how much room it has to write into
                    header.write((bytes - ATOM_HEADER) as u32);
                    header.add(1).write(chunk);
                }
            }
        }
    }

    /// Append everything the UI sent since last cycle to the input sequences.
    fn deliver_ui_messages(&mut self) {
        while let Some((port_index, len)) = read_message(&mut self.to_dsp, &mut self.message) {
            let Some(port) = self
                .atom_ports
                .iter_mut()
                .find(|p| p.is_input && p.index == port_index)
            else {
                continue;
            };

            // An event is a frame time (i64) followed by the atom, padded to 8 bytes
            let event = 8 + len.div_ceil(8) * 8;
            if port.fill + event > port.bytes() {
                continue;
            }

            unsafe {
                let at = port.ptr().add(port.fill);
                (at as *mut i64).write(0);
                ptr::copy_nonoverlapping(self.message.as_ptr(), at.add(8), len);
                port.fill += event;
                (port.ptr() as *mut u32).write((port.fill - ATOM_HEADER) as u32);
            }
        }
    }

    /// Forward the events the plugin produced this cycle to the UI thread.
    fn collect_plugin_messages(&mut self) {
        for port in &mut self.atom_ports {
            if port.is_input {
                continue;
            }

            let bytes = port.bytes();
            let base = port.ptr();
            unsafe {
                let size = (base as *const u32).read() as usize;
                let type_ = (base as *const u32).add(1).read();
                if type_ != self.sequence || size < SEQUENCE_BODY || ATOM_HEADER + size > bytes {
                    continue;
                }

                let end = ATOM_HEADER + size;
                let mut offset = ATOM_HEADER + SEQUENCE_BODY;
                while offset + 8 + ATOM_HEADER <= end {
                    let atom = base.add(offset + 8);
                    let atom_len = ATOM_HEADER + (atom as *const u32).read() as usize;
                    if offset + 8 + atom_len > end {
                        break;
                    }
                    let body = std::slice::from_raw_parts(atom, atom_len);
                    write_message(&mut self.from_dsp, port.index, body);
                    offset += 8 + atom_len.div_ceil(8) * 8;
                }
            }
        }
    }
}

const fn empty_option() -> OptionsOption {
    OptionsOption {
        context: 0,
        subject: 0,
        key: 0,
        size: 0,
        type_: 0,
        value: ptr::null(),
    }
}

/// Queue one atom for the other side. Returns false (dropping it) if the ring is full.
pub fn write_message(ring: &mut Producer<u8>, port: u32, atom: &[u8]) -> bool {
    let total = FRAME_HEADER + atom.len();
    let Ok(mut chunk) = ring.write_chunk(total) else {
        return false;
    };

    let mut header = [0u8; FRAME_HEADER];
    header[..4].copy_from_slice(&port.to_ne_bytes());
    header[4..].copy_from_slice(&(atom.len() as u32).to_ne_bytes());

    let (first, second) = chunk.as_mut_slices();
    for (at, byte) in header.iter().chain(atom.iter()).enumerate() {
        if at < first.len() {
            first[at] = *byte;
        } else {
            second[at - first.len()] = *byte;
        }
    }
    chunk.commit_all();
    true
}

/// Pop one atom into `into`, returning its port and length. Messages too
/// large for `into` are discarded.
pub fn read_message(ring: &mut Consumer<u8>, into: &mut [u8]) -> Option<(u32, usize)> {
    loop {
        let mut header = [0u8; FRAME_HEADER];
        {
            let chunk = ring.read_chunk(FRAME_HEADER).ok()?;
            copy_out(chunk.as_slices(), 0, &mut header);
            // Dropped without committing: this was only a peek
        }
        let port = u32::from_ne_bytes(header[..4].try_into().unwrap());
        let len = u32::from_ne_bytes(header[4..].try_into().unwrap()) as usize;

        // Writers commit a whole message at once, so the body is always there
        let chunk = ring.read_chunk(FRAME_HEADER + len).ok()?;
        if len > into.len() {
            chunk.commit_all();
            continue;
        }
        copy_out(chunk.as_slices(), FRAME_HEADER, &mut into[..len]);
        chunk.commit_all();
        return Some((port, len));
    }
}

fn copy_out(slices: (&[u8], &[u8]), skip: usize, into: &mut [u8]) {
    let (first, second) = slices;
    for (i, byte) in into.iter_mut().enumerate() {
        let at = skip + i;
        *byte = if at < first.len() {
            first[at]
        } else {
            second[at - first.len()]
        };
    }
}

pub fn load(controls: &[AtomicU32], port: u32) -> f32 {
    f32::from_bits(controls[port as usize].load(Ordering::Relaxed))
}

pub fn store(controls: &[AtomicU32], port: u32, value: f32) {
    controls[port as usize].store(value.to_bits(), Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_through_the_ring_including_wraparound() {
        let (mut tx, mut rx) = RingBuffer::new(64);
        let mut scratch = [0u8; 64];

        // Enough rounds that messages straddle the end of the ring
        for round in 0..20u8 {
            let atom: Vec<u8> = (0..24).map(|i| i + round).collect();
            assert!(write_message(&mut tx, 7 + round as u32, &atom));
            let (port, len) = read_message(&mut rx, &mut scratch).unwrap();
            assert_eq!(port, 7 + round as u32);
            assert_eq!(&scratch[..len], &atom[..]);
        }
        assert!(read_message(&mut rx, &mut scratch).is_none());
    }

    #[test]
    fn a_full_ring_drops_the_message_instead_of_corrupting_it() {
        let (mut tx, mut rx) = RingBuffer::new(40);
        let mut scratch = [0u8; 64];

        assert!(write_message(&mut tx, 1, &[1; 16]));
        assert!(!write_message(&mut tx, 2, &[2; 16]));

        let (port, len) = read_message(&mut rx, &mut scratch).unwrap();
        assert_eq!((port, len), (1, 16));
        assert!(read_message(&mut rx, &mut scratch).is_none());
    }

    #[test]
    fn oversized_messages_are_skipped() {
        let (mut tx, mut rx) = RingBuffer::new(128);
        let mut scratch = [0u8; 8];

        assert!(write_message(&mut tx, 1, &[1; 32]));
        assert!(write_message(&mut tx, 2, &[2; 8]));

        let (port, len) = read_message(&mut rx, &mut scratch).unwrap();
        assert_eq!((port, len), (2, 8));
    }
}
