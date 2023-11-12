// SPDX-License-Identifier: BSL-1.0 OR Apache-2.0
//               Copyright John Nunley, 2023.
// Distributed under the Boost Software License, Version 1.0 or the Apache
//                 License, Version 2.0.
//       (See accompanying file LICENSE or copy at
//         https://www.boost.org/LICENSE_1_0.txt)

#![cfg(any(
    target_os = "windows",
    target_os = "macos",
    target_os = "android",
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
))]

use super::{make_proxy_waker, Inner, Signal};

use std::future::Future;
use std::task::{Context, Poll};

use winit::event::Event;
use winit::event_loop::{ControlFlow, EventLoop, EventLoopWindowTarget as Elwt};
use winit::platform::run_return::EventLoopExtRunReturn as _;

/// An extension trait for `EventLoop` that allows one to block on a non-infinite future.
pub trait EventLoopRunReturnExt {
    type User;

    /// Block on the provided future until either the loop exits or the future returns a value.
    fn block_on_return<F, Fut>(&mut self, handler: F, fut: Fut) -> BlockOnReturnResult<Fut::Output>
    where
        F: FnMut(Event<'_, Self::User>, &Elwt<Signal<Self::User>>, &mut ControlFlow),
        Fut: Future;
}

impl<T: Send + 'static> EventLoopRunReturnExt for EventLoop<Signal<T>> {
    type User = T;

    fn block_on_return<F, Fut>(
        &mut self,
        mut handler: F,
        fut: Fut,
    ) -> BlockOnReturnResult<Fut::Output>
    where
        F: FnMut(Event<'_, Self::User>, &Elwt<Signal<Self::User>>, &mut ControlFlow),
        Fut: Future,
    {
        // We need to pin the future on the heap, since the callback needs to be movable.
        let mut ready = true;
        pin_utils::pin_mut!(fut);

        // Create a waker that will wake up the event loop.
        let waker = make_proxy_waker(self);

        // The output of the future.
        let mut output = None;

        self.run_return({
            let output = &mut output;

            move |event, target, control_flow| {
                match event {
                    Event::UserEvent(Signal(Inner::Wakeup)) => {
                        // Make sure the future is ready to wake up.
                        ready = true;
                    }

                    Event::UserEvent(Signal(Inner::User(user))) => {
                        // Forward the user event to the callback.
                        handler(Event::UserEvent(user), target, control_flow);
                    }

                    event @ Event::RedrawEventsCleared | event @ Event::LoopDestroyed => {
                        // The handler may be interested in this event.
                        let event = event.map_nonuser_event().unwrap_or_else(|_| unreachable!());
                        handler(event, target, control_flow);

                        // If the future is ready to be polled, poll it.
                        if ready {
                            ready = false;

                            // Poll the future.
                            if let Poll::Ready(res) =
                                fut.as_mut().poll(&mut Context::from_waker(&waker))
                            {
                                // The future returned a value.
                                *output = Some(res);

                                // Request to exit the loop.
                                control_flow.set_exit();
                            }
                        }
                    }

                    event => {
                        // Forward the event to the inner callback.
                        let event = event.map_nonuser_event().unwrap_or_else(|_| unreachable!());
                        handler(event, target, control_flow);
                    }
                }
            }
        });

        match output {
            Some(output) => BlockOnReturnResult::Value(output),
            None => BlockOnReturnResult::Exit,
        }
    }
}

/// Either one option or another.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BlockOnReturnResult<T> {
    /// The future returned a value.
    Value(T),

    /// The loop exited.
    Exit,
}
