use std::ffi::{CStr, CString};
use std::ptr::NonNull;
use std::sync::Arc;

use anyhow::Result;

use tracing::info;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

use wgpu::rwh::{RawDisplayHandle, RawWindowHandle, XcbDisplayHandle, XcbWindowHandle};
use wgpu::SurfaceTargetUnsafe;
use x11rb::protocol::xproto::Screen;
use x11rb_async::blocking::BlockingConnection;

#[cfg(not(debug_assertions))]
use tracing_subscriber::EnvFilter;

#[cfg(debug_assertions)]
const DEBUG_LOG_LEVEL: LevelFilter = LevelFilter::DEBUG;

pub struct Compositor<'a> {
    conn: XConn,
    overlay_win: xproto::Window,
    #[allow(unused)]
    root_size: (u16, u16),
    surface: wgpu::Surface<'a>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
}

use x11rb::xcb_ffi::XCBConnection;
use x11rb_async::connection::Connection;
use x11rb_async::protocol::composite::ConnectionExt as _;
use x11rb_async::protocol::xproto::{self};

/// The main compositor state struct, which manages the [`XConn`],
/// the [`wgpu`] surface, and the [`wgpu`] render pipeline.
impl<'a> Compositor<'a> {
    /// Create a new compositor instance. This will create a new overlay window and
    /// initialize a wgpu surface and render pipeline for it.
    ///
    /// Note that this will show the window immediately, so it should not be called
    /// until you are ready to start rendering.
    pub async fn new(display: Option<&str>) -> Result<Self> {
        let display = display.map(CString::new).transpose()?;
        let conn = x11rb::xcb_ffi::XCBConnection::connect(display.as_deref())
            .map(|(conn, screen)| XConn::new(Arc::new(conn), screen))?;
        let s: &Screen = &conn.setup().roots[conn.screen_num];
        let root = s.root;
        let root_size = (s.width_in_pixels, s.height_in_pixels);

        // It is required to query the composite extension before making any other
        // composite extension requests, or those requests will fail with BadRequest.
        let ver = conn
            .composite_query_version(
                999, // idk just pick a big number
                0,
            )
            .await?
            .reply()
            .await?;

        info!("Composite extension version: {:?}", ver);

        let win_id = conn
            .composite_get_overlay_window(root)
            .await?
            .reply()
            .await?
            .overlay_win;

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            // backends: wgpu::Backends::GL, // setting this to GL fails for some reason
            backends: wgpu::Backends::VULKAN,
            ..Default::default()
        });

        // Safety: we get the raw connection from the XCBConnection, which is a valid XCB connection
        // so this should be safe.
        //
        // We need this to convert the x11tb connection to something wgpu can use.
        let surface = unsafe {
            instance.create_surface_unsafe(SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: RawDisplayHandle::Xcb(XcbDisplayHandle::new(
                    Some(conn.as_raw_connection()),
                    conn.screen().try_into()?,
                )),
                raw_window_handle: RawWindowHandle::Xcb(XcbWindowHandle::new(win_id.try_into()?)),
            })?
        };

        let adapter = instance
            .request_adapter({
                &wgpu::RequestAdapterOptions {
                    // Should this be configurable at some point?
                    // Should high power be the default?
                    power_preference: wgpu::PowerPreference::default(),
                    compatible_surface: Some(&surface),
                    force_fallback_adapter: false,
                }
            })
            .await
            .ok_or_else(|| anyhow::anyhow!("No adapter found"))?;

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    required_features: wgpu::Features::default(),
                    required_limits: wgpu::Limits::default(),
                    label: None,
                },
                None,
            )
            .await?;

        let capabilities = surface.get_capabilities(&adapter);

        let format = capabilities
            .formats
            .iter()
            .filter(|f| f.is_srgb())
            .next()
            .or_else(|| capabilities.formats.get(0))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("No sRGB surface format found"))?;

        let alpha_mode = capabilities
            .alpha_modes
            .get(0)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("No supported usage found"))?;

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: root_size.0 as u32,
            height: root_size.1 as u32,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2, // 2 is the default
        };

        surface.configure(&device, &config);

        Ok(Self {
            conn,
            root_size,
            surface,
            queue,
            device,
            config,
            overlay_win: win_id,
        })
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        // TBD: Can this actually happen in a compositor? not sure how screen attach/detach is
        // handled.
        self.config.width = width as u32;
        self.config.height = height as u32;
        self.surface.configure(&self.device, &self.config);
    }

    pub fn render(&self) -> Result<()> {
        let output = self.surface.get_current_texture()?;

        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Render Encoder"),
            });

        {
            let _ = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.1,
                            g: 0.2,
                            b: 0.3,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });
        }

        // submit will accept anything that implements IntoIter
        self.queue.submit(std::iter::once(encoder.finish()));

        output.present();

        Ok(())
    }

    pub async fn run(&self) -> Result<()> {
        // NOTE: this is just for debugging, so I don't lock myself out of my computer lol
        //
        // Do *NOT* remove this and then "cargo run." You will need to reboot your computer.
        static I: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        loop {
            self.render().ok();
            if I.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 200 {
                break;
            }
            // println!("Event: {:?}", self.conn.wait_for_event().await?);
        }

        Ok(())
    }
}

pub struct XConn {
    raw: Arc<XCBConnection>,
    conn: BlockingConnection<XCBConnection>,
    screen_num: usize,
}

impl XConn {
    pub fn new(raw: Arc<XCBConnection>, screen_num: usize) -> Self {
        let conn = BlockingConnection::new(Arc::clone(&raw));
        Self {
            raw,
            conn,
            screen_num,
        }
    }

    pub fn screen(&self) -> usize {
        self.screen_num
    }

    pub fn as_raw_connection(&self) -> NonNull<std::ffi::c_void> {
        // Safety: XCB should hopefully never hand us a null pointer.
        unsafe { NonNull::new_unchecked(self.raw.get_raw_xcb_connection()) }
    }
}

impl std::ops::Deref for XConn {
    type Target = BlockingConnection<XCBConnection>;

    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_ansi(true)
        .with_file(true)
        .with_timer(UtcTime::rfc_3339())
        .with_filter(
            #[cfg(debug_assertions)]
            DEBUG_LOG_LEVEL,
            #[cfg(not(debug_assertions))]
            EnvFilter::from_default_env(),
        );
    let perf_layer = tracing_timing::Builder::default()
        .layer(|| tracing_timing::Histogram::new(2).expect("to create histogram"));
    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(perf_layer)
        .init();

    info!("Connected to X11 server");

    let session = Compositor::new(None).await?;

    session.run().await?;

    session
        .conn
        .composite_release_overlay_window(session.overlay_win)
        .await?;

    Ok(())
}
