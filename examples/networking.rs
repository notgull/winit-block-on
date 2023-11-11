// SPDX-License-Identifier: BSL-1.0 OR Apache-2.0
//               Copyright John Nunley, 2023.
// Distributed under the Boost Software License, Version 1.0 or the Apache
//                 License, Version 2.0.
//       (See accompanying file LICENSE or copy at
//         https://www.boost.org/LICENSE_1_0.txt)

//! A somewhat more complex example than what's given in the docs that uses a single thread
//! to handle both networking and rendering.
//!
//! This example uses `winit` to manage the thread, `softbuffer` to draw to the screen,
//! `rustybuzz` to lay out text, `async-h1` to make HTTP requests, `async-rustls` for
//! TLS and `smol` to tie the async bits together.

// Libstd imports.
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::convert::TryFrom;
use std::env;
use std::error::Error;
use std::fmt::Write as _;
use std::net::TcpStream;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context as AsyncContext, Poll};
use std::time::Duration;

// Asynchronous I/O imports.
use async_io::Async;
use futures_lite::{future, io, prelude::*};
use http_types::{Method, Request, Url};

// TLS imports.
use async_rustls::{client::TlsStream, TlsConnector};
use rustls::OwnedTrustAnchor;

// Text rasterizing imports.
use rustybuzz::Face as LayoutFace;
use softbuffer::{Context, Surface};
use ttf_parser::Face;

// Winit imports.
use winit::dpi::LogicalSize;
use winit::event::{Event, KeyEvent, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop, EventLoopBuilder};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowBuilder};
use winit_block_on::{prelude::*, Signal};

// Convenient error handling aliases.
type BoxError = Box<dyn Error + 'static>;
type Result<T> = std::result::Result<T, BoxError>;

// Important constants.
const WIDTH: u32 = 800;
const HEIGHT: u32 = 600;
const DRAW_BUFFER_SIZE: usize = WIDTH as usize * HEIGHT as usize;

// Default URLs to use if none are passed in.
const DEFAULT_URLS: &[&str] = &[
    "https://www.rust-lang.org/",
    "https://www.google.com/",
    "http://neverssl.com",
    "https://serde.rs",
];

struct Rerender;

struct AppState {
    /// The in-flight HTTP applications.
    http_states: Vec<HttpState>,
}

struct HttpState {
    url: String,
    status: Cow<'static, str>,
    progress: u8,
    was_changed: bool,
    error: bool,
}

impl HttpState {
    fn new(url: String) -> Self {
        Self {
            url,
            status: "Waiting to start...".into(),
            progress: 0,
            was_changed: false,
            error: false,
        }
    }

    fn add_progress(&mut self, new_status: &'static str, new_progress: u8) {
        self.status = new_status.into();
        self.progress += new_progress;

        if self.progress > 100 {
            self.progress = 100;
        }

        self.was_changed = true;
    }
}

/// Send an HTTP request.
///
/// While sending the request, indicate how much of it we've sent.
async fn get_http(url: &str, state: &RefCell<AppState>, index: usize) -> Result<Vec<u8>> {
    let update = move |new_status: &'static str, new_progress: u8| {
        state.borrow_mut().http_states[index].add_progress(new_status, new_progress);
    };

    let url = Url::parse(url)?;
    let use_tls = url.scheme() == "https";

    let socket_list = {
        let domain = url
            .domain()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "No domain in URL"))?;
        let port = match (url.port(), use_tls) {
            (Some(port), _) => port,
            (None, true) => 443,
            (None, false) => 80,
        };

        let url_to_lookup = format!("{}:{}", domain, port);

        blocking::unblock(move || {
            std::net::ToSocketAddrs::to_socket_addrs(&url_to_lookup)
                .map(|sock| sock.collect::<Vec<_>>())
        })
        .await?
    };

    // Update progress.
    update("Connecting to server...", 10);

    // Connect to the server.
    let stream = {
        let mut error = None;
        let mut socks = socket_list.into_iter();
        let stream = loop {
            let sock = socks.next().ok_or_else(|| {
                error.unwrap_or_else(|| io::Error::new(io::ErrorKind::Other, "No more sockets"))
            })?;

            match Async::<TcpStream>::connect(sock).await {
                Ok(stream) => break stream,
                Err(e) => {
                    error = Some(e);
                    continue;
                }
            }
        };

        stream
    };

    // Update progress.
    update("Performing TLS handshake...", 10);

    let stream = {
        if use_tls {
            // Set up the TLS configuration.
            let mut root_cert_store = rustls::RootCertStore::empty();
            root_cert_store.add_server_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.0.iter().map(
                |ta| {
                    OwnedTrustAnchor::from_subject_spki_name_constraints(
                        ta.subject,
                        ta.spki,
                        ta.name_constraints,
                    )
                },
            ));

            let config = rustls::ClientConfig::builder()
                .with_safe_defaults()
                .with_root_certificates(root_cert_store)
                .with_no_client_auth();

            let connector = TlsConnector::from(Arc::new(config));
            let domain = url
                .domain()
                .and_then(|domain| rustls::ServerName::try_from(domain).ok())
                .ok_or_else(|| BoxError::from("No domain in URL"))?;
            let stream = connector.connect(domain, stream).await?;

            Connection::Tls(stream)
        } else {
            Connection::NoTls(stream)
        }
    };

    // Update progress.
    update("Sending HTTP request...", 15);

    // Create the HTTP request.
    let req = Request::new(Method::Get, url);
    // TODO: Headers

    // Send the request.
    // TODO: Keep track of how much progress we've made.
    let mut response = async_h1::client::connect(stream, req).await?;

    // Update progress.
    update("Reading response...", 35);

    // Yield to let other requests be processed.
    future::yield_now().await;

    // Get the bytes from the response.
    let bytes = response.body_bytes().await?;

    // Update progress.
    update("Parsing response...", 30);

    Ok(bytes)
}

/// The actual networking function.
async fn network(state: &RefCell<AppState>) -> Result<()> {
    let ongoing_op_count = Cell::new(0);

    // Spawn an HTTP task for every target url.
    let ex = async_executor::LocalExecutor::new();

    state.borrow().http_states.iter().enumerate().for_each({
        let ongoing_op_count = &ongoing_op_count;
        let ex = &ex;

        move |(i, http)| {
            ongoing_op_count.set(ongoing_op_count.get() + 1);

            // Spawn the future that runs the URL.
            let url = http.url.to_string();
            let fut = async move {
                let data = match get_http(&url, state, i).await {
                    Ok(data) => data,
                    Err(err) => {
                        // Borrow the state.
                        let mut state = state.borrow_mut();
                        let http = &mut state.http_states[i];

                        // Return the error.
                        http.error = true;
                        http.progress = 100;
                        http.status = Cow::Owned(format!("Error: {err}"));

                        return;
                    }
                };

                // Yield and then parse some statistics.
                future::yield_now().await;
                let bytes = data.len();
                let mut analysis = format!("# of Bytes: {bytes}");

                if let Ok(data) = String::from_utf8(data) {
                    let num_opening_braces = data.chars().filter(|&c| c == '<').count();
                    write!(
                        &mut analysis,
                        " | # of Opening Braces: {num_opening_braces}"
                    )
                    .ok();
                }

                ongoing_op_count.set(ongoing_op_count.get() - 1);

                let mut state = state.borrow_mut();
                let http = &mut state.http_states[i];
                http.progress = 100;
                http.status = Cow::Owned(analysis);
            };

            ex.spawn(fut).detach();
        }
    });

    // Also wake up once every 100 ms while we're still running.
    ex.run({
        let ongoing_op_count = &ongoing_op_count;
        async move {
            let mut timer = async_io::Timer::interval(Duration::from_millis(100));

            while ongoing_op_count.get() > 0 {
                timer.next().await;
            }
        }
    })
    .await;

    Ok(())
}

/// Structure for drawing to the window.
struct Draw {
    /// The face we're using to draw.
    face: Face<'static>,

    /// A buffer for unicode.
    unicode_buffer: Option<rustybuzz::UnicodeBuffer>,

    /// The drawing surface.
    surface: Surface,

    /// The buffer to draw to.
    draw_buffer: Box<[u32; DRAW_BUFFER_SIZE]>,
}

impl Draw {
    fn new(evl: &EventLoop<Signal<Rerender>>, window: &Window) -> Result<Self> {
        // Create a face from the font.
        let face = Face::from_slice(include_bytes!("../assets/Roboto-Regular.ttf"), 0)?;

        // Create a buffer for unicode.
        let buffer = rustybuzz::UnicodeBuffer::new();

        // Create a drawing context.
        let context = unsafe { Context::new(&**evl)? };

        // Create a drawing surface.
        let surface = unsafe { Surface::new(&context, window)? };

        Ok(Self {
            face,
            unicode_buffer: Some(buffer),
            surface,
            draw_buffer: Box::new([0; DRAW_BUFFER_SIZE]),
        })
    }

    fn render(&mut self, state: &AppState) -> Result<()> {
        // Clear the surface with white.
        self.draw_buffer.fill(0xFFFFFF);

        let x = 20;
        let mut y = 20;
        let mut all_done = true;

        for http in &state.http_states {
            self.draw_text(&http.url, x, y)?;
            self.draw_text(&http.status, x + 20, y + 20)?;
            self.draw_progress_bar(http.progress, y + 40, http.error)?;
            y += 100;

            if http.progress != 100 {
                all_done = false;
            }
        }

        if all_done {
            self.draw_text("Press [R] to restart", 120, y)?;
        }

        // Draw the image to the surface.
        self.surface.resize(
            NonZeroU32::new(WIDTH.into()).unwrap(),
            NonZeroU32::new(HEIGHT.into()).unwrap(),
        )?;
        let mut buffer = self.surface.buffer_mut()?;
        buffer.copy_from_slice(&*self.draw_buffer);

        Ok(())
    }

    fn draw_text(&mut self, text: &str, x: u32, y: u32) -> Result<()> {
        // Push the text to the buffer.
        let mut unicode_buffer = self
            .unicode_buffer
            .take()
            .unwrap_or_else(rustybuzz::UnicodeBuffer::new);
        unicode_buffer.clear();
        unicode_buffer.push_str(text);

        // A RustType font.
        let font = rusttype::Font::Ref(Arc::new(self.face.clone()));

        // Shape the text.
        let glyph_buffer = rustybuzz::shape(
            &LayoutFace::from_face(self.face.clone())
                .ok_or_else(|| BoxError::from("Failed to create layout face"))?,
            &[],
            unicode_buffer,
        );

        let scale = rusttype::Scale::uniform(16.0);

        // Iterate over the glyphs and their positions.
        glyph_buffer
            .glyph_infos()
            .iter()
            .copied()
            .zip(glyph_buffer.glyph_positions().iter().copied())
            .try_fold((x, y), |(mut x, mut y), (info, position)| {
                // Get the glyph.
                let glyph = font.glyph(rusttype::GlyphId(info.glyph_id as _));

                // Scale and position it.
                let glyph = glyph.scaled(scale).positioned(rusttype::Point {
                    x: x as f32,
                    y: y as f32,
                });

                if let Some(bbx) = glyph.pixel_bounding_box() {
                    // Draw the glyph.
                    glyph.draw(|dx, dy, pixel| {
                        let (mut dx, mut dy) = (dx as i32, dy as i32);
                        dx += bbx.min.x;
                        dy += bbx.min.y;

                        let start_index = dx as usize + (dy as usize * WIDTH as usize);
                        let buffer_start = &mut self.draw_buffer[start_index];
                        let buffer =
                            bytemuck::cast_slice_mut::<u32, u8>(std::slice::from_mut(buffer_start));

                        for i in 0..4 {
                            buffer[i] = ((1.0 - pixel) * 255.0) as u8;
                        }
                    });
                }

                x += position.x_advance as u32 / 100;
                y += position.y_advance as u32 / 100;

                Result::Ok((x, y))
            })?;

        // Re-use the buffer for the next time we draw.
        self.unicode_buffer = Some(glyph_buffer.clear());

        Ok(())
    }

    /// Draw a progress bar.
    fn draw_progress_bar(&mut self, progress: u8, y: u32, error: bool) -> Result<()> {
        use tiny_skia::Color;

        let mut pixmap = tiny_skia::PixmapMut::from_bytes(
            bytemuck::cast_slice_mut::<u32, u8>(&mut *self.draw_buffer),
            WIDTH,
            HEIGHT,
        )
        .unwrap();
        let mut builder = None;

        // Draw a rectangle.
        let mut draw_rect = move |x, y, width: f32, height: f32, color| {
            // Rectangle shaped path.
            let path = {
                let mut path = builder
                    .take()
                    .unwrap_or_else(tiny_skia_path::PathBuilder::new);
                path.move_to(x, y);
                path.line_to(x + width, y);
                path.line_to(x + width, y + height);
                path.line_to(x, y + height);
                path.close();

                path.finish().unwrap()
            };

            // Paint the whole thing with `color`.
            let paint = {
                let mut paint = tiny_skia::Paint::default();
                paint.set_color(color);
                paint
            };

            // Run the fill operation.
            pixmap
                .fill_path(
                    &path,
                    &paint,
                    tiny_skia::FillRule::EvenOdd,
                    tiny_skia::Transform::default(),
                    None,
                )
                .ok_or_else(|| BoxError::from("Failed to paint progress bar"))?;

            Result::Ok(())
        };

        // Draw the bars.
        draw_rect(
            20.0,
            y as _,
            (WIDTH - 40) as _,
            24.0,
            Color::from_rgba8(0, 0, 0, 0xFF),
        )?;

        let mut width = (WIDTH - 50) as f32;
        draw_rect(
            25.0,
            (y + 4) as _,
            width,
            16.0,
            Color::from_rgba8(0x77, 0x77, 0x77, 0xFF),
        )?;
        width *= (progress as f32) / 100.0;
        draw_rect(
            25.0,
            (y + 4) as _,
            width,
            16.0,
            if error {
                Color::from_rgba8(0xFF, 0, 0, 0xFF)
            } else {
                Color::from_rgba8(0, 0xFF, 0, 0xFF)
            },
        )
        .ok();

        Ok(())
    }
}

fn main() {
    // Create an event loop with block-on capability.
    let evl = EventLoopBuilder::new_block_on().build().unwrap();

    // Create a window.
    let window = Rc::new(
        WindowBuilder::new()
            .with_inner_size(LogicalSize::new(WIDTH, HEIGHT))
            .with_min_inner_size(LogicalSize::new(WIDTH, HEIGHT))
            .with_max_inner_size(LogicalSize::new(WIDTH, HEIGHT))
            .with_title("Networking Example")
            .build(&evl)
            .expect("Failed to create window"),
    );

    let mut draw = Draw::new(&evl, &window).expect("Failed to create draw context");

    // Application state.
    let state = Rc::new(RefCell::new(AppState {
        http_states: {
            let mut states = env::args()
                .skip(1)
                .map(|url| HttpState::new(url))
                .collect::<Vec<_>>();

            if states.is_empty() {
                states.extend(
                    DEFAULT_URLS
                        .iter()
                        .map(|url| HttpState::new(url.to_string())),
                );
            }

            states
        },
    }));

    // Use a channel to communicate between the future and the event loop.
    let reset = Rc::new(event_listener::Event::new());

    // The future to block on.
    let mut fut = Box::pin({
        let state = state.clone();
        let reset = reset.clone();

        async move {
            loop {
                if let Err(e) = network(&state).await {
                    eprintln!("Error: {}", e);
                    break;
                }

                // Wait to go again.
                reset.listen().await;

                // Reset the state.
                state.borrow_mut().http_states.iter_mut().for_each(|http| {
                    *http = HttpState::new(http.url.clone());
                });
            }
        }
    });

    // Run the event loop.
    evl.block_on(
        {
            let window = window.clone();

            move |event, target| {
                target.set_control_flow(ControlFlow::Wait);

                match event {
                    Event::WindowEvent { event, window_id } if window_id == window.id() => {
                        match event {
                            WindowEvent::CloseRequested => target.exit(),
                            WindowEvent::KeyboardInput {
                                event:
                                    KeyEvent {
                                        physical_key: PhysicalKey::Code(KeyCode::KeyR),
                                        ..
                                    },
                                ..
                            } => {
                                // Reset.
                                reset.notify(1);
                            }
                            WindowEvent::RedrawRequested => {
                                draw.render(&state.borrow()).expect("failed to render");
                            }
                            _ => {}
                        }
                    }
                    Event::UserEvent(Rerender) => {
                        window.request_redraw();
                    }
                    _ => {}
                }
            }
        },
        future::poll_fn(move |cx| {
            // Poll the future once.
            let _ = fut.as_mut().poll(cx);

            // Wake up the loop to redraw.
            window.request_redraw();

            Poll::Pending
        }),
    )
    .unwrap();
}

enum Connection {
    NoTls(Async<TcpStream>),
    Tls(TlsStream<Async<TcpStream>>),
}

impl AsyncRead for Connection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut AsyncContext<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Connection::NoTls(stream) => Pin::new(stream).poll_read(cx, buf),
            Connection::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }

    fn poll_read_vectored(
        self: Pin<&mut Self>,
        cx: &mut AsyncContext<'_>,
        bufs: &mut [std::io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Connection::NoTls(stream) => Pin::new(stream).poll_read_vectored(cx, bufs),
            Connection::Tls(stream) => Pin::new(stream).poll_read_vectored(cx, bufs),
        }
    }
}

impl AsyncWrite for Connection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut AsyncContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Connection::NoTls(stream) => Pin::new(stream).poll_write(cx, buf),
            Connection::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut AsyncContext<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Connection::NoTls(stream) => Pin::new(stream).poll_flush(cx),
            Connection::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut AsyncContext<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Connection::NoTls(stream) => Pin::new(stream).poll_close(cx),
            Connection::Tls(stream) => Pin::new(stream).poll_close(cx),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut AsyncContext<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Connection::NoTls(stream) => Pin::new(stream).poll_write_vectored(cx, bufs),
            Connection::Tls(stream) => Pin::new(stream).poll_write_vectored(cx, bufs),
        }
    }
}
