//! Solid text. The app is captured, its background color is keyed out on the GPU, and the
//! result is drawn on the glass. The real window stays on top at almost zero opacity, so it
//! still gets every click and key press while the copy underneath is what you see.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use windows::core::{s, Interface, PCSTR};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::UI::Composition::{Compositor, ICompositionSurface};
use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_DRIVER_TYPE_HARDWARE,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader,
    ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D,
    ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX,
    D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_FILTER_MIN_MAG_MIP_POINT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SAMPLER_DESC, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::WinRT::Composition::ICompositorInterop;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// Posted to the effect thread's host window when a new frame of the app in WPARAM is ready.
pub const WM_FRAME: u32 = WM_APP + 1;

const FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;
/// Apps change theme or scroll to a new page, so the background color is re-read this often.
const KEY_EVERY: Duration = Duration::from_secs(1);
/// Rows sampled across the window to find the background color.
const KEY_ROWS: u32 = 9;
/// The background must cover at least this share of the sampled pixels to be trusted.
const KEY_MIN_SHARE: f32 = 0.2;
/// How far from the background a pixel must be, as a fraction of the way to black or white,
/// before it is drawn fully solid. Anti-aliased text edges fall between and blend smoothly.
const SOLID_AT: f32 = 0.25;

const SHADER: &str = r#"
Texture2D frame : register(t0);
SamplerState nearest : register(s0);
cbuffer Params : register(b0) {
    float4 key;    // background color
    float4 extra;  // x: background opacity, y: solid threshold, zw: content size / texture size
};

struct Vertex { float4 pos : SV_Position; float2 uv : TEXCOORD0; };

Vertex vs_main(uint id : SV_VertexID) {
    float2 corner = float2((id << 1) & 2, id & 2);
    Vertex v;
    v.uv = corner * extra.zw;
    v.pos = float4(corner * float2(2, -2) + float2(-1, 1), 0, 1);
    return v;
}

float4 ps_main(Vertex v) : SV_Target {
    float4 p = frame.Sample(nearest, v.uv);
    float3 d = p.rgb - key.rgb;
    // How much of the way from the background toward black or white each channel moved.
    float3 room = max(d > 0 ? 1 - key.rgb : key.rgb, 1.0 / 255);
    float3 need = abs(d) / room;
    float a = saturate(max(need.r, max(need.g, need.b)) / extra.y);
    a = max(a, extra.x);
    // Premultiplied color that looks exactly like the original when laid over the background.
    float3 color = max(p.rgb - key.rgb * (1 - a), 0);
    return float4(color, a) * p.a;
}
"#;

#[repr(C)]
struct Params {
    key: [f32; 4],
    extra: [f32; 4],
}

fn err(context: &str) -> impl Fn(windows::core::Error) -> String + '_ {
    move |error| format!("{context}: {}", error.message())
}

/// The GPU device and shaders, shared by every solid-text window.
pub struct Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    capture_device: IDirect3DDevice,
    factory: IDXGIFactory2,
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    params: ID3D11Buffer,
}

impl Renderer {
    pub fn new() -> Result<Self, String> {
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            return Err("This version of Windows cannot capture app windows.".into());
        }
        unsafe {
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(err("Could not open the graphics card"))?;
            let device: ID3D11Device = device.ok_or("No graphics device.")?;
            let context = context.ok_or("No graphics context.")?;
            let dxgi: IDXGIDevice = device.cast().map_err(err("Graphics device"))?;
            let capture_device: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)
                .and_then(|device| device.cast())
                .map_err(err("Capture device"))?;
            let factory: IDXGIFactory2 = dxgi
                .GetAdapter()
                .and_then(|adapter| adapter.GetParent())
                .map_err(err("Graphics adapter"))?;

            let mut vertex = None;
            device
                .CreateVertexShader(&compile(s!("vs_main"), s!("vs_5_0"))?, None, Some(&mut vertex))
                .map_err(err("Vertex shader"))?;
            let mut pixel = None;
            device
                .CreatePixelShader(&compile(s!("ps_main"), s!("ps_5_0"))?, None, Some(&mut pixel))
                .map_err(err("Pixel shader"))?;
            let mut sampler = None;
            device
                .CreateSamplerState(
                    &D3D11_SAMPLER_DESC {
                        Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
                        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                        ComparisonFunc: D3D11_COMPARISON_NEVER,
                        MaxLOD: f32::MAX,
                        ..Default::default()
                    },
                    Some(&mut sampler),
                )
                .map_err(err("Sampler"))?;
            let mut params = None;
            device
                .CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: std::mem::size_of::<Params>() as u32,
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut params),
                )
                .map_err(err("Shader parameters"))?;
            Ok(Self {
                device,
                context,
                capture_device,
                factory,
                vertex: vertex.ok_or("No vertex shader.")?,
                pixel: pixel.ok_or("No pixel shader.")?,
                sampler: sampler.ok_or("No sampler.")?,
                params: params.ok_or("No shader parameters.")?,
            })
        }
    }
}

fn compile(entry: PCSTR, target: PCSTR) -> Result<Vec<u8>, String> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let result = unsafe {
        D3DCompile(
            SHADER.as_ptr().cast(),
            SHADER.len(),
            PCSTR::null(),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(error) = result {
        let detail = errors.map(|blob| blob_bytes(&blob)).unwrap_or_default();
        return Err(format!(
            "The shader did not compile ({}): {}",
            error.message(),
            String::from_utf8_lossy(&detail)
        ));
    }
    code.map(|blob| blob_bytes(&blob)).ok_or_else(|| "The shader compiler returned nothing.".into())
}

fn blob_bytes(blob: &ID3DBlob) -> Vec<u8> {
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) }
        .to_vec()
}

/// Live capture of one app window, drawn into a swap chain the glass shows.
pub struct SolidView {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    arrived: i64,
    swapchain: IDXGISwapChain1,
    target_view: Option<ID3D11RenderTargetView>,
    /// Size of the pool and swap chain, which follows the window.
    size: SizeInt32,
    /// Copy of the newest frame, so a slider change can redraw an app that is not repainting.
    last: Option<(ID3D11Texture2D, ID3D11ShaderResourceView, SizeInt32)>,
    staging: Option<(ID3D11Texture2D, u32)>,
    key: Option<[u8; 3]>,
    keyed_at: Option<Instant>,
}

impl SolidView {
    /// Start capturing `target`. Frame notifications are posted to `host` as [`WM_FRAME`].
    pub fn start(
        renderer: &Renderer,
        compositor: &Compositor,
        target: HWND,
        host: HWND,
    ) -> Result<(Self, ICompositionSurface), String> {
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(err("Window capture"))?;
        let item: GraphicsCaptureItem =
            unsafe { interop.CreateForWindow(target) }.map_err(err("Could not capture the app"))?;
        let size = item.Size().map_err(err("Window size"))?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&renderer.capture_device, FORMAT, 2, size)
            .map_err(err("Capture buffers"))?;
        let session = pool.CreateCaptureSession(&item).map_err(err("Capture session"))?;
        let _ = session.SetIsCursorCaptureEnabled(false);
        let _ = session.SetIsBorderRequired(false);
        let (target_id, host_id) = (target.0 as isize, host.0 as isize);
        let arrived = pool
            .FrameArrived(&TypedEventHandler::new(move |_, _| {
                unsafe {
                    let _ = PostMessageW(
                        Some(HWND(host_id as *mut _)),
                        WM_FRAME,
                        WPARAM(target_id as usize),
                        LPARAM(0),
                    );
                }
                Ok(())
            }))
            .map_err(err("Capture events"))?;

        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: size.Width.max(1) as u32,
            Height: size.Height.max(1) as u32,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            ..Default::default()
        };
        let swapchain = unsafe { renderer.factory.CreateSwapChainForComposition(&renderer.device, &desc, None) }
            .map_err(err("Drawing surface"))?;
        let surface = compositor
            .cast::<ICompositorInterop>()
            .and_then(|interop| unsafe { interop.CreateCompositionSurfaceForSwapChain(&swapchain) })
            .map_err(err("Glass surface"))?;
        session.StartCapture().map_err(err("Could not start capturing the app"))?;
        Ok((
            Self {
                pool,
                session,
                arrived,
                swapchain,
                target_view: None,
                size,
                last: None,
                staging: None,
                key: None,
                keyed_at: None,
            },
            surface,
        ))
    }

    /// Draw the newest frame. With `force`, redraw the last one even if nothing new arrived.
    /// Returns true when something was drawn.
    pub fn draw(&mut self, renderer: &Renderer, background_alpha: f32, force: bool) -> Result<bool, String> {
        let fresh = self.take_frame(renderer)?;
        if !fresh && !force {
            return Ok(false);
        }
        let Some((_, view, content)) = &self.last else {
            return Ok(false);
        };
        let key = self.key.unwrap_or([0, 0, 0]);
        let context = &renderer.context;
        unsafe {
            if self.target_view.is_none() {
                let back: ID3D11Texture2D = self.swapchain.GetBuffer(0).map_err(err("Back buffer"))?;
                let mut target_view = None;
                renderer
                    .device
                    .CreateRenderTargetView(&back, None, Some(&mut target_view))
                    .map_err(err("Render target"))?;
                self.target_view = target_view;
            }
            let params = Params {
                key: [key[0] as f32 / 255.0, key[1] as f32 / 255.0, key[2] as f32 / 255.0, 1.0],
                extra: [
                    background_alpha,
                    SOLID_AT,
                    content.Width as f32 / self.size.Width.max(1) as f32,
                    content.Height as f32 / self.size.Height.max(1) as f32,
                ],
            };
            context.UpdateSubresource(&renderer.params, 0, None, (&raw const params).cast(), 0, 0);
            context.OMSetRenderTargets(Some(std::slice::from_ref(&self.target_view)), None);
            context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: self.size.Width as f32,
                Height: self.size.Height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&renderer.vertex, None);
            context.PSSetShader(&renderer.pixel, None);
            let params = Some(renderer.params.clone());
            context.VSSetConstantBuffers(0, Some(std::slice::from_ref(&params)));
            context.PSSetConstantBuffers(0, Some(std::slice::from_ref(&params)));
            context.PSSetShaderResources(0, Some(&[Some(view.clone())]));
            context.PSSetSamplers(0, Some(&[Some(renderer.sampler.clone())]));
            context.Draw(3, 0);
            context.PSSetShaderResources(0, Some(&[None]));
            context.OMSetRenderTargets(None, None);
            self.swapchain.Present(0, DXGI_PRESENT(0)).ok().map_err(err("Present"))?;
        }
        Ok(true)
    }

    /// Copy the newest captured frame into `last`, following size changes. True if there was one.
    fn take_frame(&mut self, renderer: &Renderer) -> Result<bool, String> {
        let mut newest = None;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            if let Some(older) = newest.replace(frame) {
                let _ = older.Close();
            }
        }
        let Some(frame) = newest else {
            return Ok(false);
        };
        let content = frame.ContentSize().map_err(err("Frame size"))?;
        let texture: ID3D11Texture2D = frame
            .Surface()
            .and_then(|surface| surface.cast::<IDirect3DDxgiInterfaceAccess>())
            .and_then(|access| unsafe { access.GetInterface() })
            .map_err(err("Frame"))?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        if content.Width <= 0 || content.Height <= 0 {
            let _ = frame.Close();
            return Ok(false);
        }
        let content = SizeInt32 {
            Width: content.Width.min(desc.Width as i32),
            Height: content.Height.min(desc.Height as i32),
        };

        let reusable = self.last.as_ref().is_some_and(|(copy, _, _)| {
            let mut have = D3D11_TEXTURE2D_DESC::default();
            unsafe { copy.GetDesc(&mut have) };
            have.Width == desc.Width && have.Height == desc.Height
        });
        if !reusable {
            let copy_desc = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
                ..desc
            };
            let mut copy = None;
            unsafe { renderer.device.CreateTexture2D(&copy_desc, None, Some(&mut copy)) }
                .map_err(err("Frame copy"))?;
            let copy = copy.ok_or("No frame copy.")?;
            let mut view = None;
            unsafe { renderer.device.CreateShaderResourceView(&copy, None, Some(&mut view)) }
                .map_err(err("Frame view"))?;
            self.last = Some((copy, view.ok_or("No frame view.")?, content));
        }
        if let Some((copy, _, size)) = &mut self.last {
            unsafe { renderer.context.CopyResource(&*copy, &texture) };
            *size = content;
        }
        if self.keyed_at.is_none_or(|at| at.elapsed() >= KEY_EVERY) {
            self.read_key(renderer, &texture, content);
        }
        let _ = frame.Close();

        if content.Width != self.size.Width || content.Height != self.size.Height {
            self.size = content;
            self.target_view = None;
            unsafe {
                renderer.context.OMSetRenderTargets(None, None);
                renderer.context.Flush();
                self.swapchain
                    .ResizeBuffers(
                        2,
                        content.Width as u32,
                        content.Height as u32,
                        DXGI_FORMAT_B8G8R8A8_UNORM,
                        DXGI_SWAP_CHAIN_FLAG(0),
                    )
                    .map_err(err("Resize"))?;
            }
            self.pool
                .Recreate(&renderer.capture_device, FORMAT, 2, content)
                .map_err(err("Capture buffers"))?;
        }
        Ok(true)
    }

    /// Sample a few rows of the frame and take the most common color as the background.
    fn read_key(&mut self, renderer: &Renderer, texture: &ID3D11Texture2D, content: SizeInt32) {
        self.keyed_at = Some(Instant::now());
        let (width, height) = (content.Width as u32, content.Height as u32);
        if width == 0 || height <= KEY_ROWS {
            return;
        }
        if self.staging.as_ref().is_none_or(|(_, have)| *have != width) {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: KEY_ROWS,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..Default::default()
            };
            let mut staging = None;
            if unsafe { renderer.device.CreateTexture2D(&desc, None, Some(&mut staging)) }.is_err() {
                return;
            }
            self.staging = staging.map(|texture| (texture, width));
        }
        let Some((staging, _)) = &self.staging else {
            return;
        };
        let mut pixels = Vec::with_capacity((width * KEY_ROWS * 4) as usize);
        unsafe {
            for row in 0..KEY_ROWS {
                let y = height * (row + 1) / (KEY_ROWS + 1);
                let source = D3D11_BOX { left: 0, top: y, front: 0, right: width, bottom: y + 1, back: 1 };
                renderer
                    .context
                    .CopySubresourceRegion(staging, 0, 0, row, 0, texture, 0, Some(&source));
            }
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if renderer.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).is_err() {
                return;
            }
            for row in 0..KEY_ROWS as usize {
                let line = std::slice::from_raw_parts(
                    (mapped.pData as *const u8).add(row * mapped.RowPitch as usize),
                    width as usize * 4,
                );
                pixels.extend_from_slice(line);
            }
            renderer.context.Unmap(staging, 0);
        }
        if let Some(key) = background_color(&pixels) {
            self.key = Some(key);
        }
    }
}

impl Drop for SolidView {
    fn drop(&mut self) {
        let _ = self.pool.RemoveFrameArrived(self.arrived);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// The most common opaque color in BGRA pixels, as RGB, if it covers enough of them.
pub fn background_color(bgra: &[u8]) -> Option<[u8; 3]> {
    let mut counts: HashMap<[u8; 3], usize> = HashMap::new();
    let mut opaque = 0usize;
    for pixel in bgra.as_chunks::<4>().0 {
        if pixel[3] == 255 {
            opaque += 1;
            *counts.entry([pixel[2], pixel[1], pixel[0]]).or_default() += 1;
        }
    }
    let (color, count) = counts.into_iter().max_by_key(|&(_, count)| count)?;
    (count as f32 >= opaque as f32 * KEY_MIN_SHARE).then_some(color)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgra(rgb: [u8; 3]) -> [u8; 4] {
        [rgb[2], rgb[1], rgb[0], 255]
    }

    #[test]
    fn background_is_the_most_common_opaque_color() {
        let mut pixels = Vec::new();
        for _ in 0..60 {
            pixels.extend_from_slice(&bgra([20, 20, 20]));
        }
        for _ in 0..30 {
            pixels.extend_from_slice(&bgra([230, 230, 230]));
        }
        for _ in 0..100 {
            pixels.extend_from_slice(&[0, 0, 0, 0]);
        }
        assert_eq!(background_color(&pixels), Some([20, 20, 20]));
    }

    #[test]
    fn busy_content_has_no_background() {
        let pixels: Vec<u8> = (0..200u32)
            .flat_map(|i| bgra([(i % 256) as u8, (i * 7 % 256) as u8, 40]))
            .collect();
        assert_eq!(background_color(&pixels), None);
    }
}
