//! A glass window that sits behind another app and blurs whatever is behind it.

use crate::mapping;
use windows::core::{implement, Interface, BOOL, GUID, HSTRING, PCWSTR};
use windows::Foundation::{IPropertyValue, PropertyValue};
use windows::Graphics::Effects::{
    IGraphicsEffect, IGraphicsEffectSource, IGraphicsEffectSource_Impl, IGraphicsEffect_Impl,
};
use windows::System::DispatcherQueueController;
use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::{
    CompositionEffectBrush, CompositionEffectFactory, CompositionEffectSourceParameter,
    CompositionStretch, Compositor, ContainerVisual, ICompositionSurface, SpriteVisual,
};
use windows::Win32::Foundation::{E_INVALIDARG, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_USE_HOSTBACKDROPBRUSH, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND, DWMWINDOWATTRIBUTE,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::WinRT::Composition::ICompositorDesktopInterop;
use windows::Win32::System::WinRT::Graphics::Direct2D::{
    IGraphicsEffectD2D1Interop, IGraphicsEffectD2D1Interop_Impl, GRAPHICS_EFFECT_PROPERTY_MAPPING,
    GRAPHICS_EFFECT_PROPERTY_MAPPING_DIRECT,
};
use windows::Win32::System::WinRT::{
    CreateDispatcherQueueController, DispatcherQueueOptions, RoInitialize, DQTAT_COM_STA,
    DQTYPE_THREAD_CURRENT, RO_INIT_SINGLETHREADED,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindow, GetWindowRect, IsWindowVisible,
    RegisterClassExW, SetWindowPos,
    ShowWindow, GW_HWNDNEXT, HTTRANSPARENT, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE, WM_ERASEBKGND, WM_NCHITTEST, WNDCLASSEXW,
    WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows_numerics::Vector2;

const CLSID_D2D1_GAUSSIAN_BLUR: GUID = GUID::from_u128(0x1feb6d69_2fe6_4ac9_8c58_1d7f93e7a6a5);
const WCA_ACCENT_POLICY: i32 = 19;
const ACCENT_ENABLE_ACRYLICBLURBEHIND: i32 = 4;
const GLASS_CLASS: PCWSTR = windows::core::w!("BlurmanGlass");

#[implement(IGraphicsEffect, IGraphicsEffectSource, IGraphicsEffectD2D1Interop)]
struct BlurEffect {
    name: HSTRING,
    source: IGraphicsEffectSource,
    deviation: f32,
}

impl IGraphicsEffect_Impl for BlurEffect_Impl {
    fn Name(&self) -> windows::core::Result<HSTRING> {
        Ok(self.name.clone())
    }

    fn SetName(&self, _value: &HSTRING) -> windows::core::Result<()> {
        Ok(())
    }
}

impl IGraphicsEffectSource_Impl for BlurEffect_Impl {}

impl IGraphicsEffectD2D1Interop_Impl for BlurEffect_Impl {
    fn GetEffectId(&self) -> windows::core::Result<GUID> {
        Ok(CLSID_D2D1_GAUSSIAN_BLUR)
    }

    fn GetNamedPropertyMapping(
        &self,
        name: &PCWSTR,
        index: *mut u32,
        mapping: *mut GRAPHICS_EFFECT_PROPERTY_MAPPING,
    ) -> windows::core::Result<()> {
        let text = unsafe { name.to_string() }.unwrap_or_default();
        let property = match text.as_str() {
            "BlurAmount" | "StandardDeviation" => 0u32,
            "Optimization" => 1,
            "BorderMode" => 2,
            _ => return Err(E_INVALIDARG.into()),
        };
        unsafe {
            if !index.is_null() {
                *index = property;
            }
            if !mapping.is_null() {
                *mapping = GRAPHICS_EFFECT_PROPERTY_MAPPING_DIRECT;
            }
        }
        Ok(())
    }

    fn GetPropertyCount(&self) -> windows::core::Result<u32> {
        Ok(3)
    }

    fn GetProperty(&self, index: u32) -> windows::core::Result<IPropertyValue> {
        let value = match index {
            0 => PropertyValue::CreateSingle(self.deviation)?,
            // D2D1_GAUSSIANBLUR_OPTIMIZATION_BALANCED and D2D1_BORDER_MODE_HARD, passed as uint32.
            1 => PropertyValue::CreateUInt32(1)?,
            2 => PropertyValue::CreateUInt32(1)?,
            _ => return Err(E_INVALIDARG.into()),
        };
        value.cast()
    }

    fn GetSource(&self, index: u32) -> windows::core::Result<IGraphicsEffectSource> {
        if index == 0 {
            Ok(self.source.clone())
        } else {
            Err(E_INVALIDARG.into())
        }
    }

    fn GetSourceCount(&self) -> windows::core::Result<u32> {
        Ok(1)
    }
}

struct Composition {
    _queue: DispatcherQueueController,
    compositor: Compositor,
    factory: CompositionEffectFactory,
}

/// Must be created and used on one thread that pumps messages.
pub struct GlassSession {
    composition: Option<Composition>,
}

impl GlassSession {
    pub fn new() -> Self {
        let _ = unsafe { RoInitialize(RO_INIT_SINGLETHREADED) };
        let composition = match build_composition() {
            Ok(composition) => Some(composition),
            Err(err) => {
                eprintln!("Adjustable blur is unavailable ({err}). Using system acrylic.");
                None
            }
        };
        Self { composition }
    }

    /// True when the blur radius cannot be changed and the slider sets the tint instead.
    pub fn fallback(&self) -> bool {
        self.composition.is_none()
    }

    pub fn compositor(&self) -> Option<&Compositor> {
        self.composition.as_ref().map(|composition| &composition.compositor)
    }

    pub fn open(&mut self, bounds: RECT, blur: u8) -> Result<GlassPane, String> {
        register_class()?;
        let hwnd = create_glass_window(bounds)?;
        let mut pane = GlassPane {
            hwnd,
            visuals: None,
            topmost: None,
        };
        if let Some(composition) = &self.composition {
            match attach_gaussian(composition, hwnd, blur) {
                Ok(visuals) => {
                    pane.visuals = Some(visuals);
                    return Ok(pane);
                }
                Err(err) => {
                    eprintln!("Adjustable blur failed ({err}). Using system acrylic.");
                    self.composition = None;
                }
            }
        }
        if let Err(err) = apply_acrylic(hwnd, blur) {
            pane.close();
            return Err(err);
        }
        Ok(pane)
    }
}

struct Visuals {
    _target: DesktopWindowTarget,
    root: ContainerVisual,
    brush: CompositionEffectBrush,
    /// The app's picture for solid text, drawn over the blur.
    content: Option<SpriteVisual>,
}

pub struct GlassPane {
    hwnd: HWND,
    visuals: Option<Visuals>,
    /// Which z-order band the pane is in, or None while it is hidden.
    topmost: Option<bool>,
}

impl GlassPane {
    pub fn set_blur(&self, blur: u8) -> Result<(), String> {
        let Some(visuals) = &self.visuals else {
            return apply_acrylic(self.hwnd, blur);
        };
        let radius = mapping::blur_strength_to_radius(blur);
        visuals
            .brush
            .Properties()
            .and_then(|props| props.InsertScalar(&HSTRING::from("Blur.BlurAmount"), radius))
            .map_err(|err| err.to_string())
    }

    /// Draw `surface` over the blur at its own pixel size, pinned to the top-left corner.
    pub fn show_content(&mut self, compositor: &Compositor, surface: &ICompositionSurface) -> Result<(), String> {
        let Some(visuals) = &mut self.visuals else {
            return Err("Solid text needs the adjustable blur, which this PC does not have.".into());
        };
        let build = || -> windows::core::Result<SpriteVisual> {
            let brush = compositor.CreateSurfaceBrushWithSurface(surface)?;
            brush.SetStretch(CompositionStretch::None)?;
            brush.SetHorizontalAlignmentRatio(0.0)?;
            brush.SetVerticalAlignmentRatio(0.0)?;
            let sprite = compositor.CreateSpriteVisual()?;
            sprite.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
            sprite.SetBrush(&brush)?;
            visuals.root.Children()?.InsertAtTop(&sprite)?;
            Ok(sprite)
        };
        let sprite = build().map_err(|err| format!("Could not draw the app on the glass: {}", err.message()))?;
        if let Some(old) = visuals.content.replace(sprite) {
            let _ = visuals.root.Children().and_then(|children| children.Remove(&old));
        }
        Ok(())
    }

    pub fn hide_content(&mut self) {
        if let Some(visuals) = &mut self.visuals {
            if let Some(sprite) = visuals.content.take() {
                let _ = visuals.root.Children().and_then(|children| children.Remove(&sprite));
            }
        }
    }

    /// Size the pane to `bounds` and put it directly beneath `target` in the z-order.
    pub fn place_under(&mut self, target: HWND, bounds: RECT, topmost: bool) {
        if self.topmost == Some(topmost) && self.sits_under(target, bounds) {
            return;
        }
        unsafe {
            if topmost != self.topmost.unwrap_or(false) {
                let band = if topmost { HWND_TOPMOST } else { HWND_NOTOPMOST };
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(band),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
            let _ = SetWindowPos(
                self.hwnd,
                Some(target),
                bounds.left,
                bounds.top,
                (bounds.right - bounds.left).max(1),
                (bounds.bottom - bounds.top).max(1),
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
        self.topmost = Some(topmost);
    }

    /// Checks the real window rather than a cached position, so outside moves get corrected.
    fn sits_under(&self, target: HWND, bounds: RECT) -> bool {
        let mut rect = RECT::default();
        unsafe {
            GetWindowRect(self.hwnd, &mut rect).is_ok()
                && rect == bounds
                && IsWindowVisible(self.hwnd).as_bool()
                && GetWindow(target, GW_HWNDNEXT).ok() == Some(self.hwnd)
        }
    }

    pub fn hide(&mut self) {
        if self.topmost.take().is_some() {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
    }

    pub fn close(mut self) {
        self.visuals = None;
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

fn build_composition() -> windows::core::Result<Composition> {
    let queue = unsafe {
        CreateDispatcherQueueController(DispatcherQueueOptions {
            dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
            threadType: DQTYPE_THREAD_CURRENT,
            apartmentType: DQTAT_COM_STA,
        })?
    };
    let compositor = Compositor::new()?;
    compositor.CreateHostBackdropBrush()?;
    let source = CompositionEffectSourceParameter::Create(&HSTRING::from("backdrop"))?;
    let effect: IGraphicsEffect = BlurEffect {
        name: HSTRING::from("Blur"),
        source: source.cast()?,
        deviation: mapping::blur_strength_to_radius(mapping::BLUR_DEFAULT),
    }
    .into();
    let animatable = windows_collections::IIterable::<HSTRING>::from(vec![HSTRING::from(
        "Blur.BlurAmount",
    )]);
    let factory = compositor.CreateEffectFactoryWithProperties(&effect, &animatable)?;
    Ok(Composition {
        _queue: queue,
        compositor,
        factory,
    })
}

fn attach_gaussian(composition: &Composition, hwnd: HWND, blur: u8) -> windows::core::Result<Visuals> {
    set_dwm_bool(hwnd, DWMWA_USE_HOSTBACKDROPBRUSH, true)?;
    let compositor = &composition.compositor;
    let interop: ICompositorDesktopInterop = compositor.cast()?;
    let target = unsafe { interop.CreateDesktopWindowTarget(hwnd, false)? };
    let brush = composition.factory.CreateBrush()?;
    brush.SetSourceParameter(&HSTRING::from("backdrop"), &compositor.CreateHostBackdropBrush()?)?;
    brush
        .Properties()?
        .InsertScalar(&HSTRING::from("Blur.BlurAmount"), mapping::blur_strength_to_radius(blur))?;

    let sprite = compositor.CreateSpriteVisual()?;
    sprite.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
    sprite.SetBrush(&brush)?;
    let root = compositor.CreateContainerVisual()?;
    root.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
    root.Children()?.InsertAtTop(&sprite)?;
    target.SetRoot(&root)?;
    Ok(Visuals {
        _target: target,
        root,
        brush,
        content: None,
    })
}

fn set_dwm_bool(hwnd: HWND, attribute: DWMWINDOWATTRIBUTE, value: bool) -> windows::core::Result<()> {
    let value = BOOL::from(value);
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            attribute,
            &value as *const BOOL as *const std::ffi::c_void,
            std::mem::size_of::<BOOL>() as u32,
        )
    }
}

fn create_glass_window(bounds: RECT) -> Result<HWND, String> {
    let instance = unsafe { GetModuleHandleW(None) }.map_err(|err| err.to_string())?;
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT | WS_EX_NOREDIRECTIONBITMAP,
            GLASS_CLASS,
            PCWSTR::null(),
            WS_POPUP,
            bounds.left,
            bounds.top,
            (bounds.right - bounds.left).max(1),
            (bounds.bottom - bounds.top).max(1),
            None,
            None,
            Some(HINSTANCE(instance.0)),
            None,
        )
    }
    .map_err(|err| format!("Could not create the glass window: {err}"))?;
    let corner = DWMWCP_ROUND;
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const std::ffi::c_void,
            std::mem::size_of_val(&corner) as u32,
        );
    }
    Ok(hwnd)
}

fn register_class() -> Result<(), String> {
    static RESULT: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    RESULT
        .get_or_init(|| {
            let instance = unsafe { GetModuleHandleW(None) }.map_err(|err| err.to_string())?;
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(glass_proc),
                hInstance: instance.into(),
                lpszClassName: GLASS_CLASS,
                ..Default::default()
            };
            if unsafe { RegisterClassExW(&class) } == 0 {
                return Err("Could not register the glass window class.".into());
            }
            Ok(())
        })
        .clone()
}

unsafe extern "system" fn glass_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

#[repr(C)]
struct AccentPolicy {
    state: i32,
    flags: i32,
    color: u32,
    animation: i32,
}

#[repr(C)]
struct CompositionAttribute {
    attribute: i32,
    data: *mut AccentPolicy,
    size: usize,
}

type SetCompositionFn = unsafe extern "system" fn(HWND, *const CompositionAttribute) -> BOOL;

fn composition_fn() -> Option<SetCompositionFn> {
    static FN: std::sync::OnceLock<Option<SetCompositionFn>> = std::sync::OnceLock::new();
    *FN.get_or_init(|| unsafe {
        let lib = LoadLibraryW(windows::core::w!("user32.dll")).ok()?;
        let proc = GetProcAddress(lib, windows::core::s!("SetWindowCompositionAttribute"))?;
        Some(std::mem::transmute::<unsafe extern "system" fn() -> isize, SetCompositionFn>(proc))
    })
}

/// Undocumented acrylic blur-behind. It keeps rendering while the window is inactive.
fn apply_acrylic(hwnd: HWND, blur: u8) -> Result<(), String> {
    let set = composition_fn().ok_or("SetWindowCompositionAttribute is missing.")?;
    let alpha = mapping::blur_strength_to_tint_alpha(blur) as u32;
    // ABGR: a neutral dark tint. The alpha byte is what the slider moves.
    let mut policy = AccentPolicy {
        state: ACCENT_ENABLE_ACRYLICBLURBEHIND,
        flags: 2,
        color: (alpha << 24) | 0x0018_1818,
        animation: 0,
    };
    let data = CompositionAttribute {
        attribute: WCA_ACCENT_POLICY,
        data: &mut policy,
        size: std::mem::size_of::<AccentPolicy>(),
    };
    if unsafe { set(hwnd, &data) }.as_bool() {
        Ok(())
    } else {
        Err("System acrylic was rejected.".into())
    }
}
