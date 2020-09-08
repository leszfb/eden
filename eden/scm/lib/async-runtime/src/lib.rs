/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

///! Async job scheduling utilities for a blocking application
///!
///! We have a blocking application. We have async libraries. This crate provides common utilities
///! for communicating between the blocking world and the async world. It is intended to be a guide
///! so that not all developers have to get in depth understanding of Tokio in order to use async
///! functions.
///!
///! The crate sets up a common Runtime that all async tasks run on. We use a threaded scheduler
///! which enables parallelism. The async code is expected to be ran in parallel, not just
///! concurrently. As a reminder, Python has concurrency with multiple threads but no parallelism
///! because of the global interpreter lock.
///! The Runtime will get forcefully shut down when the main thread exits. Any running background
///! work will be lost at that time. This is not a hard requirement though, we can be tweak it to
///! wait for tasks to finish but requires some ceremony around the Runtime. Since we have no need
///! for that right now so that feature is skipped for now.
///!
///! TODO(T73962890): allow threads to execute futures in parallel; now they hold a lock while
///! waiting
///! TODO(T74221415): monitoring, signal handling
use futures::future::Future;
use futures::stream::{Stream, StreamExt};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use tokio::runtime::{Builder as RuntimeBuilder, Runtime};

static RUNTIME: Lazy<Mutex<Runtime>> = Lazy::new(|| {
    Mutex::new(
        RuntimeBuilder::new()
            .threaded_scheduler()
            .enable_all()
            .build()
            .expect("failed to initialize the async runtime"),
    )
});
pub static STREAM_BUFFER_SIZE: usize = 128;

/// Blocks the current thread while waiting for the computation defined by the Future `f` to
/// complete.
///
/// If the async computation panics then the panic gets propagated up. At that point the mutex
/// holding the runtime gets poisoned.
pub fn block_on_future<F: Future>(f: F) -> F::Output {
    // Should be replaced with `runtime.handle().block_on` after updating tokio, see T65261126.
    // T73962890 tracks updating this code internally. Externally the issue is tracked under at
    // https://github.com/tokio-rs/tokio/issues/2390
    let mut runtime = RUNTIME.lock();
    runtime.block_on(f)
}

/// Takes an async stream and provide its contents in the form of a regular iterator.
///
/// If processing one of the items in the stream panics then the stream stops without further
/// feedback. It wouldn't be a bad idea to propagate the issue somehow.
///
/// This implementation will poll as long as there is space in the buffer. The sync iterator will
/// be returned practically right after the function is called. Calls to `next()` on the
/// iterator will block as long as there are no items to read from the buffer and stream items are
/// in flight. Calls to `next()` return as soon as items are available to pop from the buffer.
/// `STREAM_BUFFER_SIZE` determines the default size of the buffer. If this value is inconvenient
/// then check out `RunStreamOptions` which allows tweaking the buffer size. The buffering is just
/// to streamline the async/parallel computation and manage some of the synchronization overhead.
/// Unless the stream `s` is buffered, the items in the stream will be processed one after the
/// other.
/// When you want to process items strictly one after the other without any sort of buffering, you
/// should use `block_on_future(my_stream.next())`.
/// Example.
///  1. unbuffered stream, stream_to_iter buffer size = 2.
///     - the stream has the first item polled and the result is added to the buffer
///     - the stream continues with polling the second item; while this is happening the iterator
///     that is returned may start being consumed releasing capacity in the buffer; for our example
///     let's say that the blocking thread hasn't reached that point yet and the stream fills the
///     buffer using the second item
///     - the stream will now poll the third item; assuming that the buffer is still full when the
///     computation is done, it will yield the thread that is is running on until the blocking
///     thread reads one of the items in the buffer.
///  2. buffered stream over 2 items (ordered), stream_to_iter buffer = 2
///     - the stream will take 2 futures from the underlying iterator and poll on them; when the
///     first one returns it enquees the result in our buffer and polls the third future in the
///     underlying stream. Assuming that f(x) produces r(x) we could write:
///      stream: #f1, *f2, *f3, f4, f5
///      buffer: r1
///     - let's assume that the blocking thread will not consume the buffer and the next future
///     finishes; the result then fills the buffer and f4 will get polled:
///      stream: #f1, #f2, *f3, *f4, f5
///      buffer: r1, r2
///     - adding the result of the third future to the buffer will have to wait until the blocking
///     thread consumes the returned iterator; only after that will the stream proceed with
///     polling the fifth future in the stream
pub fn stream_to_iter<S>(s: S) -> RunStream<S::Item>
where
    S: Stream + Unpin + Send + 'static,
    S::Item: Send,
{
    RunStreamOptions::new().run(s)
}

/// See `stream_to_iter`. Allows tweaking run parameters. See individual methods for parameter
/// details.
pub struct RunStreamOptions {
    buffer_size: usize,
}

impl RunStreamOptions {
    pub fn new() -> Self {
        Self {
            buffer_size: STREAM_BUFFER_SIZE,
        }
    }

    /// When dealing with heavy computation or objects a smaller buffer size may be appropriate.
    /// The current implementation does not provide a means to completely wait on polling the
    /// second item until the blocking thread reads the first value.
    pub fn buffer_size(&mut self, buffer_size: usize) -> &mut Self {
        self.buffer_size = buffer_size;
        self
    }

    /// Takes an async stream and provide it's contents in the form of a regular iterator.
    /// See `stream_to_iter`.
    pub fn run<S>(&self, mut s: S) -> RunStream<S::Item>
    where
        S: Stream + Unpin + Send + 'static,
        S::Item: Send,
    {
        // Why use a channel vs using `std::iter::from_fn`
        // It is probably a bit easier to reason about what happens when using the channel. The
        // implementation details of the executor and the buffered stream don't come in discussion
        // as when directly scheduling the next future. It's a bit of insurance against changes and
        // it separates the two worlds more clearly.  The channel approach can be optimized to
        // reduce entering the async runtime context when the stream is completed faster that it is
        // processed on the main thread. We could also add multiple consumers.
        let runtime = RUNTIME.lock();
        let (mut tx, rx) = tokio::sync::mpsc::channel(self.buffer_size);
        runtime.enter(|| {
            tokio::spawn(async move {
                while let Some(v) = s.next().await {
                    if tx.send(v).await.is_err() {
                        // receiver dropped; TODO(T74252041): add logging
                        return;
                    }
                }
            })
        });
        RunStream { rx }
    }
}

/// Blocking thread handler for receiving the results following processing a `Stream`.
pub struct RunStream<T> {
    rx: tokio::sync::mpsc::Receiver<T>,
}

impl<T> Iterator for RunStream<T> {
    type Item = T;

    /// Returns the items extracted from processing the stream. Will return `None` when the stream's
    /// end is reached or when processing an item panics.
    /// See `stream_to_iter`.
    fn next(&mut self) -> Option<Self::Item> {
        block_on_future(self.rx.recv())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::thread;

    use futures::future;
    use futures::stream;

    #[test]
    fn test_block_on_future() {
        assert_eq!(block_on_future(async { 2 + 2 }), 4);
    }

    #[test]
    #[should_panic(expected = "hello future")]
    fn test_block_on_future_will_panic() {
        block_on_future(async {
            panic!("hello future");
        });
    }

    #[test]
    fn test_panic_in_future_does_not_poisons_runtime() {
        let th = thread::spawn(|| {
            block_on_future(async {
                panic!("no poison");
            })
        });
        assert!(th.join().is_err());
        assert_eq!(block_on_future(async { 2 + 2 }), 4);
    }

    #[test]
    fn test_block_on_future_block_on_other_thread() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        thread::spawn(|| {
            block_on_future(async move {
                for i in 12.. {
                    tx.send(i).unwrap();
                }
            })
        });
        assert_eq!(
            rx.into_iter().take(5).collect::<Vec<i32>>(),
            vec![12, 13, 14, 15, 16]
        );
    }

    // TODO(T73962890): allow threads to call block_on_future in paralel
    // #[test]
    // fn test_block_on_future_pseudo_parallelism() {
    //     // This test will deadlock if threads can't schedule tasks while another thread
    //     // is waiting for a result
    //     let (mut tx, mut rx) = tokio::sync::mpsc::channel(1);
    //     let th1 = thread::spawn(|| block_on_future(async move { rx.recv().await }));
    //     let _th2 = thread::spawn(|| block_on_future(async move { tx.send(5).await }));
    //     assert_eq!(th1.join().unwrap(), Some(5));
    // }

    #[test]
    fn test_stream_to_iter() {
        let stream = stream::iter(vec![42, 33, 12]);
        let iter = stream_to_iter(stream);
        assert_eq!(iter.collect::<Vec<_>>(), vec![42, 33, 12]);
    }

    #[test]
    fn test_stream_to_iter_two_instances() {
        let mut options = RunStreamOptions::new();
        options.buffer_size(1);
        let mut iter1 = options.run(stream::iter(vec![42, 33, 12]));
        let mut iter2 = options.run(stream::iter(vec![11, 25, 67]));
        assert_eq!(iter2.next(), Some(11));
        assert_eq!(iter2.next(), Some(25));
        assert_eq!(iter1.next(), Some(42));
        assert_eq!(iter2.next(), Some(67));
        assert_eq!(iter2.next(), None);
        assert_eq!(iter1.next(), Some(33));
        assert_eq!(iter1.next(), Some(12));
        assert_eq!(iter1.next(), None);
    }

    #[test]
    fn test_stream_to_iter_some_items_panic() {
        let stream = stream::iter(vec![43, 33, 12, 11])
            .then(future::ready)
            .map(|v| {
                assert!(v & 1 == 1);
                v + 1
            });
        let iter = stream_to_iter(stream);
        assert_eq!(iter.collect::<Vec<_>>(), vec![44, 34]);
    }
}
