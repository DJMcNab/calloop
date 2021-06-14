//! An MPSC channel whose receiving end is an event source
//!
//! Create a channel using [`channel()`](channel), which returns a
//! [`Sender`] that can be cloned and sent accross threads if `T: Send`,
//! and a [`Channel`] that can be inserted into an [`EventLoop`](crate::EventLoop).
//! It will generate one event per message.
//!
//! A synchronous version of the channel is provided by [`sync_channel`], in which
//! the [`SyncSender`] will block when the channel is full.

use std::sync::mpsc;

use crate::{EventSource, Poll, PostAction, Readiness, Token};

use super::ping::{make_ping, Ping, PingSource};

/// The events generated by the channel event source
pub enum Event<T> {
    /// A message was received and is bundled here
    Msg(T),
    /// The channel was closed
    ///
    /// This means all the `Sender`s associated with this channel
    /// have been dropped, no more messages will ever be received.
    Closed,
}

/// The sender end of a channel
///
/// It can be cloned and sent accross threads (if `T` is).
pub struct Sender<T> {
    sender: mpsc::Sender<T>,
    ping: Ping,
}

#[cfg(not(tarpaulin_include))]
impl<T> Clone for Sender<T> {
    fn clone(&self) -> Sender<T> {
        Sender {
            sender: self.sender.clone(),
            ping: self.ping.clone(),
        }
    }
}

impl<T> Sender<T> {
    /// Send a message to the channel
    ///
    /// This will wake the event loop and deliver an `Event::Msg` to
    /// it containing the provided value.
    pub fn send(&self, t: T) -> Result<(), mpsc::SendError<T>> {
        self.sender.send(t).map(|()| self.ping.ping())
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // ping on drop, to notify about channel closure
        self.ping.ping();
    }
}

/// The sender end of a synchronous channel
///
/// It can be cloned and sent accross threads (if `T` is).
pub struct SyncSender<T> {
    sender: mpsc::SyncSender<T>,
    ping: Ping,
}

#[cfg(not(tarpaulin_include))]
impl<T> Clone for SyncSender<T> {
    fn clone(&self) -> SyncSender<T> {
        SyncSender {
            sender: self.sender.clone(),
            ping: self.ping.clone(),
        }
    }
}

impl<T> SyncSender<T> {
    /// Send a message to the synchronous channel
    ///
    /// This will wake the event loop and deliver an `Event::Msg` to
    /// it containing the provided value. If the channel is full, this
    /// function will block until the event loop empties it and it can
    /// deliver the message.
    ///
    /// Due to the blocking behavior, this method should not be used on the
    /// same thread as the one running the event loop, as it could cause deadlocks.
    pub fn send(&self, t: T) -> Result<(), mpsc::SendError<T>> {
        let ret = self.try_send(t);
        match ret {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(t)) => self.sender.send(t).map(|()| self.ping.ping()),
            Err(mpsc::TrySendError::Disconnected(t)) => Err(mpsc::SendError(t)),
        }
    }

    /// Send a message to the synchronous channel
    ///
    /// This will wake the event loop and deliver an `Event::Msg` to
    /// it containing the provided value. If the channel is full, this
    /// function will return an error, but the event loop will still be
    /// signaled for readiness.
    pub fn try_send(&self, t: T) -> Result<(), mpsc::TrySendError<T>> {
        let ret = self.sender.try_send(t);
        if let Ok(()) | Err(mpsc::TrySendError::Full(_)) = ret {
            self.ping.ping();
        }
        ret
    }
}

/// The receiving end of the channel
///
/// This is the event source to be inserted into your `EventLoop`.
pub struct Channel<T> {
    receiver: mpsc::Receiver<T>,
    source: PingSource,
}

// This impl is safe because the Channel is only able to move around threads
// when it is not inserted into an event loop. (Otherwise it is stuck into
// a Source<_> and the internals of calloop, which are not Send).
// At this point, the Arc<Receiver> has a count of 1, and it is obviously
// safe to Send between threads.
unsafe impl<T: Send> Send for Channel<T> {}

/// Create a new asynchronous channel
pub fn channel<T>() -> (Sender<T>, Channel<T>) {
    let (sender, receiver) = mpsc::channel();
    let (ping, source) = make_ping().expect("Failed to create a Ping.");
    (Sender { sender, ping }, Channel { receiver, source })
}

/// Create a new synchronous, bounded channel
pub fn sync_channel<T>(bound: usize) -> (SyncSender<T>, Channel<T>) {
    let (sender, receiver) = mpsc::sync_channel(bound);
    let (ping, source) = make_ping().expect("Failed to create a Ping.");
    (SyncSender { sender, ping }, Channel { receiver, source })
}

impl<T> EventSource for Channel<T> {
    type Event = Event<T>;
    type Metadata = ();
    type Ret = ();

    fn process_events<C>(
        &mut self,
        readiness: Readiness,
        token: Token,
        mut callback: C,
    ) -> std::io::Result<PostAction>
    where
        C: FnMut(Self::Event, &mut Self::Metadata) -> Self::Ret,
    {
        let receiver = &self.receiver;
        self.source
            .process_events(readiness, token, |(), &mut ()| loop {
                match receiver.try_recv() {
                    Ok(val) => callback(Event::Msg(val), &mut ()),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        callback(Event::Closed, &mut ());
                        break;
                    }
                }
            })
    }

    fn register(&mut self, poll: &mut Poll, token: Token) -> std::io::Result<()> {
        self.source.register(poll, token)
    }

    fn reregister(&mut self, poll: &mut Poll, token: Token) -> std::io::Result<()> {
        self.source.reregister(poll, token)
    }

    fn unregister(&mut self, poll: &mut Poll) -> std::io::Result<()> {
        self.source.unregister(poll)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_channel() {
        let mut event_loop = crate::EventLoop::try_new().unwrap();

        let handle = event_loop.handle();

        let (tx, rx) = channel::<()>();

        // (got_msg, got_closed)
        let mut got = (false, false);

        let _channel_token = handle
            .insert_source(rx, move |evt, &mut (), got: &mut (bool, bool)| match evt {
                Event::Msg(()) => {
                    got.0 = true;
                }
                Event::Closed => {
                    got.1 = true;
                }
            })
            .map_err(Into::<std::io::Error>::into)
            .unwrap();

        // nothing is sent, nothing is received
        event_loop
            .dispatch(Some(::std::time::Duration::from_millis(0)), &mut got)
            .unwrap();

        assert_eq!(got, (false, false));

        // a message is send
        tx.send(()).unwrap();
        event_loop
            .dispatch(Some(::std::time::Duration::from_millis(0)), &mut got)
            .unwrap();

        assert_eq!(got, (true, false));

        // the sender is dropped
        ::std::mem::drop(tx);
        event_loop
            .dispatch(Some(::std::time::Duration::from_millis(0)), &mut got)
            .unwrap();

        assert_eq!(got, (true, true));
    }

    #[test]
    fn basic_sync_channel() {
        let mut event_loop = crate::EventLoop::try_new().unwrap();

        let handle = event_loop.handle();

        let (tx, rx) = sync_channel::<()>(2);

        let mut received = (0, false);

        let _channel_token = handle
            .insert_source(
                rx,
                move |evt, &mut (), received: &mut (u32, bool)| match evt {
                    Event::Msg(()) => {
                        received.0 += 1;
                    }
                    Event::Closed => {
                        received.1 = true;
                    }
                },
            )
            .map_err(Into::<std::io::Error>::into)
            .unwrap();

        // nothing is sent, nothing is received
        event_loop
            .dispatch(Some(::std::time::Duration::from_millis(0)), &mut received)
            .unwrap();

        assert_eq!(received.0, 0);
        assert!(!received.1);

        // fill the channel
        tx.send(()).unwrap();
        tx.send(()).unwrap();
        assert!(tx.try_send(()).is_err());

        // empty it
        event_loop
            .dispatch(Some(::std::time::Duration::from_millis(0)), &mut received)
            .unwrap();

        assert_eq!(received.0, 2);
        assert!(!received.1);

        // send a final message and drop the sender
        tx.send(()).unwrap();
        std::mem::drop(tx);

        // final read of the channel
        event_loop
            .dispatch(Some(::std::time::Duration::from_millis(0)), &mut received)
            .unwrap();

        assert_eq!(received.0, 3);
        assert!(received.1);
    }
}
