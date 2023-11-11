// SPDX-License-Identifier: BSL-1.0 OR Apache-2.0
//               Copyright John Nunley, 2023.
// Distributed under the Boost Software License, Version 1.0 or the Apache
//                 License, Version 2.0.
//       (See accompanying file LICENSE or copy at
//         https://www.boost.org/LICENSE_1_0.txt)

//! A simple wrapper around `winit` that allows one to block on a future using the `winit` event loop.
//!
//! `winit` does not support `async` programming by default. This crate provides a small workaround
//! that allows one to block on a future using the `winit` event loop.
//!
//! ## Examples
//!
//! ```no_run
//! use winit::event::{Event, WindowEvent};
//! use winit::event_loop::{ControlFlow, EventLoopBuilder};
//! use winit::window::WindowBuilder;
//! use winit_block_on::prelude::*;
//!
//! use std::future::pending;
//! use std::time::Duration;
//!
//! // Create an event loop.
//! let event_loop = EventLoopBuilder::new_block_on().build().unwrap_or_else(|_| {
//!     panic!("loop creation failed")
//! });
//!
//! // Create a window inside the event loop.
//! let window = WindowBuilder::new().build(&event_loop).unwrap();
//!
//! // Create a proxy that can be used to send events to the event loop.
//! let proxy = event_loop.create_proxy();
//!
//! // Block on the future indefinitely.
//! event_loop.block_on(
//!     move |event, target| {
//!         match event {
//!             Event::UserEvent(()) => target.exit(),
//!             Event::WindowEvent {
//!                 event: WindowEvent::CloseRequested,
//!                 window_id
//!             } if window_id == window.id() => target.exit(),
//!             _ => {}
//!         }
//!     },
//!     async move {
//!         // Wait for one second.
//!         async_io::Timer::after(Duration::from_secs(1)).await;
//!
//!         // Tell the event loop to close.
//!         proxy.send_event(winit_block_on::Signal::from(())).unwrap();
//!
//!         // Wait forever for the system to close.
//!         std::future::pending::<core::convert::Infallible>().await
//!     }
//! );
//! ```
//!
//! This is a contrived example, since `control_flow.set_wait_deadline()` can do the same thing. See
//! the `networking` example for a more complex and in-depth example of combining `async` with `winit`.
//!
//! ## Limitations
//!
//! In both cases, the user event `T` needs to be `Send` and `'static`. This is because the event loop
//! proxy needs to be put into a `Waker`. In addition, if you are not using `run_return`, the future
//! needs to be `'static`. Both of these limitations could be removed if `block_on` were integrated
//! directly into `winit`.

#![forbid(unsafe_code)]

mod run_return;
pub use run_return::*;

use std::convert::Infallible;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::task::{Context, Wake, Waker};

use winit::error::EventLoopError;
use winit::event::Event;
use winit::event_loop::{
    EventLoop, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget as Elwt,
};

/// Import all relevant traits for this crate.
pub mod prelude {
    pub use super::{EventLoopBuilderExt, EventLoopExt};
}

/// An extension trait for `EventLoop` that allows one to block on a future.
pub trait EventLoopExt {
    type User;

    /// Block on the provided future indefinitely.
    fn block_on<F, Fut>(self, handler: F, fut: Fut) -> Result<(), EventLoopError>
    where
        F: FnMut(Event<Self::User>, &Elwt<Signal<Self::User>>) + 'static,
        Fut: Future<Output = Infallible> + 'static;
}

impl<T: Send + 'static> EventLoopExt for EventLoop<Signal<T>> {
    type User = T;

    fn block_on<F, Fut>(self, mut handler: F, fut: Fut) -> Result<(), EventLoopError>
    where
        F: FnMut(Event<Self::User>, &Elwt<Signal<Self::User>>) + 'static,
        Fut: Future<Output = Infallible> + 'static,
    {
        // We need to pin the future on the heap, since the callback needs to be movable.
        let mut fut = Box::pin(fut);
        let mut ready = true;

        // Create a waker that will wake up the event loop.
        let waker = make_proxy_waker(&self);

        self.run(move |event, target| {
            // If we got a wakeup signal, process it.
            match event {
                Event::UserEvent(Signal(Inner::Wakeup)) => {
                    // Make sure the future is ready to wake up.
                    ready = true;
                }

                Event::UserEvent(Signal(Inner::User(user))) => {
                    // Forward the user event to the inner callback.
                    let event = Event::UserEvent(user);
                    handler(event, target);
                }

                Event::AboutToWait => {
                    // The handler may be interested in this event.
                    handler(Event::AboutToWait, target);

                    // Since we are no longer blocking any events, we can process the future.
                    if ready {
                        ready = false;

                        // Eat the unused poll warning, it should never be ready anyways.
                        let _ = fut.as_mut().poll(&mut Context::from_waker(&waker));
                    }
                }

                event => {
                    // This is another type of event, so forward it to the inner callback.
                    let event: Event<T> =
                        event.map_nonuser_event().unwrap_or_else(|_| unreachable!());
                    handler(event, target);
                }
            }
        })
    }
}

fn make_proxy_waker<T: Send + 'static>(evl: &EventLoop<Signal<T>>) -> Waker {
    let proxy = evl.create_proxy();
    Waker::from(Arc::new(ProxyWaker(Mutex::new(proxy))))
}

struct ProxyWaker<T: 'static>(Mutex<EventLoopProxy<Signal<T>>>);

impl<T> Wake for ProxyWaker<T> {
    fn wake(self: Arc<Self>) {
        self.0
            .lock()
            .unwrap()
            .send_event(Signal(Inner::Wakeup))
            .ok();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0
            .lock()
            .unwrap()
            .send_event(Signal(Inner::Wakeup))
            .ok();
    }
}

/// An extension trait for `EventLoopBuilder` that allows one to construct an
/// event loop that can block on the future.
pub trait EventLoopBuilderExt {
    /// Starts building a new async event loop.
    fn new_block_on() -> Self;
}

impl<T> EventLoopBuilderExt for EventLoopBuilder<Signal<T>> {
    fn new_block_on() -> Self {
        Self::with_user_event()
    }
}

/// The signal used to notify the event loop that it should wake up.
#[derive(Debug)]
pub struct Signal<T>(Inner<T>);

impl<T> From<T> for Signal<T> {
    fn from(t: T) -> Self {
        Self(Inner::User(t))
    }
}

#[derive(Debug)]
enum Inner<T> {
    User(T),
    Wakeup,
}
