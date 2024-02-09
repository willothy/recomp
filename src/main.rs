use std::num::NonZeroU32;
use std::ptr::NonNull;
use std::sync::Arc;

use anyhow::Result;

use tracing::{info, warn};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};
use wgpu::{SurfaceTarget, SurfaceTargetUnsafe, WindowHandle};
use winit::event_loop::{EventLoop, EventLoopBuilder, EventLoopWindowTarget};
use winit::platform::x11::EventLoopWindowTargetExtX11;
use x11rb::protocol::composite::ConnectionExt;
use x11rb::protocol::xproto::{CreateWindowAux, Screen, WindowClass};
use x11rb_async::blocking::BlockingConnection;

use x11rb::COPY_DEPTH_FROM_PARENT;

#[cfg(debug_assertions)]
const DEBUG_LOG_LEVEL: LevelFilter = LevelFilter::DEBUG;

pub struct Session<'a> {
    // conn: RustConnection,
    // conn: BlockingConnection<XCBConnection>,
    // screen_num: usize,
    conn: XConn,
    overlay_win: xproto::Window,
    root_size: (u32, u32),
    surface: wgpu::Surface<'a>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
}

use x11rb::xcb_ffi::XCBConnection;
use x11rb_async::connection::Connection;
use x11rb_async::protocol::composite::ConnectionExt as _;
use x11rb_async::protocol::xproto::{self, ConnectionExt as _};

impl<'a> Session<'a> {
    pub async fn new(conn: XConn) -> Result<Self> {
        // let conn = BlockingConnection::new(Arc::clone(&raw));
        let (root, root_size) = {
            let screen: &Screen = &conn.conn.setup().roots[conn.screen_num];
            (
                screen.root,
                (
                    screen.width_in_pixels as u32,
                    screen.height_in_pixels as u32,
                ),
            )
        };

        // conn.ver

        let ver = conn
            .composite_query_version(
                999, //
                0,
            )
            .await?
            .reply()
            .await?;

        info!("Composite extension version: {:?}", ver);

        let win = conn
            .composite_get_overlay_window(root)
            .await?
            .reply()
            .await?;

        let win_id = win.overlay_win;

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            // backends: wgpu::Backends::GL, // setting this to GL fails for some reason
            backends: wgpu::Backends::VULKAN,
            ..Default::default()
        });

        let win = winit::raw_window_handle::XcbWindowHandle::new(win_id.try_into()?);
        let scr = winit::raw_window_handle::XcbDisplayHandle::new(
            Some(NonNull::new(conn.raw.get_raw_xcb_connection()).expect("Non-null")),
            conn.screen_num.try_into()?,
        );

        let surface = unsafe {
            instance.create_surface_unsafe(SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: scr.into(),
                raw_window_handle: win.into(),
            })?
        };

        let adapter = instance
            .request_adapter({
                &wgpu::RequestAdapterOptions {
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

        let surface_caps = surface.get_capabilities(&adapter);

        let surface_format = surface_caps
            .formats
            .iter()
            .filter(|f| f.is_srgb())
            .next()
            .or_else(|| surface_caps.formats.get(0))
            .ok_or_else(|| anyhow::anyhow!("No sRGB surface format found"))?
            .clone();

        let alpha_mode = surface_caps
            .alpha_modes
            .get(0)
            .ok_or_else(|| anyhow::anyhow!("No supported usage found"))?
            .clone();

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: root_size.0,
            height: root_size.1,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2, // 2 is the default
        };

        surface.configure(&device, &config);

        conn.map_window(win_id).await?.check().await?;
        conn.flush().await?;

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

        // self.conn.damage_add(self.win_id, 0).await?;
        output.present();

        Ok(())
    }

    pub async fn run(&self) -> Result<()> {
        use winit::platform::x11::*;
        // let ev = EventLoopBuilder::new().with_x11().build()?;

        // winit::window::WindowBuilder::new().;

        static I: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        // ev.run(move |e, t| {})?;
        loop {
            self.render().ok();
            if I.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 200 {
                break;
            }
            println!("Event: {:?}", self.conn.wait_for_event().await?);
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

    pub fn as_ptr(&self) -> *mut std::ffi::c_void {
        self.raw.get_raw_xcb_connection()
    }
}

impl AsRef<XCBConnection> for XConn {
    fn as_ref(&self) -> &XCBConnection {
        &self.raw
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

    let (conn, screen_num) = x11rb::xcb_ffi::XCBConnection::connect(None)?;
    let conn = XConn::new(Arc::new(conn), screen_num);
    info!("Connected to X11 server");

    let session = Session::new(conn).await?;

    session.run().await?;

    session
        .conn
        .composite_release_overlay_window(session.overlay_win)
        .await?;

    Ok(())
}
