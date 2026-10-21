//! Draws the picture through a flip-model DXGI swap chain on the window.
//!
//! The device's video processor converts and scales each picture into the
//! back buffer in one pass: NV12 frames from the hardware decoder, or BGRA
//! frames from the CPU decoder after an upload. The text overlays (status,
//! statistics, hints) are drawn with Direct2D and DirectWrite on the same
//! device, and only when there is text: GDI on a flip-model back buffer
//! (`GetDC`) stalled the device for about 260 ms a frame.

use std::mem::ManuallyDrop;
use std::sync::Arc;

use anyhow::{Context, Result};
use windows::core::{w, Interface, BOOL};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DeviceContext, ID2D1Factory1, ID2D1SolidColorBrush,
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
    D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_MULTI_THREADED,
};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_RGBA,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_METRICS, DWRITE_WORD_WRAPPING_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN,
    DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGIFactory5, IDXGISurface, IDXGISwapChain1,
    DXGI_FEATURE_PRESENT_ALLOW_TEARING, DXGI_MWA_NO_ALT_ENTER, DXGI_PRESENT,
    DXGI_PRESENT_ALLOW_TEARING, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING, DXGI_SWAP_EFFECT_DISCARD, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

use super::gpu::{DisplayFrame, Gpu};

/// Where a label sits in the window.
#[derive(Clone, Copy)]
pub enum Anchor {
    Center,
    TopLeft,
    Bottom,
}

/// The top-left corner of a `size` label at `anchor` in a `client` area,
/// with `pad` the scaled 8 px spacing. Shared by the GDI and Direct2D paths.
pub fn place(anchor: Anchor, client: (i32, i32), size: (i32, i32), pad: i32) -> (i32, i32) {
    let ((cw, ch), (w, h)) = (client, size);
    match anchor {
        Anchor::Center => ((cw - w) / 2, (ch - h) / 2),
        Anchor::TopLeft => (pad * 2, pad * 2),
        Anchor::Bottom => ((cw - w) / 2, ch - h - pad * 4),
    }
}

/// The widest a label may be: the window less a margin, at most 720 px
/// scaled.
pub fn label_width(client_width: i32, scale: f64, pad: i32) -> i32 {
    (client_width - 4 * pad).clamp(1, (720.0 * scale) as i32)
}

/// What to draw this time.
pub enum Source<'a> {
    /// A picture from the hardware decoder.
    Nv12(&'a DisplayFrame),
    /// A picture from the CPU decoder, top-down BGRA.
    Bgra {
        width: u32,
        height: u32,
        data: &'a [u8],
    },
}

pub struct Presenter {
    gpu: Arc<Gpu>,
    hwnd: HWND,
    factory: IDXGIFactory2,
    swap: Swap,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext1,
    processor: Option<Processor>,
    /// The texture CPU frames are uploaded into.
    upload: Option<(ID3D11Texture2D, (u32, u32))>,
    size: (u32, u32),
    text: Text,
    /// Flip-model presents so far, and how many took over `SLOW_PRESENT`.
    presents: u32,
    slow_presents: u32,
}

/// A swap chain and how it presents.
struct Swap {
    chain: IDXGISwapChain1,
    flags: DXGI_SWAP_CHAIN_FLAG,
    /// The blit model (DWM copies the buffer) rather than the flip model.
    blit: bool,
    /// Present without waiting for the next refresh (and allow tearing in
    /// full screen): the stream paces itself.
    tearing: bool,
}

/// A flip-model present that takes this long is stalled, not waiting for a
/// refresh. On one machine every flip-model present took 260 ms, whatever
/// was drawn (an overlay hooking `Present` is the likely cause), while the
/// blit model took 1 ms.
const SLOW_PRESENT: std::time::Duration = std::time::Duration::from_millis(50);
/// Within the first `PROBE_PRESENTS` flip-model presents, this many slow
/// ones switch the window to the blit model.
const PROBE_PRESENTS: u32 = 30;
const SLOW_TO_SWITCH: u32 = 5;

/// Direct2D and DirectWrite on the shared device, for the labels.
struct Text {
    context: ID2D1DeviceContext,
    dwrite: IDWriteFactory,
    /// The label font at one size; replaced when the DPI changes.
    format: Option<(IDWriteTextFormat, f32)>,
    plate: ID2D1SolidColorBrush,
    ink: ID2D1SolidColorBrush,
}

/// A video processor for one input and output size.
struct Processor {
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input: (u32, u32),
    output: (u32, u32),
}

impl Presenter {
    /// A swap chain for `hwnd` (whose client area is `size`) on `gpu`.
    pub fn new(
        hwnd: windows_sys::Win32::Foundation::HWND,
        gpu: Arc<Gpu>,
        size: (u32, u32),
    ) -> Result<Self> {
        let hwnd = HWND(hwnd);
        let size = (size.0.max(1), size.1.max(1));
        // SAFETY: DXGI calls on the shared device and our window.
        unsafe {
            let factory: IDXGIFactory2 = gpu
                .device
                .cast::<IDXGIDevice>()?
                .GetAdapter()?
                .GetParent()?;
            let swap = Swap::create(&factory, &gpu, hwnd, size, false)?;
            // F11 is ours; DXGI's own Alt+Enter full screen would fight it.
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
            let video_device: ID3D11VideoDevice = gpu.device.cast()?;
            let video_context: ID3D11VideoContext1 = gpu.context.cast()?;
            let text = Text::new(&gpu)?;
            Ok(Self {
                gpu,
                hwnd,
                factory,
                swap,
                video_device,
                video_context,
                processor: None,
                upload: None,
                size,
                text,
                presents: 0,
                slow_presents: 0,
            })
        }
    }

    /// Follow the window's client area.
    pub fn resize(&mut self, size: (u32, u32)) -> Result<()> {
        let size = (size.0.max(1), size.1.max(1));
        if size == self.size {
            return Ok(());
        }
        // The output size is baked into the processor.
        self.processor = None;
        // SAFETY: no back buffer reference outlives `render`, as the resize
        // requires.
        unsafe {
            self.swap
                .chain
                .ResizeBuffers(0, size.0, size.1, DXGI_FORMAT_UNKNOWN, self.swap.flags)
        }
        .context("resize the swap chain")?;
        self.size = size;
        Ok(())
    }

    /// Draw `source` into `dest` (client pixels) on black, then `labels`
    /// (`scale` is DPI / 96), and present. With no source the window is
    /// black under the labels.
    pub fn render(
        &mut self,
        source: Option<(Source, RECT)>,
        labels: &[(&str, Anchor)],
        scale: f64,
    ) -> Result<()> {
        // SAFETY: the back buffer is used within this call and released
        // before the present, as ResizeBuffers requires.
        let elapsed = unsafe {
            let back: ID3D11Texture2D = self
                .swap
                .chain
                .GetBuffer(0)
                .context("get the back buffer")?;
            let picture = match source {
                Some((source, dest)) => {
                    self.blit(&back, source, dest)?;
                    true
                }
                None => false,
            };
            if !picture || !labels.is_empty() {
                self.text
                    .draw(&back, !picture, labels, scale, self.size)
                    .context("draw the labels")?;
            }
            drop(back);
            let flags = if self.swap.tearing {
                DXGI_PRESENT_ALLOW_TEARING
            } else {
                DXGI_PRESENT(0)
            };
            let started = std::time::Instant::now();
            self.swap.chain.Present(0, flags).ok().context("present")?;
            started.elapsed()
        };
        tracing::trace!(present_us = elapsed.as_micros() as u64, "presented");
        if !self.swap.blit && self.presents < PROBE_PRESENTS {
            self.presents += 1;
            if elapsed > SLOW_PRESENT {
                self.slow_presents += 1;
                if self.slow_presents == SLOW_TO_SWITCH {
                    self.use_blit_model(elapsed)?;
                }
            }
        }
        Ok(())
    }

    /// Replace the flip-model swap chain with a blit-model one.
    fn use_blit_model(&mut self, slow: std::time::Duration) -> Result<()> {
        tracing::warn!(
            present_ms = slow.as_millis() as u64,
            "flip-model presents stall on this machine (an overlay hooking Present, such as \
             RivaTuner, is the usual cause); presenting with the blit model instead"
        );
        // SAFETY: the old chain holds no buffer references outside
        // `render`; clearing and flushing the context releases the
        // runtime's own before a new chain is made for the same window.
        unsafe {
            let swap = &mut self.swap;
            let old = std::mem::replace(
                swap,
                Swap::create(&self.factory, &self.gpu, self.hwnd, self.size, true).or_else(
                    |_| {
                        // DXGI may refuse a second chain while the first still
                        // lives; release it and retry.
                        self.gpu.context.ClearState();
                        self.gpu.context.Flush();
                        Swap::create(&self.factory, &self.gpu, self.hwnd, self.size, true)
                    },
                )?,
            );
            drop(old);
            self.gpu.context.ClearState();
            self.gpu.context.Flush();
        }
        Ok(())
    }

    /// Convert and scale `source` into `dest` on `back`, black around it.
    ///
    /// # Safety
    /// `back` must be the swap chain's current back buffer.
    unsafe fn blit(&mut self, back: &ID3D11Texture2D, source: Source, dest: RECT) -> Result<()> {
        let (texture, size, color_space) = match source {
            Source::Nv12(frame) => (
                frame.texture.clone(),
                (frame.width, frame.height),
                // BT.709, limited range. gliff's 4:2:0 takes each 2x2
                // block's chroma from its top-left pixel; DXGI has no
                // top-left siting for BT.709, and left siting is half a
                // pixel off vertically only.
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            ),
            Source::Bgra {
                width,
                height,
                data,
            } => (
                // SAFETY: the caller's contract covers the upload too.
                unsafe { self.upload_bgra((width, height), data) }?,
                (width, height),
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            ),
        };
        let output = self.size;
        let p = self.processor(size, output)?;
        let (enumerator, processor) = (p.enumerator.clone(), p.processor.clone());
        let vc = &self.video_context;
        // SAFETY: views and settings on the processor just made for these
        // sizes; the stream's input view is released after the blit.
        unsafe {
            let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut input = None;
            self.video_device
                .CreateVideoProcessorInputView(&texture, &enumerator, &input_desc, Some(&mut input))
                .context("create the input view")?;
            let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut target = None;
            self.video_device
                .CreateVideoProcessorOutputView(back, &enumerator, &output_desc, Some(&mut target))
                .context("create the output view")?;
            let target = target.context("no output view")?;

            vc.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            // No driver "enhancements": the picture should be the remote's.
            vc.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            vc.VideoProcessorSetStreamColorSpace1(&processor, 0, color_space);
            vc.VideoProcessorSetOutputColorSpace1(
                &processor,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            let source_rect = rect((0, 0), size);
            vc.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&source_rect));
            vc.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dest));
            let target_rect = rect((0, 0), output);
            vc.VideoProcessorSetOutputTargetRect(&processor, true, Some(&target_rect));
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 {
                    RGBA: D3D11_VIDEO_COLOR_RGBA {
                        R: 0.0,
                        G: 0.0,
                        B: 0.0,
                        A: 1.0,
                    },
                },
            };
            vc.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(input),
                ..Default::default()
            };
            let result =
                vc.VideoProcessorBlt(&processor, &target, 0, std::slice::from_ref(&stream));
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result.context("draw the picture")
        }
    }

    /// The processor for these sizes, made on first use or when they change.
    fn processor(&mut self, input: (u32, u32), output: (u32, u32)) -> Result<&Processor> {
        let current = self
            .processor
            .as_ref()
            .is_some_and(|p| p.input == input && p.output == output);
        if !current {
            // A hint only; a zero rate is refused by some drivers.
            let rate = DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: input.0,
                InputHeight: input.1,
                OutputFrameRate: rate,
                OutputWidth: output.0,
                OutputHeight: output.1,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            // SAFETY: a valid description on the shared video device.
            let (enumerator, processor) = unsafe {
                let enumerator = self
                    .video_device
                    .CreateVideoProcessorEnumerator(&desc)
                    .context("create the video processor enumerator")?;
                let processor = self
                    .video_device
                    .CreateVideoProcessor(&enumerator, 0)
                    .context("create the video processor")?;
                (enumerator, processor)
            };
            self.processor = Some(Processor {
                enumerator,
                processor,
                input,
                output,
            });
        }
        Ok(self.processor.as_ref().expect("set above"))
    }

    /// Copy a CPU frame into the upload texture.
    ///
    /// # Safety
    /// `data` must hold `size.0 * size.1` BGRA pixels.
    unsafe fn upload_bgra(&mut self, size: (u32, u32), data: &[u8]) -> Result<ID3D11Texture2D> {
        if self.upload.as_ref().is_none_or(|(_, s)| *s != size) {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: size.0,
                Height: size.1,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                // The video processor takes input views only of textures it
                // could also render to.
                BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut texture = None;
            // SAFETY: a valid description and an out-pointer to a local.
            unsafe {
                self.gpu
                    .device
                    .CreateTexture2D(&desc, None, Some(&mut texture))
            }?;
            self.upload = Some((texture.context("no upload texture")?, size));
        }
        let texture = self.upload.as_ref().expect("set above").0.clone();
        // SAFETY: the caller guarantees `data` covers the texture.
        unsafe {
            self.gpu.context.UpdateSubresource(
                &texture,
                0,
                None,
                data.as_ptr().cast(),
                size.0 * 4,
                0,
            )
        };
        Ok(texture)
    }
}

fn rect((x, y): (i32, i32), (w, h): (u32, u32)) -> RECT {
    RECT {
        left: x,
        top: y,
        right: x + w as i32,
        bottom: y + h as i32,
    }
}

impl Swap {
    /// A swap chain for `hwnd`: flip model, or the blit model if `blit`.
    ///
    /// # Safety
    /// `hwnd` must be a live window with no other swap chain.
    unsafe fn create(
        factory: &IDXGIFactory2,
        gpu: &Gpu,
        hwnd: HWND,
        size: (u32, u32),
        blit: bool,
    ) -> Result<Self> {
        // SAFETY: plain DXGI calls with descriptions that live across them.
        unsafe {
            let tearing = !blit
                && factory.cast::<IDXGIFactory5>().is_ok_and(|f| {
                    let mut allowed = BOOL(0);
                    f.CheckFeatureSupport(
                        DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                        (&mut allowed as *mut BOOL).cast(),
                        std::mem::size_of::<BOOL>() as u32,
                    )
                    .is_ok()
                        && allowed.as_bool()
                });
            let flags = if tearing {
                DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING
            } else {
                DXGI_SWAP_CHAIN_FLAG(0)
            };
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: size.0,
                Height: size.1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: if blit { 1 } else { 2 },
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: if blit {
                    DXGI_SWAP_EFFECT_DISCARD
                } else {
                    DXGI_SWAP_EFFECT_FLIP_DISCARD
                },
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: flags.0 as u32,
            };
            let chain = factory
                .CreateSwapChainForHwnd(&gpu.device, hwnd, &desc, None, None)
                .context("create the swap chain")?;
            Ok(Self {
                chain,
                flags,
                blit,
                tearing,
            })
        }
    }
}

impl Text {
    fn new(gpu: &Gpu) -> Result<Self> {
        // SAFETY: factory and device creation on the shared device; the
        // factory is multithreaded because the device is shared.
        unsafe {
            let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_MULTI_THREADED, None)
                .context("create the Direct2D factory")?;
            let device = factory.CreateDevice(&gpu.device.cast::<IDXGIDevice>()?)?;
            let context = device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            // One unit is one pixel; the font size carries the DPI.
            context.SetDpi(96.0, 96.0);
            let plate = context.CreateSolidColorBrush(&rgb(0x20, 0x20, 0x20), None)?;
            let ink = context.CreateSolidColorBrush(&rgb(0xE8, 0xE8, 0xE8), None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            Ok(Self {
                context,
                dwrite,
                format: None,
                plate,
                ink,
            })
        }
    }

    /// The labels on `back`, after clearing it to black if `clear`.
    ///
    /// # Safety
    /// `back` must be the current back buffer, not otherwise in use.
    unsafe fn draw(
        &mut self,
        back: &ID3D11Texture2D,
        clear: bool,
        labels: &[(&str, Anchor)],
        scale: f64,
        size: (u32, u32),
    ) -> Result<()> {
        let format = self.format(scale)?;
        let client = (size.0 as i32, size.1 as i32);
        let pad = (8.0 * scale) as i32;
        let max_width = label_width(client.0, scale, pad) as f32;
        // SAFETY: the target bitmap wraps the back buffer for this draw only
        // and is unset before returning.
        unsafe {
            let surface: IDXGISurface = back.cast()?;
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                colorContext: ManuallyDrop::new(None),
            };
            let target = self
                .context
                .CreateBitmapFromDxgiSurface(&surface, Some(&props))?;
            self.context.SetTarget(&target);
            self.context.BeginDraw();
            if clear {
                self.context.Clear(Some(&rgb(0, 0, 0)));
            }
            for (text, anchor) in labels {
                let text: Vec<u16> = text.encode_utf16().collect();
                let layout = self
                    .dwrite
                    .CreateTextLayout(&text, &format, max_width, f32::MAX)?;
                let mut m = DWRITE_TEXT_METRICS::default();
                layout.GetMetrics(&mut m)?;
                let (w, h) = (m.width.ceil() as i32, m.height.ceil() as i32);
                let (x, y) = place(*anchor, client, (w, h), pad);
                let plate = D2D_RECT_F {
                    left: (x - pad) as f32,
                    top: (y - pad / 2) as f32,
                    right: (x + w + pad) as f32,
                    bottom: (y + h + pad / 2) as f32,
                };
                self.context.FillRectangle(&plate, &self.plate);
                // The layout centres each line in `max_width`; `m.left` is
                // where the widest line starts.
                let origin = windows_numerics::Vector2 {
                    X: x as f32 - m.left,
                    Y: y as f32,
                };
                self.context.DrawTextLayout(
                    origin,
                    &layout,
                    &self.ink,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                );
            }
            let result = self.context.EndDraw(None, None);
            self.context.SetTarget(None);
            result?;
        }
        Ok(())
    }

    /// Segoe UI at 15 px scaled, centred and wrapping, like the GDI path.
    fn format(&mut self, scale: f64) -> Result<IDWriteTextFormat> {
        let size = (15.0 * scale) as f32;
        if let Some((format, s)) = &self.format {
            if *s == size {
                return Ok(format.clone());
            }
        }
        // SAFETY: plain DirectWrite calls with static strings.
        let format = unsafe {
            let format = self.dwrite.CreateTextFormat(
                w!("Segoe UI"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!(""),
            )?;
            format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            format.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP)?;
            format
        };
        self.format = Some((format.clone(), size));
        Ok(format)
    }
}

fn rgb(r: u8, g: u8, b: u8) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: f32::from(r) / 255.0,
        g: f32::from(g) / 255.0,
        b: f32::from(b) / 255.0,
        a: 1.0,
    }
}
