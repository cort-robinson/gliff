//! The Windows GPU tier: one Direct3D 11 device the window and the decode
//! worker share, Media Foundation's H.264 decoder on it (hardware through
//! DXVA, in low-latency mode), and a small ring of NV12 textures the decoded
//! pictures are copied into for the window to draw.
//!
//! It stands in for `gliff_vk`'s `Gpu`, `Decoder` and `DisplayFrame`, so the
//! decode worker's GPU path drives it unchanged. The worker owns the
//! [`Decoder`]; the window's presenter draws each [`DisplayFrame`] with the
//! device's video processor. Both threads use the immediate context, so the
//! device is multithread-protected before it is shared.

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, bail, Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_BIND_DECODER, D3D11_BOX, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264DecoderMFT, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFDXGIBuffer,
    IMFDXGIDeviceManager, IMFSample, IMFTransform, MFCreateDXGIDeviceManager, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFStartup, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFSTARTUP_LITE,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SA_D3D11_AWARE, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::Variant::VARIANT;

/// Display textures: one being written, one on screen, one in transit.
const RING: usize = 3;

pub struct Gpu {
    /// The adapter, for the logs.
    pub name: String,
    pub(super) device: ID3D11Device,
    pub(super) context: ID3D11DeviceContext,
    ring: Mutex<Ring>,
}

// SAFETY: a D3D11 device is free-threaded, and the immediate context is
// multithread-protected in `Gpu::create` before the device leaves the
// creating thread, so calls from the window and the decode worker
// serialize inside the runtime.
unsafe impl Send for Gpu {}
// SAFETY: as for `Send`.
unsafe impl Sync for Gpu {}

static SHARED: OnceLock<Arc<Gpu>> = OnceLock::new();

#[derive(Default)]
struct Ring {
    size: (u32, u32),
    /// Bumped when the textures are replaced, so a frame from the old ring
    /// releases nothing in the new one.
    generation: u64,
    slots: Vec<(ID3D11Texture2D, bool)>,
}

impl Gpu {
    /// A hardware device with video support, multithread-protected.
    pub fn create() -> Result<Arc<Gpu>> {
        let mut device = None;
        let mut context = None;
        // SAFETY: plain device creation with out-pointers to locals.
        unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .context("create a Direct3D 11 device")?;
        let device: ID3D11Device = device.context("no Direct3D 11 device")?;
        let context: ID3D11DeviceContext = context.context("no immediate context")?;
        let multithread: ID3D11Multithread = context.cast()?;
        // SAFETY: a valid context; protection must be on before a second
        // thread uses it. The result is the previous setting.
        let _ = unsafe { multithread.SetMultithreadProtected(true) };
        // SAFETY: plain queries on the device just created.
        let name = unsafe {
            let desc = device.cast::<IDXGIDevice>()?.GetAdapter()?.GetDesc()?;
            let len = desc.Description.iter().position(|c| *c == 0).unwrap_or(128);
            String::from_utf16_lossy(&desc.Description[..len])
        };
        Ok(Arc::new(Gpu {
            name,
            device,
            context,
            ring: Mutex::default(),
        }))
    }

    /// Make `gpu` the device the decode worker uses.
    pub fn share(gpu: &Arc<Gpu>) {
        let _ = SHARED.set(gpu.clone());
    }

    /// The window's device, when it has one.
    pub fn shared() -> Option<Arc<Gpu>> {
        SHARED.get().cloned()
    }

    /// A free display texture of `size`, replacing the ring when the size
    /// changed.
    fn acquire(&self, size: (u32, u32)) -> Result<(usize, u64, ID3D11Texture2D)> {
        let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        if ring.size != size || ring.slots.is_empty() {
            let mut slots = Vec::with_capacity(RING);
            for _ in 0..RING {
                slots.push((self.nv12_texture(size)?, false));
            }
            ring.slots = slots;
            ring.size = size;
            ring.generation += 1;
        }
        let generation = ring.generation;
        let (index, slot) = ring
            .slots
            .iter_mut()
            .enumerate()
            .find(|(_, (_, busy))| !*busy)
            .ok_or_else(|| anyhow!("every display texture is still in use"))?;
        slot.1 = true;
        Ok((index, generation, slot.0.clone()))
    }

    fn release(&self, index: usize, generation: u64) {
        let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        if ring.generation == generation {
            if let Some(slot) = ring.slots.get_mut(index) {
                slot.1 = false;
            }
        }
    }

    fn nv12_texture(&self, (width, height): (u32, u32)) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            // The video processor takes input views only of textures a
            // decoder could write.
            BindFlags: D3D11_BIND_DECODER.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: a valid description and an out-pointer to a local.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .with_context(|| format!("create a {width}x{height} NV12 texture"))?;
        texture.context("no texture")
    }
}

/// A decoded picture in one of the ring's NV12 textures. Dropping it frees
/// the texture for the decoder, so the window keeps it while it is shown.
pub struct DisplayFrame {
    gpu: Arc<Gpu>,
    index: usize,
    generation: u64,
    pub(super) texture: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
}

// SAFETY: the texture belongs to the shared, multithread-protected device
// (see `Gpu`); the frame moves from the decode worker to the window.
unsafe impl Send for DisplayFrame {}

impl Drop for DisplayFrame {
    fn drop(&mut self) {
        self.gpu.release(self.index, self.generation);
    }
}

/// Media Foundation's H.264 decoder on the shared device.
pub struct Decoder {
    gpu: Arc<Gpu>,
    mft: IMFTransform,
    _manager: IMFDXGIDeviceManager,
    size: (u32, u32),
    /// Sample timestamps; only their order matters to the decoder.
    pts: i64,
    /// Access units fed before the first picture came out, for the log: one
    /// means the decoder shows pictures as soon as they are decoded.
    first_picture: Option<i64>,
}

impl Decoder {
    pub fn new(gpu: &Arc<Gpu>, dual: bool, width: u32, height: u32) -> Result<Self> {
        if dual {
            // 4:4:4 needs a second stream and the recombine; the CPU tier
            // has both.
            bail!("4:4:4 (two streams) is decoded on the CPU");
        }
        start_media_foundation()?;
        // SAFETY: COM calls on objects created here; every pointer passed
        // is to a live local or to the shared device.
        unsafe {
            let mft: IMFTransform =
                CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)
                    .context("create the H.264 decoder")?;
            let attributes = mft.GetAttributes()?;
            if attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                bail!("the H.264 decoder cannot decode on Direct3D 11");
            }
            // Low-latency mode, best effort. On its own it did not stop the
            // decoder holding 16 pictures; declaring no reordering in the
            // SPS does (see `sps`).
            if let Err(e) = mft
                .cast::<ICodecAPI>()
                .and_then(|c| c.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(1u32)))
            {
                tracing::debug!(error = %e, "the H.264 decoder has no low-latency mode");
            }
            let mut token = 0;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager: IMFDXGIDeviceManager = manager.context("no DXGI device manager")?;
            manager.ResetDevice(&gpu.device, token)?;
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .context("hand the decoder the Direct3D 11 device")?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            input.SetUINT64(
                &MF_MT_FRAME_SIZE,
                (u64::from(width) << 32) | u64::from(height),
            )?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            mft.SetInputType(0, &input, 0)
                .with_context(|| format!("decode {width}x{height} H.264"))?;
            set_nv12_output(&mft)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Self {
                gpu: gpu.clone(),
                mft,
                _manager: manager,
                size: (width, height),
                pts: 0,
                first_picture: None,
            })
        }
    }

    /// The window scales on the GPU, so the decoder never zooms.
    pub fn zoom(&self) -> u32 {
        1
    }

    pub fn set_zoom(&mut self, _zoom: u32) -> Result<()> {
        Ok(())
    }

    /// Nothing to redraw: the window still holds the last picture.
    pub fn redraw(&mut self) -> Result<Option<DisplayFrame>> {
        Ok(None)
    }

    /// Decode one access unit (`aux` is always empty: Single420 only).
    pub fn decode(&mut self, main: &[u8], _aux: &[u8]) -> Result<Option<DisplayFrame>> {
        let patched = super::sps::declare_no_reordering(main);
        let sample = self.sample(patched.as_deref().unwrap_or(main))?;
        // SAFETY: a COM call with a sample created here.
        match unsafe { self.mft.ProcessInput(0, &sample, 0) } {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_NOTACCEPTING => {
                // A picture is still waiting to come out; take it first.
                let earlier = self.drain()?;
                // SAFETY: as above.
                unsafe { self.mft.ProcessInput(0, &sample, 0) }?;
                return Ok(self.drain()?.or(earlier));
            }
            Err(e) => return Err(e.into()),
        }
        self.drain()
    }

    fn sample(&mut self, data: &[u8]) -> Result<IMFSample> {
        let len = u32::try_from(data.len()).context("access unit too large")?;
        // SAFETY: the buffer is locked for exactly `len` bytes while the
        // access unit is copied in; the sample owns the buffer after.
        unsafe {
            let buffer = MFCreateMemoryBuffer(len)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(len)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.pts)?;
            sample.SetSampleDuration(1)?;
            self.pts += 1;
            Ok(sample)
        }
    }

    /// Every picture the decoder has ready; the newest is returned and older
    /// ones are released.
    fn drain(&mut self) -> Result<Option<DisplayFrame>> {
        let mut latest = None;
        loop {
            let mut out = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0;
            // SAFETY: the decoder provides the samples; the slots start
            // empty and whatever it filled is taken and dropped below.
            let result = unsafe { self.mft.ProcessOutput(0, &mut out, &mut status) };
            // SAFETY: each field is taken exactly once.
            let (sample, _events) = unsafe {
                (
                    ManuallyDrop::take(&mut out[0].pSample),
                    ManuallyDrop::take(&mut out[0].pEvents),
                )
            };
            match result {
                Ok(()) => {
                    if let Some(sample) = sample {
                        latest = Some(self.copy_out(&sample)?);
                        if self.first_picture.is_none() {
                            self.first_picture = Some(self.pts);
                            tracing::debug!(
                                access_units = self.pts,
                                "first picture out of the decoder"
                            );
                        }
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(latest),
                // SAFETY: renegotiates the output on the decoder we own.
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => unsafe {
                    set_nv12_output(&self.mft)?
                },
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Copy the decoder's picture into a display texture, so the decoder's
    /// own surface goes straight back to it.
    fn copy_out(&self, sample: &IMFSample) -> Result<DisplayFrame> {
        let (width, height) = self.size;
        // SAFETY: the buffer of a sample the D3D11-aware decoder produced
        // is a DXGI buffer over one of its textures; the copy is queued on
        // the same immediate context as the decode, so it runs after it.
        unsafe {
            let dxgi: IMFDXGIBuffer = sample.GetBufferByIndex(0)?.cast()?;
            let mut raw: *mut c_void = std::ptr::null_mut();
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)?;
            let source = ID3D11Texture2D::from_raw(raw);
            let subresource = dxgi.GetSubresourceIndex()?;
            let (index, generation, texture) = self.gpu.acquire(self.size)?;
            self.gpu.context.CopySubresourceRegion(
                &texture,
                0,
                0,
                0,
                0,
                &source,
                subresource,
                Some(&D3D11_BOX {
                    left: 0,
                    top: 0,
                    front: 0,
                    right: width,
                    bottom: height,
                    back: 1,
                }),
            );
            Ok(DisplayFrame {
                gpu: self.gpu.clone(),
                index,
                generation,
                texture,
                width,
                height,
            })
        }
    }
}

/// Ask the decoder for NV12, on creation and after a stream change.
///
/// # Safety
/// `mft` must be a live decoder whose input type is set.
unsafe fn set_nv12_output(mft: &IMFTransform) -> Result<()> {
    for index in 0.. {
        // SAFETY: the caller's decoder; an index past the last type errors.
        let Ok(kind) = (unsafe { mft.GetOutputAvailableType(0, index) }) else {
            break;
        };
        // SAFETY: plain attribute read and type set on the decoder.
        unsafe {
            if kind.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                mft.SetOutputType(0, &kind, 0)?;
                return Ok(());
            }
        }
    }
    bail!("the H.264 decoder offers no NV12 output")
}

/// COM on this thread, and Media Foundation once per process.
fn start_media_foundation() -> Result<()> {
    // SAFETY: joins (or creates) this thread's multithreaded apartment; a
    // thread already in an apartment keeps it.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    static STARTED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    STARTED
        // SAFETY: process-wide startup, run once.
        .get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }.map_err(|e| e.to_string()))
        .clone()
        .map_err(|e| anyhow!("start Media Foundation: {e}"))
}
