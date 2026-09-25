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
    ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_FLAG, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8_UNORM,
    DXGI_SAMPLE_DESC,
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
/// App backgrounds are one flat color, so a pixel only counts as background when it matches
/// the key exactly. Any looser and smooth gradients in images pick up bands of glass.
const MATCH_TOLERANCE: f32 = 0.5 / 255.0;
/// Matching pixels out of the 5x5 around one before it counts as background. Photos and video
/// only hit the key color in stray pixels, so they stay solid.
const PATCH_MIN: f32 = 8.0;
/// Pixels are grouped into square blocks this wide to find which patches of background color
/// are connected to each other. Keep in sync with `BLOCK` in the shader.
const BLOCK: u32 = 4;
/// Matching pixels out of a block's 16 before it can join the background. Half a block lets
/// the background reach across text strokes but not across the thicker content of images.
const BLOCK_MIN: u8 = 8;
/// Enclosed patches smaller than this many blocks, like the inside of letters, still count as
/// background. Bigger enclosed patches are part of an image and stay solid.
const SMALL_PATCH: usize = 16;

const SHADER: &str = r#"
static const int BLOCK = 4;
Texture2D frame : register(t0);
Texture2D<float> background : register(t1);  // 1 for blocks of the app's background
cbuffer Params : register(b0) {
    float4 key;    // rgb: background color, a: 1 when the app has one
    float4 extra;  // x: background opacity, y: solid threshold, z: match tolerance, w: patch minimum
    float4 area;   // xy: content size in pixels
};

float4 vs_main(uint id : SV_VertexID) : SV_Position {
    float2 corner = float2((id << 1) & 2, id & 2);
    return float4(corner * float2(2, -2) + float2(-1, 1), 0, 1);
}

float4 pixel_at(int2 at) {
    if (any(at < 0) || any(at >= int2(area.xy))) return float4(0, 0, 0, 0);
    return frame.Load(int3(at, 0));
}

uint matches(int2 at) {
    float4 q = pixel_at(at);
    return q.a > 0.99 && all(abs(q.rgb - key.rgb) <= extra.z) ? 1 : 0;
}

// Number of pixels matching the background color in each block, read back by the CPU.
float ps_count(float4 pos : SV_Position) : SV_Target {
    int2 origin = int2(pos.xy) * BLOCK;
    uint count = 0;
    [unroll] for (int y = 0; y < BLOCK; y++) {
        [unroll] for (int x = 0; x < BLOCK; x++) {
            count += matches(origin + int2(x, y));
        }
    }
    return count / 255.0;
}

float4 ps_main(float4 pos : SV_Position) : SV_Target {
    int2 at = int2(pos.xy);
    float4 p = pixel_at(at);
    if (key.a == 0) return p;

    uint match[7][7];
    [unroll] for (int y = 0; y < 7; y++) {
        [unroll] for (int x = 0; x < 7; x++) {
            int2 q = at + int2(x - 3, y - 3);
            match[y][x] = matches(q) * (background.Load(int3(q / BLOCK, 0)) > 0.5 ? 1 : 0);
        }
    }
    // Only this pixel and its neighbors can turn to glass, and only when they sit in a patch of
    // background, so the edge of text blends while everything else stays exactly as it was.
    bool near_background = false;
    [unroll] for (int cy = 2; cy <= 4; cy++) {
        [unroll] for (int cx = 2; cx <= 4; cx++) {
            uint count = 0;
            [unroll] for (int dy = -2; dy <= 2; dy++) {
                [unroll] for (int dx = -2; dx <= 2; dx++) {
                    count += match[cy + dy][cx + dx];
                }
            }
            near_background = near_background || (match[cy][cx] == 1 && count >= extra.w);
        }
    }
    if (!near_background) return p;

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
    area: [f32; 4],
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
    count: ID3D11PixelShader,
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
            let mut count = None;
            device
                .CreatePixelShader(&compile(s!("ps_count"), s!("ps_5_0"))?, None, Some(&mut count))
                .map_err(err("Pixel shader"))?;
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
                count: count.ok_or("No pixel shader.")?,
                params: params.ok_or("No shader parameters.")?,
            })
        }
    }

    /// Draw `source` into `target` with the background color keyed out, turning only the
    /// blocks marked in `background` to glass. Without a background color the app is drawn
    /// unchanged.
    fn render(
        &self,
        source: &ID3D11ShaderResourceView,
        background: Option<&ID3D11ShaderResourceView>,
        target: &ID3D11RenderTargetView,
        size: SizeInt32,
        params: &Params,
    ) {
        let sources = [Some(source.clone()), background.cloned()];
        self.pass(&self.pixel, &sources, target, (size.Width as u32, size.Height as u32), params);
    }

    fn pass(
        &self,
        shader: &ID3D11PixelShader,
        sources: &[Option<ID3D11ShaderResourceView>],
        target: &ID3D11RenderTargetView,
        (width, height): (u32, u32),
        params: &Params,
    ) {
        let context = &self.context;
        unsafe {
            context.UpdateSubresource(&self.params, 0, None, std::ptr::from_ref(params).cast(), 0, 0);
            context.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: width as f32,
                Height: height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vertex, None);
            context.PSSetShader(shader, None);
            context.PSSetConstantBuffers(0, Some(&[Some(self.params.clone())]));
            context.PSSetShaderResources(0, Some(sources));
            context.Draw(3, 0);
            context.PSSetShaderResources(0, Some(&vec![None; sources.len()]));
            context.OMSetRenderTargets(None, None);
        }
    }
}

fn params(content: SizeInt32, key: Option<[u8; 3]>, background_alpha: f32) -> Params {
    let [r, g, b] = key.unwrap_or_default().map(|channel| channel as f32 / 255.0);
    Params {
        key: [r, g, b, if key.is_some() { 1.0 } else { 0.0 }],
        extra: [background_alpha, SOLID_AT, MATCH_TOLERANCE, PATCH_MIN],
        area: [content.Width as f32, content.Height as f32, 0.0, 0.0],
    }
}

fn texture(
    renderer: &Renderer,
    (width, height): (u32, u32),
    format: DXGI_FORMAT,
    usage: D3D11_USAGE,
    bind: D3D11_BIND_FLAG,
) -> Result<ID3D11Texture2D, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: usage,
        BindFlags: bind.0 as u32,
        CPUAccessFlags: if usage == D3D11_USAGE_STAGING { D3D11_CPU_ACCESS_READ.0 as u32 } else { 0 },
        ..Default::default()
    };
    let mut texture = None;
    unsafe { renderer.device.CreateTexture2D(&desc, None, Some(&mut texture)) }.map_err(err("Texture"))?;
    texture.ok_or_else(|| "No texture.".into())
}

/// Which parts of a frame are the app's own background, as opposed to patches of the same
/// color inside an image or video. The GPU counts matching pixels per block, and the blocks
/// are grouped into connected patches on the CPU.
struct Regions {
    content: SizeInt32,
    blocks: (u32, u32),
    counts: ID3D11Texture2D,
    counts_target: ID3D11RenderTargetView,
    staging: ID3D11Texture2D,
    mask: ID3D11Texture2D,
    mask_view: ID3D11ShaderResourceView,
}

impl Regions {
    fn new(renderer: &Renderer, content: SizeInt32) -> Result<Self, String> {
        let blocks = (
            (content.Width as u32).div_ceil(BLOCK).max(1),
            (content.Height as u32).div_ceil(BLOCK).max(1),
        );
        let counts = texture(renderer, blocks, DXGI_FORMAT_R8_UNORM, D3D11_USAGE_DEFAULT, D3D11_BIND_RENDER_TARGET)?;
        let staging = texture(renderer, blocks, DXGI_FORMAT_R8_UNORM, D3D11_USAGE_STAGING, D3D11_BIND_FLAG(0))?;
        let mask = texture(renderer, blocks, DXGI_FORMAT_R8_UNORM, D3D11_USAGE_DEFAULT, D3D11_BIND_SHADER_RESOURCE)?;
        let (mut counts_target, mut mask_view) = (None, None);
        unsafe {
            renderer
                .device
                .CreateRenderTargetView(&counts, None, Some(&mut counts_target))
                .map_err(err("Render target"))?;
            renderer.device.CreateShaderResourceView(&mask, None, Some(&mut mask_view)).map_err(err("Mask view"))?;
        }
        Ok(Self {
            content,
            blocks,
            counts,
            counts_target: counts_target.ok_or("No render target.")?,
            staging,
            mask,
            mask_view: mask_view.ok_or("No mask view.")?,
        })
    }

    fn update(&self, renderer: &Renderer, source: &ID3D11ShaderResourceView, key: [u8; 3]) -> Result<(), String> {
        let (width, height) = (self.blocks.0 as usize, self.blocks.1 as usize);
        let params = params(self.content, Some(key), 0.0);
        renderer.pass(&renderer.count, &[Some(source.clone())], &self.counts_target, self.blocks, &params);
        let mut counts = Vec::with_capacity(width * height);
        unsafe {
            renderer.context.CopyResource(&self.staging, &self.counts);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            renderer
                .context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(err("Reading the frame"))?;
            for row in 0..height {
                let line = (mapped.pData as *const u8).add(row * mapped.RowPitch as usize);
                counts.extend_from_slice(std::slice::from_raw_parts(line, width));
            }
            renderer.context.Unmap(&self.staging, 0);
            let mask = background_blocks(&counts, width, height);
            renderer.context.UpdateSubresource(&self.mask, 0, None, mask.as_ptr().cast(), width as u32, 0);
        }
        Ok(())
    }
}

/// Which blocks belong to the app's background, given how many pixels in each block match the
/// background color. Blocks with enough matches are grouped into connected patches. The
/// biggest patch is the background, along with any touching the edge of the window (split
/// panes) and any too small to be more than the inside of a letter. Patches enclosed by other
/// content, like a flat area inside an image, are not. Kept patches grow by one block so the
/// text and image edges along them can blend. Returns 255 for background blocks, 0 otherwise.
pub fn background_blocks(counts: &[u8], width: usize, height: usize) -> Vec<u8> {
    const NONE: u32 = u32::MAX;
    let open: Vec<bool> = counts.iter().map(|&count| count >= BLOCK_MIN).collect();
    let mut label = vec![NONE; counts.len()];
    let mut sizes: Vec<usize> = Vec::new();
    let mut edges: Vec<bool> = Vec::new();
    let mut stack = Vec::new();
    for start in 0..counts.len() {
        if !open[start] || label[start] != NONE {
            continue;
        }
        let id = sizes.len() as u32;
        let (mut size, mut edge) = (0, false);
        label[start] = id;
        stack.push(start);
        while let Some(i) = stack.pop() {
            size += 1;
            let (x, y) = (i % width, i / width);
            edge |= x == 0 || y == 0 || x + 1 == width || y + 1 == height;
            let neighbors = [
                (x > 0).then(|| i - 1),
                (x + 1 < width).then(|| i + 1),
                (y > 0).then(|| i - width),
                (y + 1 < height).then(|| i + width),
            ];
            for j in neighbors.into_iter().flatten() {
                if open[j] && label[j] == NONE {
                    label[j] = id;
                    stack.push(j);
                }
            }
        }
        sizes.push(size);
        edges.push(edge);
    }
    let biggest = sizes.iter().copied().max().unwrap_or(0);
    let keep: Vec<bool> = sizes
        .iter()
        .zip(&edges)
        .map(|(&size, &edge)| size == biggest || edge || size < SMALL_PATCH)
        .collect();
    let kept = |x: usize, y: usize| label[y * width + x] != NONE && keep[label[y * width + x] as usize];

    let mut mask = vec![0u8; counts.len()];
    for y in 0..height {
        for x in 0..width {
            let near = (y.saturating_sub(1)..=(y + 1).min(height - 1))
                .any(|ny| (x.saturating_sub(1)..=(x + 1).min(width - 1)).any(|nx| kept(nx, ny)));
            if near {
                mask[y * width + x] = 255;
            }
        }
    }
    mask
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
    regions: Option<Regions>,
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
                regions: None,
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
        let (view, content) = (view.clone(), *content);
        if self.target_view.is_none() {
            let back: ID3D11Texture2D = unsafe { self.swapchain.GetBuffer(0) }.map_err(err("Back buffer"))?;
            let mut target_view = None;
            unsafe { renderer.device.CreateRenderTargetView(&back, None, Some(&mut target_view)) }
                .map_err(err("Render target"))?;
            self.target_view = target_view;
        }
        if let Some(key) = self.key {
            let stale = self.regions.as_ref().is_none_or(|regions| regions.content != content);
            if stale {
                self.regions = Some(Regions::new(renderer, content)?);
            }
            if let Some(regions) = self.regions.as_ref().filter(|_| fresh || stale) {
                regions.update(renderer, &view, key)?;
            }
        }
        let target = self.target_view.as_ref().ok_or("No render target.")?;
        let background = self.regions.as_ref().map(|regions| &regions.mask_view);
        let params = params(content, self.key, background_alpha);
        renderer.render(&view, background, target, self.size, &params);
        unsafe { self.swapchain.Present(0, DXGI_PRESENT(0)) }.ok().map_err(err("Present"))?;
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

    const W: u32 = 96;
    const H: u32 = 72;
    const KEY: [u8; 3] = [31, 31, 31];
    const ALPHA: f32 = 0.7;

    /// Dark theme background with a text stroke, a noisy dark photo that often hits the
    /// background color exactly, and a bright image with a flat area of exactly the background
    /// color inside it.
    fn scene() -> Vec<u8> {
        let mut pixels = Vec::new();
        for y in 0..H {
            for x in 0..W {
                let rgb = if (4..12).contains(&y) && x == 4 {
                    [220, 220, 220]
                } else if (4..12).contains(&y) && x == 5 {
                    [80, 80, 80]
                } else if (20..64).contains(&y) && (4..36).contains(&x) {
                    let noise = (x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663)) % 11;
                    let v = 26 + noise as u8;
                    [v, v, v]
                } else if (24..60).contains(&y) && (52..84).contains(&x) {
                    KEY
                } else if (16..68).contains(&y) && (44..92).contains(&x) {
                    [200, (x * 2) as u8, (y * 3) as u8]
                } else {
                    KEY
                };
                pixels.extend_from_slice(&bgra(rgb));
            }
        }
        pixels
    }

    /// Run the shader on BGRA pixels and read back the premultiplied BGRA result.
    fn render_on_gpu(renderer: &Renderer, pixels: &[u8], key: Option<[u8; 3]>) -> Vec<u8> {
        use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_RENDER_TARGET, D3D11_SUBRESOURCE_DATA};
        let size = SizeInt32 { Width: W as i32, Height: H as i32 };
        let desc = D3D11_TEXTURE2D_DESC {
            Width: W,
            Height: H,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let data = D3D11_SUBRESOURCE_DATA { pSysMem: pixels.as_ptr().cast(), SysMemPitch: W * 4, SysMemSlicePitch: 0 };
        unsafe {
            let device = &renderer.device;
            let (mut source, mut view, mut target, mut target_view, mut staging) = (None, None, None, None, None);
            device.CreateTexture2D(&desc, Some(&data), Some(&mut source)).unwrap();
            device.CreateShaderResourceView(source.as_ref().unwrap(), None, Some(&mut view)).unwrap();
            let target_desc = D3D11_TEXTURE2D_DESC { BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32, ..desc };
            device.CreateTexture2D(&target_desc, None, Some(&mut target)).unwrap();
            let target = target.unwrap();
            device.CreateRenderTargetView(&target, None, Some(&mut target_view)).unwrap();
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..desc
            };
            device.CreateTexture2D(&staging_desc, None, Some(&mut staging)).unwrap();
            let staging = staging.unwrap();

            let view = view.unwrap();
            let regions = Regions::new(renderer, size).unwrap();
            if let Some(key) = key {
                regions.update(renderer, &view, key).unwrap();
            }
            let background = Some(&regions.mask_view);
            renderer.render(&view, background, target_view.as_ref().unwrap(), size, &params(size, key, ALPHA));
            renderer.context.CopyResource(&staging, &target);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            renderer.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).unwrap();
            let mut out = Vec::new();
            for y in 0..H as usize {
                let row = (mapped.pData as *const u8).add(y * mapped.RowPitch as usize);
                out.extend_from_slice(std::slice::from_raw_parts(row, W as usize * 4));
            }
            renderer.context.Unmap(&staging, 0);
            out
        }
    }

    fn at(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
        let i = ((y * W + x) * 4) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
    }

    #[test]
    fn only_the_background_turns_to_glass() {
        let renderer = match Renderer::new() {
            Ok(renderer) => renderer,
            Err(error) => return eprintln!("skipped, no GPU capture: {error}"),
        };
        let input = scene();
        let output = render_on_gpu(&renderer, &input, Some(KEY));

        let glass = (ALPHA * 255.0).round() as i32;
        let [b, _, _, a] = at(&output, 60, 4);
        assert!((a as i32 - glass).abs() <= 1, "background alpha {a}");
        assert!((b as i32 - (31.0 * ALPHA) as i32).abs() <= 1, "background color {b}");

        assert_eq!(at(&output, 4, 8), at(&input, 4, 8), "text stays solid");
        let edge = at(&output, 5, 8)[3];
        assert!(edge as i32 > glass && edge < 255, "text edge blends, alpha {edge}");

        let mut changed = Vec::new();
        for y in 18..66 {
            for x in (7..33).filter(|_| (23..61).contains(&y)).chain(46..90) {
                if at(&output, x, y) != at(&input, x, y) {
                    changed.push((x, y, at(&input, x, y), at(&output, x, y)));
                }
            }
        }
        assert!(changed.is_empty(), "image pixels changed: {changed:?}");
    }

    #[test]
    fn without_a_background_color_the_app_is_unchanged() {
        let Ok(renderer) = Renderer::new() else { return };
        let input = scene();
        assert_eq!(render_on_gpu(&renderer, &input, None), input);
    }

    /// A grid of blocks that all match, with `holes` cleared.
    fn blocks(width: usize, height: usize, holes: impl Fn(usize, usize) -> bool) -> Vec<u8> {
        (0..width * height)
            .map(|i| if holes(i % width, i / width) { 0 } else { BLOCK as u8 * BLOCK as u8 })
            .collect()
    }

    #[test]
    fn enclosed_patches_are_not_background() {
        let ring = |x: usize, y: usize| {
            (2..=11).contains(&x) && (2..=9).contains(&y) && (x == 2 || x == 11 || y == 2 || y == 9)
        };
        let mask = background_blocks(&blocks(14, 12, ring), 14, 12);
        let at = |x: usize, y: usize| mask[y * 14 + x];
        assert_eq!(at(0, 0), 255, "outside is background");
        assert_eq!(at(2, 5), 255, "the ring's outer edge can blend");
        assert_eq!(at(3, 3), 0, "inside the ring is not");
        assert_eq!(at(6, 6), 0);
    }

    #[test]
    fn letter_holes_and_edge_panes_are_background() {
        let letter = |x: usize, y: usize| (5..=7).contains(&x) && (5..=7).contains(&y) && (x, y) != (6, 6);
        let mask = background_blocks(&blocks(14, 12, letter), 14, 12);
        assert_eq!(mask[6 * 14 + 6], 255, "inside of a letter");

        let divider = |x: usize, _: usize| x == 4 || x == 5;
        let mask = background_blocks(&blocks(14, 12, divider), 14, 12);
        assert_eq!(mask[6 * 14 + 1], 255, "the smaller pane touching the edge");
        assert_eq!(mask[6 * 14 + 10], 255);
    }
}
