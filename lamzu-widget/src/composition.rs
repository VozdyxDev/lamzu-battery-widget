use std::io;

use windows::{
    Win32::{
        Foundation::{HMODULE, HWND, POINT},
        Graphics::{
            Direct2D::{
                Common::{
                    D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT,
                },
                D2D1_BITMAP_OPTIONS_NONE, D2D1_BITMAP_PROPERTIES1,
                D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_PRIMITIVE_BLEND_COPY,
                D2D1CreateDevice, ID2D1DeviceContext,
            },
            Direct3D::{D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP},
            Direct3D11::{D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice},
            DirectComposition::{
                DCompositionCreateDevice2, IDCompositionDesktopDevice, IDCompositionSurface,
                IDCompositionTarget, IDCompositionVisual2,
            },
            Dxgi::{
                Common::{DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM},
                IDXGIAdapter, IDXGIDevice,
            },
        },
    },
    core::Interface,
};

pub struct Composition {
    device: IDCompositionDesktopDevice,
    _target: IDCompositionTarget,
    visual: IDCompositionVisual2,
    surface: Option<(u32, u32, IDCompositionSurface)>,
}

impl Composition {
    pub fn new(hwnd: windows_sys::Win32::Foundation::HWND) -> io::Result<Self> {
        let dxgi_device =
            dxgi_device(D3D_DRIVER_TYPE_HARDWARE).or_else(|_| dxgi_device(D3D_DRIVER_TYPE_WARP))?;
        unsafe {
            let renderer = D2D1CreateDevice(&dxgi_device, None).map_err(io::Error::other)?;
            let device: IDCompositionDesktopDevice =
                DCompositionCreateDevice2(&renderer).map_err(io::Error::other)?;
            let target = device
                .CreateTargetForHwnd(HWND(hwnd), true)
                .map_err(io::Error::other)?;
            let visual = device.CreateVisual().map_err(io::Error::other)?;
            target.SetRoot(&visual).map_err(io::Error::other)?;
            Ok(Self {
                device,
                _target: target,
                visual,
                surface: None,
            })
        }
    }

    pub fn present(&mut self, pixels: &[u32], width: i32, height: i32) -> io::Result<()> {
        if width <= 0
            || width > i32::MAX / 4
            || height <= 0
            || i64::from(width) * i64::from(height) != pixels.len() as i64
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid composition bitmap dimensions",
            ));
        }
        let width = width as u32;
        let height = height as u32;

        if let Some((w, h, surface)) = &self.surface
            && (*w, *h) == (width, height)
        {
            paint(surface, pixels, width, height).map_err(io::Error::other)?;
        } else {
            let surface = unsafe {
                self.device.CreateSurface(
                    width,
                    height,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_ALPHA_MODE_PREMULTIPLIED,
                )
            }
            .map_err(io::Error::other)?;
            paint(&surface, pixels, width, height).map_err(io::Error::other)?;
            unsafe { self.visual.SetContent(&surface) }.map_err(io::Error::other)?;
            self.surface = Some((width, height, surface));
        }
        unsafe { self.device.Commit() }.map_err(io::Error::other)
    }
}

fn dxgi_device(driver: D3D_DRIVER_TYPE) -> io::Result<IDXGIDevice> {
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .map_err(io::Error::other)?;
    device
        .ok_or_else(|| io::Error::other("D3D11 returned no graphics device"))?
        .cast()
        .map_err(io::Error::other)
}

fn paint(
    surface: &IDCompositionSurface,
    pixels: &[u32],
    width: u32,
    height: u32,
) -> windows::core::Result<()> {
    let mut offset = POINT::default();
    let size = D2D_SIZE_U { width, height };
    let properties = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
        ..Default::default()
    };
    unsafe {
        let context = surface.BeginDraw::<ID2D1DeviceContext>(None, &mut offset)?;
        let bitmap =
            context.CreateBitmap(size, Some(pixels.as_ptr().cast()), width * 4, &properties);
        if let Ok(bitmap) = &bitmap {
            let left = offset.x as f32;
            let top = offset.y as f32;
            let destination = D2D_RECT_F {
                left,
                top,
                right: left + width as f32,
                bottom: top + height as f32,
            };
            context.SetDpi(96.0, 96.0);
            // Overwrite the entire update rectangle, including transparent corners.
            context.SetPrimitiveBlend(D2D1_PRIMITIVE_BLEND_COPY);
            context.DrawBitmap(
                bitmap,
                Some(&destination),
                1.0,
                D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                None,
                None,
            );
        }
        let finished = surface.EndDraw();
        bitmap?;
        finished
    }
}
