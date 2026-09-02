//! macOS 14.4+ system-output capture using documented CoreAudio process taps.
//!
//! The containing application must provide `NSAudioCaptureUsageDescription`.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[cfg(not(target_os = "macos"))]
compile_error!("sori-system-audio-cleanroom supports only macOS");
use anyhow::{anyhow, bail, Context, Result};
use objc2::AnyThread;
use objc2_core_audio::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioHardwarePropertyTranslatePIDToProcessObject,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
    kAudioSubTapUIDKey, kAudioTapPropertyFormat, AudioConvertHostTimeToNanos,
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart,
    AudioDeviceStop, AudioGetCurrentHostTime, AudioHardwareCreateAggregateDevice,
    AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
    AudioHardwareDestroyProcessTap, AudioObjectAddPropertyListener, AudioObjectGetPropertyData,
    AudioObjectID, AudioObjectPropertyAddress, AudioObjectRemovePropertyListener, CATapDescription,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsBigEndian, kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved,
    kAudioFormatFlagIsPacked, kAudioFormatLinearPCM, AudioBuffer, AudioBufferList,
    AudioStreamBasicDescription, AudioTimeStamp, AudioTimeStampFlags,
};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_foundation::{NSArray, NSNumber, NSString, NSUUID};
use std::cell::{Cell, RefCell, UnsafeCell};
use std::ffi::{c_void, CStr};
use std::marker::PhantomData;
use std::mem::{size_of, MaybeUninit};
use std::ptr::{self, NonNull};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
const BLOCK_FRAMES: usize = 4_096;
const BLOCK_SLOTS: usize = 32;
const CALLBACK_ERROR_NONE: u32 = 0;
const CALLBACK_ERROR_TIMESTAMP: u32 = 1;
const CALLBACK_ERROR_BUFFER_LAYOUT: u32 = 2;
const CALLBACK_ERROR_NULL_DATA: u32 = 3;
const CALLBACK_ERROR_FORMAT_CHANGED: u32 = 4;
/// A captured mono PCM block.
pub struct CapturedChunk {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub captured_host_nanos: u64,
}

/// Return the current CoreAudio host clock in nanoseconds.
pub fn current_host_nanos() -> u64 {
    // SAFETY: Both functions have no preconditions and operate on the shared
    // monotonic CoreAudio host clock.
    unsafe { AudioConvertHostTimeToNanos(AudioGetCurrentHostTime()) }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PcmLayout {
    Interleaved { channels: usize },
    Planar { channels: usize },
}
impl PcmLayout {
    fn channels(self) -> usize {
        match self {
            Self::Interleaved { channels } | Self::Planar { channels } => channels,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Format {
    sample_rate: u32,
    layout: PcmLayout,
}
struct AudioBlock {
    samples: [f32; BLOCK_FRAMES],
    len: usize,
    host_ticks: u64,
    callback_frame_offset: u64,
}
impl AudioBlock {
    fn empty() -> Self {
        Self {
            samples: [0.0; BLOCK_FRAMES],
            len: 0,
            host_ticks: 0,
            callback_frame_offset: 0,
        }
    }
}
struct Slot(UnsafeCell<AudioBlock>);
struct BlockRing {
    slots: Box<[Slot; BLOCK_SLOTS]>,
    producer: AtomicUsize,
    consumer: AtomicUsize,
}
impl BlockRing {
    fn new() -> Self {
        let slots = (0..BLOCK_SLOTS)
            .map(|_| Slot(UnsafeCell::new(AudioBlock::empty())))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let slots = match slots.try_into() {
            Ok(slots) => slots,
            Err(_) => unreachable!("BLOCK_SLOTS elements were allocated"),
        };
        Self {
            slots,
            producer: AtomicUsize::new(0),
            consumer: AtomicUsize::new(0),
        }
    }
    /// Producer-side access. Only the single CoreAudio IOProc may call this.
    unsafe fn try_begin_push(&self) -> Option<(*mut AudioBlock, usize)> {
        let producer = self.producer.load(Ordering::Relaxed);
        let next = (producer + 1) % BLOCK_SLOTS;
        if next == self.consumer.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: `producer` is always reduced modulo BLOCK_SLOTS. Only the
        // single producer writes this slot until `finish_push` publishes it.
        let slot = unsafe { self.slots.get_unchecked(producer) };
        Some((slot.0.get(), next))
    }
    fn finish_push(&self, next: usize) {
        self.producer.store(next, Ordering::Release);
    }
    fn pop(&self, sample_rate: u32) -> Option<CapturedChunk> {
        let consumer = self.consumer.load(Ordering::Relaxed);
        if consumer == self.producer.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: Acquire observed producer publication. The producer cannot
        // reuse this slot until the consumer index is advanced below.
        let block = unsafe { &*self.slots[consumer].0.get() };
        let mut samples = Vec::with_capacity(block.len);
        samples.extend_from_slice(&block.samples[..block.len]);
        // Host-time conversion and Vec allocation deliberately happen off the
        // real-time callback.
        // SAFETY: AudioConvertHostTimeToNanos accepts every u64 host tick value.
        let base_nanos = unsafe { AudioConvertHostTimeToNanos(block.host_ticks) };
        let captured_host_nanos =
            timestamp_for_frame(base_nanos, block.callback_frame_offset, sample_rate);
        self.consumer
            .store((consumer + 1) % BLOCK_SLOTS, Ordering::Release);
        Some(CapturedChunk {
            samples,
            sample_rate,
            captured_host_nanos,
        })
    }
}
struct CallbackState {
    format: Format,
    ring: BlockRing,
    dropped: AtomicU64,
    error: AtomicU32,
    format_changed: AtomicBool,
}
impl CallbackState {
    fn new(format: Format) -> Self {
        Self {
            format,
            ring: BlockRing::new(),
            dropped: AtomicU64::new(0),
            error: AtomicU32::new(CALLBACK_ERROR_NONE),
            format_changed: AtomicBool::new(false),
        }
    }
    fn report_error(&self, code: u32, dropped_frames: usize) {
        let _ = self.error.compare_exchange(
            CALLBACK_ERROR_NONE,
            code,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        self.dropped
            .fetch_add(dropped_frames as u64, Ordering::Relaxed);
    }
}
/// Active system-output capture.
///
/// This type intentionally stays on the thread where it was opened. The
/// CoreAudio callback crosses threads only through the private atomic SPSC
/// queue and a stable raw pointer owned until IOProc destruction completes.
pub struct SystemCapture {
    sample_rate: u32,
    aggregate_id: Cell<AudioObjectID>,
    tap_id: Cell<AudioObjectID>,
    io_proc: Cell<AudioDeviceIOProcID>,
    format_listener_registered: Cell<bool>,
    callback: RefCell<Option<Box<CallbackState>>>,
    stopped: Cell<bool>,
    _not_send_or_sync: PhantomData<Rc<()>>,
}
impl SystemCapture {
    /// Open a global mono output tap, excluding this process when CoreAudio can
    /// resolve its process AudioObject.
    pub fn open() -> Result<Self> {
        ensure_supported_macos()?;
        let process_number = current_process_object_id()
            .ok()
            .flatten()
            .map(NSNumber::new_u32);
        let processes = match process_number.as_deref() {
            Some(number) => NSArray::from_slice(&[number]),
            None => NSArray::from_slice(&[]),
        };
        // SAFETY: CATapDescription owns a copied NSArray and the designated
        // initializer is valid on the checked macOS release.
        let description = unsafe {
            CATapDescription::initMonoGlobalTapButExcludeProcesses(
                CATapDescription::alloc(),
                &processes,
            )
        };
        let tap_uuid = NSUUID::UUID();
        let tap_name = NSString::from_str(&format!("sori-cleanroom-tap-{}", tap_uuid.UUIDString()));
        // SAFETY: All values are valid Foundation objects retained/copied by
        // CATapDescription.
        unsafe {
            description.setUUID(&tap_uuid);
            description.setName(&tap_name);
            description.setPrivate(true);
        }
        let mut capture = Self {
            sample_rate: 0,
            aggregate_id: Cell::new(0),
            tap_id: Cell::new(0),
            io_proc: Cell::new(None),
            format_listener_registered: Cell::new(false),
            callback: RefCell::new(None),
            stopped: Cell::new(false),
            _not_send_or_sync: PhantomData,
        };
        let mut tap_id = 0;
        status(
            // SAFETY: `description` is initialized and `tap_id` is writable.
            unsafe { AudioHardwareCreateProcessTap(Some(&description), &mut tap_id) },
            "AudioHardwareCreateProcessTap",
        )?;
        capture.tap_id.set(tap_id);
        let asbd: AudioStreamBasicDescription =
            get_property(tap_id, kAudioTapPropertyFormat, None)?;
        let format = validate_asbd(&asbd).context("unsupported tap PCM format")?;
        capture.sample_rate = format.sample_rate;
        let aggregate_uuid = NSUUID::UUID().UUIDString().to_string();
        let aggregate_name = format!("sori-cleanroom-aggregate-{aggregate_uuid}");
        let aggregate_description = aggregate_dictionary(
            &aggregate_uuid,
            &aggregate_name,
            &tap_uuid.UUIDString().to_string(),
        )?;
        let mut aggregate_id = 0;
        status(
            // SAFETY: The dictionary follows the documented aggregate schema
            // and `aggregate_id` is writable.
            unsafe {
                AudioHardwareCreateAggregateDevice(
                    &aggregate_description,
                    NonNull::from(&mut aggregate_id),
                )
            },
            "AudioHardwareCreateAggregateDevice",
        )?;
        capture.aggregate_id.set(aggregate_id);
        let mut callback = Box::new(CallbackState::new(format));
        let callback_ptr = (&mut *callback as *mut CallbackState).cast::<c_void>();
        *capture.callback.borrow_mut() = Some(callback);

        let mut io_proc = None;
        status(
            // SAFETY: callback_ptr remains stable until IOProc destruction and
            // `io_proc` is writable.
            unsafe {
                AudioDeviceCreateIOProcID(
                    aggregate_id,
                    Some(io_proc_callback),
                    callback_ptr,
                    NonNull::from(&mut io_proc),
                )
            },
            "AudioDeviceCreateIOProcID",
        )?;
        if io_proc.is_none() {
            bail!("AudioDeviceCreateIOProcID returned a null IOProc ID");
        }
        capture.io_proc.set(io_proc);
        // kAudioAggregateDeviceTapAutoStartKey is explicitly zero in the
        // aggregate dictionary. Apple documents that a non-zero value can wait
        // for the first tapped audio. AudioDeviceStart success is therefore the
        // finite readiness boundary; silence is not treated as a failure.
        status(
            // SAFETY: `io_proc` was created for this aggregate and its callback
            // storage is now owned by `capture`.
            unsafe { AudioDeviceStart(aggregate_id, io_proc) },
            "AudioDeviceStart",
        )?;

        let format_address = property_address(kAudioTapPropertyFormat);
        status(
            // SAFETY: callback_ptr remains stable in `capture.callback` until
            // the listener is removed or deliberately leaked on removal error.
            unsafe {
                AudioObjectAddPropertyListener(
                    tap_id,
                    NonNull::from(&format_address),
                    Some(tap_format_changed),
                    callback_ptr,
                )
            },
            "AudioObjectAddPropertyListener(kAudioTapPropertyFormat)",
        )?;
        capture.format_listener_registered.set(true);
        let current_asbd: AudioStreamBasicDescription =
            get_property(tap_id, kAudioTapPropertyFormat, None)?;
        let current_format = validate_asbd(&current_asbd).context("unsupported tap PCM format")?;
        if current_format != format {
            bail!("system audio format changed while capture was starting");
        }
        Ok(capture)
    }
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    pub fn pop_chunk(&self) -> Option<CapturedChunk> {
        self.callback
            .borrow()
            .as_ref()
            .and_then(|callback| callback.ring.pop(self.sample_rate))
    }
    pub fn take_dropped(&self) -> u64 {
        self.callback
            .borrow()
            .as_ref()
            .map_or(0, |callback| callback.dropped.swap(0, Ordering::Relaxed))
    }
    pub fn take_error(&self) -> Option<String> {
        let code = self
            .callback
            .borrow()
            .as_ref()
            .map_or(CALLBACK_ERROR_NONE, |callback| {
                callback.error.swap(CALLBACK_ERROR_NONE, Ordering::Relaxed)
            });
        match code {
            CALLBACK_ERROR_NONE => None,
            CALLBACK_ERROR_TIMESTAMP => {
                Some("CoreAudio input timestamp did not contain valid host time".into())
            }
            CALLBACK_ERROR_BUFFER_LAYOUT => {
                Some("CoreAudio buffer list did not match the tap ASBD".into())
            }
            CALLBACK_ERROR_NULL_DATA => Some("CoreAudio supplied a null input buffer".into()),
            CALLBACK_ERROR_FORMAT_CHANGED => {
                Some("System audio format changed during recording; start a new recording".into())
            }
            _ => Some("unknown CoreAudio callback error".into()),
        }
    }
    /// Stop capture and release objects in callback-safe order.
    pub fn stop(&self) -> Result<()> {
        if self.stopped.replace(true) {
            return Ok(());
        }
        let aggregate_id = self.aggregate_id.replace(0);
        let io_proc = self.io_proc.replace(None);
        let tap_id = self.tap_id.replace(0);
        let mut errors = None;
        let mut callback_must_leak = false;
        if aggregate_id != 0 && io_proc.is_some() {
            collect_status(
                // SAFETY: This IOProc ID belongs to `aggregate_id` and remains
                // registered until the following destroy call.
                unsafe { AudioDeviceStop(aggregate_id, io_proc) },
                "AudioDeviceStop",
                &mut errors,
            );
            // SAFETY: This IOProc ID was created on `aggregate_id`.
            let destroy_status = unsafe { AudioDeviceDestroyIOProcID(aggregate_id, io_proc) };
            collect_status(destroy_status, "AudioDeviceDestroyIOProcID", &mut errors);
            if destroy_status != 0 {
                callback_must_leak = true;
            }
        }
        if tap_id != 0 && self.format_listener_registered.replace(false) {
            let format_address = property_address(kAudioTapPropertyFormat);
            // SAFETY: the listener was registered with this tap and the same
            // stable callback pointer.
            let remove_status = unsafe {
                AudioObjectRemovePropertyListener(
                    tap_id,
                    NonNull::from(&format_address),
                    Some(tap_format_changed),
                    self.callback
                        .borrow()
                        .as_ref()
                        .map_or(ptr::null_mut(), |callback| {
                            (&**callback as *const CallbackState).cast_mut().cast()
                        }),
                )
            };
            collect_status(
                remove_status,
                "AudioObjectRemovePropertyListener(kAudioTapPropertyFormat)",
                &mut errors,
            );
            if remove_status != 0 {
                callback_must_leak = true;
            }
        }
        if aggregate_id != 0 {
            collect_status(
                // SAFETY: This process created and still owns the aggregate ID.
                unsafe { AudioHardwareDestroyAggregateDevice(aggregate_id) },
                "AudioHardwareDestroyAggregateDevice",
                &mut errors,
            );
        }
        if tap_id != 0 {
            collect_status(
                // SAFETY: This process created and still owns the tap ID.
                unsafe { AudioHardwareDestroyProcessTap(tap_id) },
                "AudioHardwareDestroyProcessTap",
                &mut errors,
            );
        }
        if callback_must_leak {
            if let Some(callback) = self.callback.borrow_mut().take() {
                // A failed IOProc/listener removal can leave CoreAudio holding
                // the raw pointer. Leak rather than free it.
                let _ = Box::leak(callback);
            }
        }
        errors.map_or(Ok(()), |message| Err(anyhow!(message)))
    }
}
impl Drop for SystemCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

unsafe extern "C-unwind" fn tap_format_changed(
    _object: AudioObjectID,
    number_addresses: u32,
    addresses: NonNull<AudioObjectPropertyAddress>,
    client_data: *mut c_void,
) -> i32 {
    if client_data.is_null() {
        return 0;
    }
    // SAFETY: registration stores a stable CallbackState pointer until this
    // listener is removed or deliberately leaked on removal failure.
    let state = unsafe { &*client_data.cast::<CallbackState>() };
    let mut index = 0usize;
    while index < number_addresses as usize {
        // SAFETY: CoreAudio supplies `number_addresses` valid entries.
        let address = unsafe { &*addresses.as_ptr().add(index) };
        if address.mSelector == kAudioTapPropertyFormat {
            state.format_changed.store(true, Ordering::Release);
            state
                .error
                .store(CALLBACK_ERROR_FORMAT_CHANGED, Ordering::Release);
            break;
        }
        index += 1;
    }
    0
}

unsafe extern "C-unwind" fn io_proc_callback(
    _device: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input_data: NonNull<AudioBufferList>,
    input_time: NonNull<AudioTimeStamp>,
    _output_data: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client_data: *mut c_void,
) -> i32 {
    if client_data.is_null() {
        return 0;
    }
    // SAFETY: `client_data` points to the stable CallbackState Box installed
    // before AudioDeviceStart. It remains allocated until IOProc destruction.
    let state = unsafe { &*client_data.cast::<CallbackState>() };
    // SAFETY: CoreAudio guarantees valid timestamp and AudioBufferList pointers
    // for the duration of this call. `process_input` is written to avoid panic,
    // allocation, locking, blocking, logging, or unwinding.
    unsafe { process_input(state, input_data.as_ptr(), input_time.as_ptr()) };
    0
}
unsafe fn process_input(
    state: &CallbackState,
    input_data: *const AudioBufferList,
    input_time: *const AudioTimeStamp,
) {
    if state.format_changed.load(Ordering::Acquire) {
        return;
    }
    // SAFETY: Caller's CoreAudio callback contract keeps both pointers valid.
    let timestamp = unsafe { &*input_time };
    // SAFETY: Caller's CoreAudio callback contract keeps this ABL valid.
    let list = unsafe { &*input_data };
    let channels = state.format.layout.channels();
    if channels == 0 {
        state.report_error(CALLBACK_ERROR_BUFFER_LAYOUT, 0);
        return;
    }
    let (frames, buffers) = match state.format.layout {
        PcmLayout::Interleaved { channels } => {
            if list.mNumberBuffers != 1 {
                state.report_error(CALLBACK_ERROR_BUFFER_LAYOUT, 0);
                return;
            }
            let buffer = &list.mBuffers[0];
            let samples = (buffer.mDataByteSize as usize) / size_of::<f32>();
            if buffer.mNumberChannels as usize != channels
                || samples.checked_mul(size_of::<f32>()) != Some(buffer.mDataByteSize as usize)
                || samples % channels != 0
            {
                state.report_error(CALLBACK_ERROR_BUFFER_LAYOUT, 0);
                return;
            }
            (samples / channels, ptr::null())
        }
        PcmLayout::Planar { channels } => {
            if list.mNumberBuffers as usize != channels {
                state.report_error(CALLBACK_ERROR_BUFFER_LAYOUT, 0);
                return;
            }
            let first = &list.mBuffers[0];
            let frames = (first.mDataByteSize as usize) / size_of::<f32>();
            if frames.checked_mul(size_of::<f32>()) != Some(first.mDataByteSize as usize) {
                state.report_error(CALLBACK_ERROR_BUFFER_LAYOUT, 0);
                return;
            }
            // AudioBufferList is a count followed by a variable-length buffer
            // array. Obtain the first element rather than assuming the struct
            // itself has AudioBuffer alignment at byte zero.
            // SAFETY: CoreAudio supplied a valid AudioBufferList pointer.
            let buffers = unsafe { ptr::addr_of!((*input_data).mBuffers) }.cast::<AudioBuffer>();
            let mut channel = 0usize;
            while channel < channels {
                // SAFETY: mNumberBuffers was checked to equal `channels`.
                let buffer = unsafe { &*buffers.add(channel) };
                if buffer.mNumberChannels != 1
                    || buffer.mDataByteSize as usize != frames * size_of::<f32>()
                {
                    state.report_error(CALLBACK_ERROR_BUFFER_LAYOUT, frames);
                    return;
                }
                channel += 1;
            }
            (frames, buffers)
        }
    };
    if frames == 0 {
        return;
    }
    // Avoid bitflag helpers here: raw bit inspection is trivially non-panicking.
    if timestamp.mFlags.0 & AudioTimeStampFlags::HostTimeValid.0 == 0 {
        state.report_error(CALLBACK_ERROR_TIMESTAMP, frames);
        return;
    }
    let mut frame_offset = 0usize;
    while frame_offset < frames {
        let chunk_len = (frames - frame_offset).min(BLOCK_FRAMES);
        // SAFETY: Only this IOProc is the ring producer.
        let Some((block_ptr, next)) = (unsafe { state.ring.try_begin_push() }) else {
            state.dropped.fetch_add(chunk_len as u64, Ordering::Relaxed);
            frame_offset += chunk_len;
            continue;
        };
        // SAFETY: The producer owns this unpublished slot exclusively.
        let block = unsafe { &mut *block_ptr };
        block.len = chunk_len;
        block.host_ticks = timestamp.mHostTime;
        block.callback_frame_offset = frame_offset as u64;
        let copied = match state.format.layout {
            // SAFETY: the interleaved ABL shape and byte count were validated
            // above, and the destination has BLOCK_FRAMES elements.
            PcmLayout::Interleaved { channels } => unsafe {
                copy_interleaved_mono(
                    &mut block.samples,
                    chunk_len,
                    frame_offset,
                    channels,
                    &*ptr::addr_of!((*input_data).mBuffers[0]),
                )
            },
            // SAFETY: each planar buffer and its byte count were validated
            // above, and the destination has BLOCK_FRAMES elements.
            PcmLayout::Planar { channels } => unsafe {
                copy_planar_mono(
                    &mut block.samples,
                    chunk_len,
                    frame_offset,
                    channels,
                    buffers,
                )
            },
        };
        if !copied {
            state.report_error(CALLBACK_ERROR_NULL_DATA, frames - frame_offset);
            return;
        }
        state.ring.finish_push(next);
        frame_offset += chunk_len;
    }
}
unsafe fn copy_interleaved_mono(
    output: &mut [f32; BLOCK_FRAMES],
    frames: usize,
    source_frame: usize,
    channels: usize,
    buffer: &AudioBuffer,
) -> bool {
    if buffer.mData.is_null() {
        return false;
    }
    let source = buffer.mData.cast::<f32>().cast_const();
    let scale = 1.0 / channels as f32;
    let mut frame = 0usize;
    while frame < frames {
        let base = (source_frame + frame) * channels;
        let mut sum = 0.0;
        let mut channel = 0usize;
        while channel < channels {
            // SAFETY: ASBD and mDataByteSize validation proved this sample is
            // within the callback buffer.
            sum += unsafe { *source.add(base + channel) };
            channel += 1;
        }
        // SAFETY: frames is capped at BLOCK_FRAMES.
        unsafe { *output.get_unchecked_mut(frame) = sum * scale };
        frame += 1;
    }
    true
}
unsafe fn copy_planar_mono(
    output: &mut [f32; BLOCK_FRAMES],
    frames: usize,
    source_frame: usize,
    channels: usize,
    buffers: *const AudioBuffer,
) -> bool {
    let scale = 1.0 / channels as f32;
    let mut frame = 0usize;
    while frame < frames {
        let mut sum = 0.0;
        let mut channel = 0usize;
        while channel < channels {
            // SAFETY: caller verified `channels` buffers in the ABL.
            let buffer = unsafe { &*buffers.add(channel) };
            if buffer.mData.is_null() {
                return false;
            }
            let source = buffer.mData.cast::<f32>().cast_const();
            // SAFETY: per-buffer size validation proved this frame exists.
            sum += unsafe { *source.add(source_frame + frame) };
            channel += 1;
        }
        // SAFETY: frames is capped at BLOCK_FRAMES.
        unsafe { *output.get_unchecked_mut(frame) = sum * scale };
        frame += 1;
    }
    true
}
fn validate_asbd(asbd: &AudioStreamBasicDescription) -> Result<Format> {
    if asbd.mFormatID != kAudioFormatLinearPCM {
        bail!("format is not linear PCM");
    }
    if asbd.mFormatFlags & kAudioFormatFlagIsFloat == 0 {
        bail!("PCM is not floating-point");
    }
    if asbd.mFormatFlags & kAudioFormatFlagIsPacked == 0 {
        bail!("PCM is not packed");
    }
    if asbd.mFormatFlags & kAudioFormatFlagIsBigEndian != 0 {
        bail!("big-endian PCM is unsupported");
    }
    if asbd.mBitsPerChannel != 32 {
        bail!("expected f32 PCM, got {} bits", asbd.mBitsPerChannel);
    }
    if !asbd.mSampleRate.is_finite()
        || asbd.mSampleRate <= 0.0
        || asbd.mSampleRate > u32::MAX as f64
        || asbd.mSampleRate.fract() != 0.0
    {
        bail!("sample rate cannot be represented exactly as u32");
    }
    if asbd.mFramesPerPacket != 1 {
        bail!("expected one PCM frame per packet");
    }
    let channels = asbd.mChannelsPerFrame as usize;
    if channels == 0 {
        bail!("tap has zero channels");
    }
    let non_interleaved = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;
    let expected_bytes_per_frame = if non_interleaved {
        size_of::<f32>()
    } else {
        channels
            .checked_mul(size_of::<f32>())
            .ok_or_else(|| anyhow!("channel count overflows bytes per frame"))?
    };
    if asbd.mBytesPerFrame as usize != expected_bytes_per_frame
        || asbd.mBytesPerPacket as usize != expected_bytes_per_frame
    {
        bail!("ASBD byte strides do not describe packed f32 PCM");
    }
    let layout = if non_interleaved {
        PcmLayout::Planar { channels }
    } else {
        PcmLayout::Interleaved { channels }
    };
    Ok(Format {
        sample_rate: asbd.mSampleRate as u32,
        layout,
    })
}
fn timestamp_for_frame(base_nanos: u64, frame_offset: u64, sample_rate: u32) -> u64 {
    if sample_rate == 0 {
        return base_nanos;
    }
    let offset = (frame_offset as u128 * 1_000_000_000u128) / sample_rate as u128;
    base_nanos.saturating_add(offset.min(u64::MAX as u128) as u64)
}
fn current_process_object_id() -> Result<Option<AudioObjectID>> {
    // SAFETY: getpid has no preconditions.
    let pid = unsafe { libc::getpid() };
    let id: AudioObjectID = get_property(
        kAudioObjectSystemObject as AudioObjectID,
        kAudioHardwarePropertyTranslatePIDToProcessObject,
        Some((&pid as *const libc::pid_t).cast()),
    )?;
    Ok((id != 0).then_some(id))
}
fn property_address(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}
fn get_property<T: Copy>(
    object_id: AudioObjectID,
    selector: u32,
    qualifier: Option<*const c_void>,
) -> Result<T> {
    let address = property_address(selector);
    let mut size = size_of::<T>() as u32;
    let mut value = MaybeUninit::<T>::uninit();
    let qualifier_size = if qualifier.is_some() {
        size_of::<libc::pid_t>() as u32
    } else {
        0
    };
    status(
        // SAFETY: all pointers reference live stack storage for this call;
        // qualifier is either null or a pid_t pointer supplied above.
        unsafe {
            AudioObjectGetPropertyData(
                object_id,
                NonNull::from(&address),
                qualifier_size,
                qualifier.unwrap_or(ptr::null()),
                NonNull::from(&mut size),
                NonNull::new(value.as_mut_ptr().cast()).expect("MaybeUninit pointer is non-null"),
            )
        },
        "AudioObjectGetPropertyData",
    )?;
    if size as usize != size_of::<T>() {
        bail!(
            "AudioObjectGetPropertyData returned {size} bytes, expected {}",
            size_of::<T>()
        );
    }
    // SAFETY: CoreAudio returned success and exactly initialized sizeof(T).
    Ok(unsafe { value.assume_init() })
}
fn aggregate_dictionary(
    aggregate_uid: &str,
    aggregate_name: &str,
    tap_uid: &str,
) -> Result<CFRetained<CFDictionary>> {
    let tap_key = cf_key(kAudioSubTapUIDKey)?;
    let tap_uid = CFString::from_str(tap_uid);
    let tap_entry =
        CFDictionary::<CFString, CFType>::from_slices(&[tap_key.as_ref()], &[tap_uid.as_ref()]);
    let taps = CFArray::<CFDictionary<CFString, CFType>>::from_objects(&[tap_entry.as_ref()]);
    let uid_key = cf_key(kAudioAggregateDeviceUIDKey)?;
    let name_key = cf_key(kAudioAggregateDeviceNameKey)?;
    let private_key = cf_key(kAudioAggregateDeviceIsPrivateKey)?;
    let taps_key = cf_key(kAudioAggregateDeviceTapListKey)?;
    let auto_start_key = cf_key(kAudioAggregateDeviceTapAutoStartKey)?;
    let uid = CFString::from_str(aggregate_uid);
    let name = CFString::from_str(aggregate_name);
    let private = CFNumber::new_i32(1);
    let auto_start = CFNumber::new_i32(0);
    let dictionary = CFDictionary::<CFString, CFType>::from_slices(
        &[
            uid_key.as_ref(),
            name_key.as_ref(),
            private_key.as_ref(),
            taps_key.as_ref(),
            auto_start_key.as_ref(),
        ],
        &[
            uid.as_ref(),
            name.as_ref(),
            private.as_ref(),
            taps.as_ref(),
            auto_start.as_ref(),
        ],
    );
    // SAFETY: Erasing generic key/value markers does not change the CF object;
    // all stored keys and values are valid Core Foundation objects.
    Ok(unsafe { CFRetained::cast_unchecked::<CFDictionary>(dictionary) })
}
fn cf_key(value: &CStr) -> Result<CFRetained<CFString>> {
    Ok(CFString::from_str(
        value.to_str().context("CoreAudio key is not UTF-8")?,
    ))
}
fn status(code: i32, operation: &str) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        bail!("{operation} failed with OSStatus {code} ({})", fourcc(code))
    }
}
fn collect_status(code: i32, operation: &str, errors: &mut Option<String>) {
    if let Err(error) = status(code, operation) {
        let message = error.to_string();
        match errors {
            Some(errors) => {
                errors.push_str("; ");
                errors.push_str(&message);
            }
            None => *errors = Some(message),
        }
    }
}
fn fourcc(code: i32) -> String {
    let bytes = (code as u32).to_be_bytes();
    if bytes
        .iter()
        .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
    {
        format!("'{}'", String::from_utf8_lossy(&bytes))
    } else {
        "non-FourCC".into()
    }
}
fn ensure_supported_macos() -> Result<()> {
    let mut version = [0i8; 64];
    let mut len = version.len();
    let name = b"kern.osproductversion\0";
    // SAFETY: name is NUL-terminated, version is writable for `len` bytes,
    // and the remaining sysctl arguments request a read.
    let result = unsafe {
        libc::sysctlbyname(
            name.as_ptr().cast(),
            version.as_mut_ptr().cast(),
            &mut len,
            ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("read macOS version");
    }
    // SAFETY: sysctl initialized at most the 64-byte buffer and reports that
    // initialized length in `len`; the final byte is the documented NUL.
    let bytes =
        unsafe { std::slice::from_raw_parts(version.as_ptr().cast::<u8>(), len.saturating_sub(1)) };
    let text = std::str::from_utf8(bytes).context("macOS version is not UTF-8")?;
    let mut components = text.split('.');
    let major = components.next().and_then(|v| v.parse::<u32>().ok());
    let minor = components.next().and_then(|v| v.parse::<u32>().ok());
    if major.zip(minor).is_some_and(|v| v >= (14, 4)) {
        Ok(())
    } else {
        bail!("macOS 14.4 or newer is required (running {text})")
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn asbd(sample_rate: f64, channels: u32, non_interleaved: bool) -> AudioStreamBasicDescription {
        let bytes_per_frame = if non_interleaved { 4 } else { channels * 4 };
        AudioStreamBasicDescription {
            mSampleRate: sample_rate,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat
                | kAudioFormatFlagIsPacked
                | if non_interleaved {
                    kAudioFormatFlagIsNonInterleaved
                } else {
                    0
                },
            mBytesPerPacket: bytes_per_frame,
            mFramesPerPacket: 1,
            mBytesPerFrame: bytes_per_frame,
            mChannelsPerFrame: channels,
            mBitsPerChannel: 32,
            mReserved: 0,
        }
    }
    #[test]
    fn validates_exact_44k1_and_48k_layouts() {
        let interleaved = validate_asbd(&asbd(44_100.0, 2, false)).unwrap();
        assert_eq!(interleaved.sample_rate, 44_100);
        assert_eq!(interleaved.layout, PcmLayout::Interleaved { channels: 2 });
        let planar = validate_asbd(&asbd(48_000.0, 6, true)).unwrap();
        assert_eq!(planar.sample_rate, 48_000);
        assert_eq!(planar.layout, PcmLayout::Planar { channels: 6 });
    }
    #[test]
    fn rejects_non_f32_non_packed_and_bad_stride() {
        let mut value = asbd(48_000.0, 2, false);
        value.mFormatFlags &= !kAudioFormatFlagIsFloat;
        assert!(validate_asbd(&value).is_err());
        value = asbd(48_000.0, 2, false);
        value.mFormatFlags &= !kAudioFormatFlagIsPacked;
        assert!(validate_asbd(&value).is_err());
        value = asbd(48_000.0, 2, false);
        value.mBytesPerFrame = 4;
        assert!(validate_asbd(&value).is_err());
    }
    #[test]
    fn rejects_fractional_sample_rate() {
        assert!(validate_asbd(&asbd(44_100.5, 2, false)).is_err());
    }
    #[test]
    fn downmixes_interleaved_pcm() {
        let input = [1.0f32, 3.0, -1.0, 1.0];
        let buffer = AudioBuffer {
            mNumberChannels: 2,
            mDataByteSize: size_of_val(&input) as u32,
            mData: input.as_ptr().cast_mut().cast(),
        };
        let mut output = [0.0; BLOCK_FRAMES];
        // SAFETY: the test buffer points to four live f32 values.
        assert!(unsafe { copy_interleaved_mono(&mut output, 2, 0, 2, &buffer) });
        assert_eq!(&output[..2], &[2.0, 0.0]);
    }
    #[test]
    fn downmixes_planar_pcm() {
        let left = [1.0f32, -1.0];
        let right = [3.0f32, 1.0];
        let buffers = [
            AudioBuffer {
                mNumberChannels: 1,
                mDataByteSize: size_of_val(&left) as u32,
                mData: left.as_ptr().cast_mut().cast(),
            },
            AudioBuffer {
                mNumberChannels: 1,
                mDataByteSize: size_of_val(&right) as u32,
                mData: right.as_ptr().cast_mut().cast(),
            },
        ];
        let mut output = [0.0; BLOCK_FRAMES];
        // SAFETY: both test buffers point to two live f32 values.
        assert!(unsafe { copy_planar_mono(&mut output, 2, 0, 2, buffers.as_ptr()) });
        assert_eq!(&output[..2], &[2.0, 0.0]);
    }
    #[test]
    fn timestamps_block_first_frame_at_real_sample_rate() {
        let base = 9_000_000_000;
        assert_eq!(timestamp_for_frame(base, 4_410, 44_100), base + 100_000_000);
        assert_eq!(timestamp_for_frame(base, 4_800, 48_000), base + 100_000_000);
        assert_eq!(timestamp_for_frame(base, 1, 44_100), base + 22_675);
    }
    #[test]
    fn format_change_stops_delivery_and_reports_a_sticky_error() {
        let state = CallbackState::new(validate_asbd(&asbd(48_000.0, 2, false)).unwrap());
        let mut address = property_address(kAudioTapPropertyFormat);
        // SAFETY: the callback receives one valid address and a live state pointer.
        unsafe {
            tap_format_changed(
                1,
                1,
                NonNull::from(&mut address),
                (&state as *const CallbackState).cast_mut().cast(),
            );
        }
        assert!(state.format_changed.load(Ordering::Acquire));
        assert_eq!(
            state.error.load(Ordering::Acquire),
            CALLBACK_ERROR_FORMAT_CHANGED
        );
    }
}
