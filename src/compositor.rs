use std::{ffi::CString, sync::Arc};

use anyhow::Result;
use tracing::{info, warn};
use wgpu::{
    rwh::{RawDisplayHandle, RawWindowHandle, XcbDisplayHandle, XcbWindowHandle},
    SurfaceTargetUnsafe,
};
use x11rb::protocol::{
    composite::Redirect,
    shape::SK,
    xproto::{self, CreateWindowAux, WindowClass},
};
use x11rb_async::{
    connection::Connection,
    protocol::{
        composite::ConnectionExt as _, damage::ConnectionExt, xfixes::ConnectionExt as _,
        xproto::ConnectionExt as _,
    },
};

use crate::connection::XConn;

pub struct Compositor<'a> {
    conn: XConn,
    root_win: xproto::Window,
    overlay_win: xproto::Window,
    #[allow(unused)]
    root_size: (u16, u16),
    surface: wgpu::Surface<'a>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
}

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
        let setup = conn.setup();
        let screen: &xproto::Screen = &setup.roots[conn.screen()];

        let root: xproto::Window = screen.root;
        let root_size = (screen.width_in_pixels, screen.height_in_pixels);

        // It is required to query the composite extension before making any other
        // composite extension requests, or those requests will fail with BadRequest.
        let composite_version = conn
            .composite_query_version(
                999, // idk just pick a big number
                0,
            )
            .await?
            .reply()
            .await?;
        info!("Composite extension version: {:?}", composite_version);

        let xfixes_version = conn.xfixes_query_version(999, 0).await?.reply().await?;
        info!("XFixes extension version: {:?}", xfixes_version);

        let xdamage_version = conn.damage_query_version(999, 0).await?.reply().await?;
        info!("XDamage extension version: {:?}", xdamage_version);

        // let wid = conn.generate_id().await?;
        // conn.create_window(
        //     0,
        //     wid,
        //     root,
        //     0,
        //     0,
        //     root_size.0,
        //     root_size.1,
        //     0,
        //     WindowClass::COPY_FROM_PARENT,
        //     0,
        //     &CreateWindowAux::default(),
        // )
        // .await?
        // .check()
        // .await?;
        //
        // let selection = conn
        //     .intern_atom(false, format!("REGISTER_PROP{}", conn.screen()).as_bytes())
        //     .await?
        //     .reply()
        //     .await?;

        // selection.atom

        // conn.set_selection_owner(wid, selection.atom, 0u32)
        //     .await?
        //     .check()
        //     .await?;

        // conn.xutf

        // Redirect all current and future children of the root window.
        conn.composite_redirect_subwindows(root, Redirect::AUTOMATIC)
            .await?
            .check()
            .await?;

        let win_id = conn
            .composite_get_overlay_window(root)
            .await?
            .reply()
            .await?
            .overlay_win;
        info!("Overlay window: {:?}", win_id);

        // let tree = conn.query_tree(root).await?.reply().await?;
        // info!("Tree: {:?}", tree);

        // Allow event pass-through to the root window
        let region = conn.generate_id().await?;
        conn.xfixes_create_region(region, &[])
            .await?
            .check()
            .await?;
        // conn.xfixes_set_window_shape_region(win_id, SK::BOUNDING, 0, 0, region)
        //     .await?
        //     .check()
        //     .await?;
        conn.xfixes_set_window_shape_region(win_id, SK::INPUT, 0, 0, region)
            .await?
            .check()
            .await?;
        conn.xfixes_destroy_region(region).await?.check().await?;

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
            root_win: root,
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

        encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Render Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.1,
                        g: 0.2,
                        b: 0.5,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            occlusion_query_set: None,
            timestamp_writes: None,
        });

        // submit will accept anything that implements IntoIter
        self.queue.submit(std::iter::once(encoder.finish()));

        output.present();

        Ok(())
    }

    pub async fn run(&self) -> Result<()> {
        loop {
            self.render()?;
            match self.conn.poll_for_event()? {
                Some(ev) => {
                    match ev {
                        x11rb::protocol::Event::Unknown(_) => info!("Unknown event"),
                        x11rb::protocol::Event::Error(err) => warn!("X11 Error: {:?}", err),
                        x11rb::protocol::Event::ButtonPress(btn) => {
                            info!("ButtonPress: {:?}", btn);
                        }
                        x11rb::protocol::Event::CreateNotify(ev) => {
                            info!("CreateNotify: {:?}", ev);
                        }
                        x11rb::protocol::Event::DestroyNotify(ev) => {
                            info!("DestroyNotify: {:?}", ev);
                        }
                        x11rb::protocol::Event::EnterNotify(ev) => {
                            info!("EnterNotify: {:?}", ev);
                        }
                        x11rb::protocol::Event::FocusIn(ev) => {
                            info!("FocusIn: {:?}", ev);
                        }
                        x11rb::protocol::Event::FocusOut(ev) => {
                            info!("FocusOut: {:?}", ev);
                        }
                        x11rb::protocol::Event::KeyPress(ev) => {
                            info!("KeyPress: {:?}", ev);
                        }
                        x11rb::protocol::Event::KeyRelease(ev) => {
                            info!("KeyRelease: {:?}", ev);
                        }
                        x11rb::protocol::Event::LeaveNotify(ev) => {
                            info!("LeaveNotify: {:?}", ev);
                        }
                        x11rb::protocol::Event::MapNotify(ev) => {
                            info!("MapNotify: {:?}", ev);
                        }
                        x11rb::protocol::Event::MapRequest(ev) => {
                            info!("MapRequest: {:?}", ev);
                        }
                        x11rb::protocol::Event::MappingNotify(ev) => {
                            info!("MappingNotify: {:?}", ev);
                        }
                        x11rb::protocol::Event::PropertyNotify(ev) => {
                            info!("PropertyNotify: {:?}", ev);
                        }
                        x11rb::protocol::Event::ReparentNotify(ev) => {
                            info!("ReparentNotify: {:?}", ev);
                        }
                        x11rb::protocol::Event::UnmapNotify(ev) => {
                            info!("UnmapNotify: {:?}", ev);
                        }
                        x11rb::protocol::Event::VisibilityNotify(ev) => {
                            info!("VisibilityNotify: {:?}", ev);
                        }
                        x11rb::protocol::Event::DamageNotify(ev) => {
                            info!("DamageNotify: {:?}", ev);
                        }
                        ev => {
                            warn!("Unhandled event: {:?}", ev);
                        }
                    };
                    break;
                }
                None => {}
            }
            // if I.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 200 {
            //     break;
            // }
            // println!("Event: {:?}", self.conn.wait_for_event().await?);
        }

        Ok(())
    }
}

impl Drop for Compositor<'_> {
    fn drop(&mut self) {
        let conn = self.conn.clone();
        let root = self.root_win;
        let overlay = self.overlay_win;
        tokio::spawn(async move {
            conn.composite_unredirect_subwindows(root, Redirect::AUTOMATIC)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))
                .ok();

            conn.composite_release_overlay_window(overlay).await.ok();
        });
    }
}
