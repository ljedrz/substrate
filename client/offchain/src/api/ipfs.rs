// Copyright 2019-2020 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! This module is composed of two structs: [`IpfsApi`] and [`IpfsWorker`]. Calling the [`http`]
//! function returns a pair of [`IpfsApi`] and [`IpfsWorker`] that share some state.
//!
//! The [`IpfsApi`] is (indirectly) passed to the runtime when calling an offchain worker, while
//! the [`IpfsWorker`] must be processed in the background. The [`IpfsApi`] mimics the API of the
//! IPFS-related methods available to offchain workers.
//!
//! The reason for this design is driven by the fact that IPFS requests should continue running
//! in the background even if the runtime isn't actively calling any function.

use async_std::task;
use crate::api::timestamp;
use fnv::FnvHashMap;
use futures::{prelude::*, future};
use ipfs::{Cid, Multiaddr, PublicKey};
use log::{error, info};
use sp_core::offchain::{IpfsRequest, IpfsRequestId, IpfsRequestStatus, Timestamp};
use std::{fmt, pin::Pin, task::{Context, Poll}};
use sp_utils::mpsc::{tracing_unbounded, TracingUnboundedSender, TracingUnboundedReceiver};

/// Creates a pair of [`IpfsApi`] and [`IpfsWorker`].
pub fn ipfs<I: ipfs::IpfsTypes>(ipfs_node: ipfs::Ipfs<I>) -> (IpfsApi, IpfsWorker<I>) {
    let (to_worker, from_api) = tracing_unbounded("mpsc_ocw_to_ipfs_worker");
    let (to_api, from_worker) = tracing_unbounded("mpsc_ocw_to_ipfs_api");

    let api = IpfsApi {
        to_worker,
        from_worker: from_worker.fuse(),
        // We start with a random ID for the first IPFS request, to prevent mischievous people from
        // writing runtime code with hardcoded IDs.
        next_id: IpfsRequestId(rand::random::<u16>() % 2000),
        requests: FnvHashMap::default(),
    };
/*
    let options = ipfs::IpfsOptions::<IpfsTypes>::default();

    let ipfs_node = task::block_on(async move {
        // Start daemon and initialize repo
        let (ipfs, fut) = ipfs::UninitializedIpfs::new(options).await.start().await.unwrap();
        task::spawn(fut);
        ipfs
    });
*/
    let engine = IpfsWorker {
        to_api,
        from_api,
        ipfs_node,
        requests: Vec::new(),
    };

    (api, engine)
}

/// Provides IPFS capabilities.
///
/// Since this struct is a helper for offchain workers, its API is mimicking the API provided
/// to offchain workers.
pub struct IpfsApi {
    /// Used to sends messages to the worker.
    to_worker: TracingUnboundedSender<ApiToWorker>,
    /// Used to receive messages from the worker.
    /// We use a `Fuse` in order to have an extra protection against panicking.
    from_worker: stream::Fuse<TracingUnboundedReceiver<WorkerToApi>>,
    /// Id to assign to the next IPFS request that is started.
    next_id: IpfsRequestId,
    /// List of IPFS requests in preparation or in progress.
    requests: FnvHashMap<IpfsRequestId, IpfsApiRequest>,
}

/// One active request within `IpfsApi`.
enum IpfsApiRequest {
    NotDispatched(IpfsRequest),
    Dispatched,
    Response(IpfsResponse),
    Fail(ipfs::Error),
}

impl IpfsApi {
    /// Mimics the corresponding method in the offchain API.
    pub fn request_start(&mut self, request: IpfsRequest) -> Result<IpfsRequestId, ()> {
        let id = self.next_id;
        debug_assert!(!self.requests.contains_key(&id));
        match self.next_id.0.checked_add(1) {
            Some(id) => self.next_id.0 = id,
            None => {
                error!("Overflow in offchain worker IPFS request ID assignment");
                return Err(());
            }
        };

        let _ = self.to_worker.unbounded_send(ApiToWorker::Dispatch {
            id,
            request
        }).unwrap();

        self.requests.insert(id, IpfsApiRequest::Dispatched);

        Ok(id)
    }

    /// Mimics the corresponding method in the offchain API.
    pub fn response_wait(
        &mut self,
        ids: &[IpfsRequestId],
        deadline: Option<Timestamp>
    ) -> Vec<IpfsRequestStatus> {
        let mut deadline = timestamp::deadline_to_future(deadline);

        loop {
            {
                let mut output = Vec::with_capacity(ids.len());
                let mut must_wait_more = false;
                for id in ids {
                    output.push(match self.requests.get(id) {
                        None => IpfsRequestStatus::Invalid,
                        Some(IpfsApiRequest::NotDispatched(_)) =>
                        	unreachable!("we replaced all the NotDispatched with Dispatched earlier; qed"),
                        Some(IpfsApiRequest::Dispatched) => {
                            must_wait_more = true;
                            IpfsRequestStatus::DeadlineReached
                        },
                        Some(IpfsApiRequest::Fail(_)) => IpfsRequestStatus::IoError,
                        Some(IpfsApiRequest::Response(resp)) => {
                            info!("IPFS response: {:?}", resp);
                            IpfsRequestStatus::Finished
                        },
                    });
                }
                debug_assert_eq!(output.len(), ids.len());

                // Are we ready to call `return`?
                let is_done = if let future::MaybeDone::Done(_) = deadline {
                    true
                } else {
                    !must_wait_more
                };

                if is_done {
                    // Requests in "fail" mode are purged before returning.
                    debug_assert_eq!(output.len(), ids.len());
                    for n in (0..ids.len()).rev() {
                        if let IpfsRequestStatus::IoError = output[n] {
                            self.requests.remove(&ids[n]);
                        }
                    }
                    return output
                }
            }

            // Grab next message from the worker. We call `continue` if deadline is reached so that
            // we loop back and `return`.
            let next_message = {
                let mut next_msg = future::maybe_done(self.from_worker.next());
                futures::executor::block_on(future::select(&mut next_msg, &mut deadline));
                if let future::MaybeDone::Done(msg) = next_msg {
                    msg
                } else {
                    debug_assert!(matches!(deadline, future::MaybeDone::Done(..)));
                    continue
                }
            };

            // Update internal state based on received message.
            match next_message {
                Some(WorkerToApi::Response { id, value }) =>
                    match self.requests.remove(&id) {
                        Some(IpfsApiRequest::Dispatched) => {
                            self.requests.insert(id, IpfsApiRequest::Response(value));
                        }
                        None => {}  // can happen if we detected an IO error when sending the body
                        _ => error!("State mismatch between the API and worker"),
                    }

                Some(WorkerToApi::Fail { id, error }) =>
                    match self.requests.remove(&id) {
                        Some(IpfsApiRequest::Dispatched) => {
                            self.requests.insert(id, IpfsApiRequest::Fail(error));
                        }
                        None => {}  // can happen if we detected an IO error when sending the body
                        _ => error!("State mismatch between the API and worker"),
                    }

                None => {
                    error!("Worker has crashed");
                    return ids.iter().map(|_| IpfsRequestStatus::IoError).collect()
                }
            }
        }
    }

    pub fn process_block(&mut self) -> Result<(), ()> {
        Ok(())
    }
}

impl fmt::Debug for IpfsApi {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_list()
            .entries(self.requests.iter())
            .finish()
    }
}

impl fmt::Debug for IpfsApiRequest {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            IpfsApiRequest::NotDispatched(_) =>
                f.debug_tuple("IpfsApiRequest::NotDispatched").finish(),
            IpfsApiRequest::Dispatched =>
                f.debug_tuple("IpfsApiRequest::Dispatched").finish(),
            IpfsApiRequest::Response(_) =>
                f.debug_tuple("IpfsApiRequest::Response").finish(),
            IpfsApiRequest::Fail(err) =>
                f.debug_tuple("IpfsApiRequest::Fail").field(err).finish(),
        }
    }
}

/// Message send from the API to the worker.
enum ApiToWorker {
    /// Dispatches a new HTTP request.
    Dispatch {
        /// ID to send back when the response comes back.
        id: IpfsRequestId,
        /// Request to start executing.
        request: IpfsRequest,
    }
}

/// Message send from the API to the worker.
enum WorkerToApi {
    /// A request has succeeded.
    Response {
        /// The ID that was passed to the worker.
        id: IpfsRequestId,
        /// Status code of the response.
        value: IpfsResponse,
    },
    /// A request has failed because of an error. The request is then no longer valid.
    Fail {
        /// The ID that was passed to the worker.
        id: IpfsRequestId,
        /// Error that happened.
        error: ipfs::Error,
    },
}

/// Must be continuously polled for the [`IpfsApi`] to properly work.
pub struct IpfsWorker<I: ipfs::IpfsTypes> {
    /// Used to sends messages to the `IpfsApi`.
    to_api: TracingUnboundedSender<WorkerToApi>,
    /// Used to receive messages from the `IpfsApi`.
    from_api: TracingUnboundedReceiver<ApiToWorker>,
    /// The engine that runs IPFS requests.
    ipfs_node: ipfs::Ipfs<I>,
    /// IPFS requests that are being worked on by the engine.
    requests: Vec<(IpfsRequestId, IpfsWorkerRequest)>,
}

/// IPFS request being processed by the worker.
enum IpfsWorkerRequest {
    /// Request has been dispatched.
    Dispatched(Pin<Box<dyn Future<Output = Result<IpfsResponse, ipfs::Error>> + Send>>),
    /// Progressively reading the body of the response and sending it to the channel.
    Ready(Result<IpfsResponse, ipfs::Error>),
}

#[derive(Debug)]
pub enum IpfsResponse {
    Identity(PublicKey, Vec<Multiaddr>),
    LocalRefs(Vec<Cid>),
}

async fn ipfs_request<I: ipfs::IpfsTypes>(ipfs: ipfs::Ipfs<I>, request: IpfsRequest) -> Result<IpfsResponse, ipfs::Error> {
    match request {
        IpfsRequest::Identity => {
            let (pk, addrs) = ipfs.identity2().await?;
            Ok(IpfsResponse::Identity(pk, addrs))
        },
        IpfsRequest::LocalRefs => Ok(IpfsResponse::LocalRefs(ipfs.refs_local2().await?)),
    }
}

impl<I: ipfs::IpfsTypes> Future for IpfsWorker<I> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        // We use a `me` variable because the compiler isn't smart enough to allow borrowing
        // multiple fields at once through a `Deref`.
        let me = &mut *self;

        // We remove each element from `requests` one by one and add them back only if necessary.
        for n in (0..me.requests.len()).rev() {
            let (id, request) = me.requests.swap_remove(n);
            match request {
                IpfsWorkerRequest::Dispatched(mut future) => {
                    let response = match Future::poll(Pin::new(&mut future), cx) {
                        Poll::Pending => {
                            me.requests.push((id, IpfsWorkerRequest::Dispatched(future)));
                            continue
                        },
                        Poll::Ready(Ok(response)) => response,
                        Poll::Ready(Err(error)) => {
							let _ = me.to_api.unbounded_send(WorkerToApi::Fail { id, error });
							continue;		// don't insert the request back
						}
                    };

					let _ = me.to_api.unbounded_send(WorkerToApi::Response {
						id,
						value: response,
					});

                    //me.requests.push((id, IpfsWorkerRequest::Ready(Ok(response))));
                    cx.waker().wake_by_ref();   // reschedule in order to poll the new future
                    continue
                }

                IpfsWorkerRequest::Ready(_) => {}
            }
        }

        let ipfs_node = me.ipfs_node.clone();

        // Check for messages coming from the [`IpfsApi`].
        match Stream::poll_next(Pin::new(&mut me.from_api), cx) {
            Poll::Pending => {},
            Poll::Ready(None) => return Poll::Ready(()),    // stops the worker
            Poll::Ready(Some(ApiToWorker::Dispatch { id, request })) => {
                let future = Box::pin(ipfs_request(ipfs_node, request));
                debug_assert!(me.requests.iter().all(|(i, _)| *i != id));
                me.requests.push((id, IpfsWorkerRequest::Dispatched(future)));
                cx.waker().wake_by_ref();   // reschedule the task to poll the request
            }
        }

        Poll::Pending
    }
}

impl<I: ipfs::IpfsTypes> fmt::Debug for IpfsWorker<I> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_list()
            .entries(self.requests.iter())
            .finish()
    }
}

impl fmt::Debug for IpfsWorkerRequest {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            IpfsWorkerRequest::Dispatched(_) =>
                f.debug_tuple("IpfsWorkerRequest::Dispatched").finish(),
            IpfsWorkerRequest::Ready(_) =>
                f.debug_tuple("IpfsWorkerRequest::Response").finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use core::convert::Infallible;
    use crate::api::timestamp;
    use super::http;
    use sp_core::offchain::{IpfsError, IpfsRequestId, IpfsRequestStatus, Duration};
    use futures::future;

    // Returns an `IpfsApi` whose worker is ran in the background, and a `SocketAddr` to an HTTP
    // server that runs in the background as well.
    macro_rules! build_api_server {
        () => {{
            // We spawn quite a bit of HTTP servers here due to how async API
            // works for offchain workers, so be sure to raise the FD limit
            // (particularly useful for macOS where the default soft limit may
            // not be enough).
            fdlimit::raise_fd_limit();

            let (api, worker) = http();

            let (addr_tx, addr_rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut rt = tokio::runtime::Runtime::new().unwrap();
                let worker = rt.spawn(worker);
                let server = rt.spawn(async move {
                    let server = hyper::Server::bind(&"127.0.0.1:0".parse().unwrap())
                        .serve(hyper::service::make_service_fn(|_| { async move {
                            Ok::<_, Infallible>(hyper::service::service_fn(move |_req| async move {
                                Ok::<_, Infallible>(
                                    hyper::Response::new(hyper::Body::from("Hello World!"))
                                )
                            }))
                        }}));
                    let _ = addr_tx.send(server.local_addr());
                    server.await.map_err(drop)
                });
                let _ = rt.block_on(future::join(worker, server));
            });
            (api, addr_rx.recv().unwrap())
        }};
    }

    #[test]
    fn basic_localhost() {
        let deadline = timestamp::now().add(Duration::from_millis(10_000));

        // Performs an HTTP query to a background HTTP server.

        let (mut api, addr) = build_api_server!();

        let id = api.request_start("POST", &format!("http://{}", addr)).unwrap();
        api.request_write_body(id, &[], Some(deadline)).unwrap();

        match api.response_wait(&[id], Some(deadline))[0] {
            IpfsRequestStatus::Finished(200) => {},
            v => panic!("Connecting to localhost failed: {:?}", v)
        }

        let headers = api.response_headers(id);
        assert!(headers.iter().any(|(h, _)| h.eq_ignore_ascii_case(b"Date")));

        let mut buf = vec![0; 2048];
        let n = api.response_read_body(id, &mut buf, Some(deadline)).unwrap();
        assert_eq!(&buf[..n], b"Hello World!");
    }

    #[test]
    fn request_start_invalid_call() {
        let (mut api, addr) = build_api_server!();

        match api.request_start("\0", &format!("http://{}", addr)) {
            Err(()) => {}
            Ok(_) => panic!()
        };

        match api.request_start("GET", "http://\0localhost") {
            Err(()) => {}
            Ok(_) => panic!()
        };
    }

    #[test]
    fn request_add_header_invalid_call() {
        let (mut api, addr) = build_api_server!();

        match api.request_add_header(IpfsRequestId(0xdead), "Foo", "bar") {
            Err(()) => {}
            Ok(_) => panic!()
        };

        let id = api.request_start("GET", &format!("http://{}", addr)).unwrap();
        match api.request_add_header(id, "\0", "bar") {
            Err(()) => {}
            Ok(_) => panic!()
        };

        let id = api.request_start("POST", &format!("http://{}", addr)).unwrap();
        match api.request_add_header(id, "Foo", "\0") {
            Err(()) => {}
            Ok(_) => panic!()
        };

        let id = api.request_start("POST", &format!("http://{}", addr)).unwrap();
        api.request_add_header(id, "Foo", "Bar").unwrap();
        api.request_write_body(id, &[1, 2, 3, 4], None).unwrap();
        match api.request_add_header(id, "Foo2", "Bar") {
            Err(()) => {}
            Ok(_) => panic!()
        };

        let id = api.request_start("GET", &format!("http://{}", addr)).unwrap();
        api.response_headers(id);
        match api.request_add_header(id, "Foo2", "Bar") {
            Err(()) => {}
            Ok(_) => panic!()
        };

        let id = api.request_start("GET", &format!("http://{}", addr)).unwrap();
        api.response_read_body(id, &mut [], None).unwrap();
        match api.request_add_header(id, "Foo2", "Bar") {
            Err(()) => {}
            Ok(_) => panic!()
        };
    }

    #[test]
    fn request_write_body_invalid_call() {
        let (mut api, addr) = build_api_server!();

        match api.request_write_body(IpfsRequestId(0xdead), &[1, 2, 3], None) {
            Err(IpfsError::Invalid) => {}
            _ => panic!()
        };

        match api.request_write_body(IpfsRequestId(0xdead), &[], None) {
            Err(IpfsError::Invalid) => {}
            _ => panic!()
        };

        let id = api.request_start("POST", &format!("http://{}", addr)).unwrap();
        api.request_write_body(id, &[1, 2, 3, 4], None).unwrap();
        api.request_write_body(id, &[1, 2, 3, 4], None).unwrap();
        api.request_write_body(id, &[], None).unwrap();
        match api.request_write_body(id, &[], None) {
            Err(IpfsError::Invalid) => {}
            _ => panic!()
        };

        let id = api.request_start("POST", &format!("http://{}", addr)).unwrap();
        api.request_write_body(id, &[1, 2, 3, 4], None).unwrap();
        api.request_write_body(id, &[1, 2, 3, 4], None).unwrap();
        api.request_write_body(id, &[], None).unwrap();
        match api.request_write_body(id, &[1, 2, 3, 4], None) {
            Err(IpfsError::Invalid) => {}
            _ => panic!()
        };

        let id = api.request_start("POST", &format!("http://{}", addr)).unwrap();
        api.request_write_body(id, &[1, 2, 3, 4], None).unwrap();
        api.response_wait(&[id], None);
        match api.request_write_body(id, &[], None) {
            Err(IpfsError::Invalid) => {}
            _ => panic!()
        };

        let id = api.request_start("POST", &format!("http://{}", addr)).unwrap();
        api.request_write_body(id, &[1, 2, 3, 4], None).unwrap();
        api.response_wait(&[id], None);
        match api.request_write_body(id, &[1, 2, 3, 4], None) {
            Err(IpfsError::Invalid) => {}
            _ => panic!()
        };

        let id = api.request_start("POST", &format!("http://{}", addr)).unwrap();
        api.response_headers(id);
        match api.request_write_body(id, &[1, 2, 3, 4], None) {
            Err(IpfsError::Invalid) => {}
            _ => panic!()
        };

        let id = api.request_start("GET", &format!("http://{}", addr)).unwrap();
        api.response_headers(id);
        match api.request_write_body(id, &[], None) {
            Err(IpfsError::Invalid) => {}
            _ => panic!()
        };

        let id = api.request_start("POST", &format!("http://{}", addr)).unwrap();
        api.response_read_body(id, &mut [], None).unwrap();
        match api.request_write_body(id, &[1, 2, 3, 4], None) {
            Err(IpfsError::Invalid) => {}
            _ => panic!()
        };

        let id = api.request_start("POST", &format!("http://{}", addr)).unwrap();
        api.response_read_body(id, &mut [], None).unwrap();
        match api.request_write_body(id, &[], None) {
            Err(IpfsError::Invalid) => {}
            _ => panic!()
        };
    }

    #[test]
    fn response_headers_invalid_call() {
        let (mut api, addr) = build_api_server!();
        assert_eq!(api.response_headers(IpfsRequestId(0xdead)), &[]);

        let id = api.request_start("POST", &format!("http://{}", addr)).unwrap();
        assert_eq!(api.response_headers(id), &[]);

        let id = api.request_start("POST", &format!("http://{}", addr)).unwrap();
        api.request_write_body(id, &[], None).unwrap();
        while api.response_headers(id).is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        let id = api.request_start("GET", &format!("http://{}", addr)).unwrap();
        api.response_wait(&[id], None);
        assert_ne!(api.response_headers(id), &[]);

        let id = api.request_start("GET", &format!("http://{}", addr)).unwrap();
        let mut buf = [0; 128];
        while api.response_read_body(id, &mut buf, None).unwrap() != 0 {}
        assert_eq!(api.response_headers(id), &[]);
    }

    #[test]
    fn response_header_invalid_call() {
        let (mut api, addr) = build_api_server!();

        let id = api.request_start("POST", &format!("http://{}", addr)).unwrap();
        assert_eq!(api.response_headers(id), &[]);

        let id = api.request_start("POST", &format!("http://{}", addr)).unwrap();
        api.request_add_header(id, "Foo", "Bar").unwrap();
        assert_eq!(api.response_headers(id), &[]);

        let id = api.request_start("GET", &format!("http://{}", addr)).unwrap();
        api.request_add_header(id, "Foo", "Bar").unwrap();
        api.request_write_body(id, &[], None).unwrap();
        // Note: this test actually sends out the request, and is supposed to test a situation
        // where we haven't received any response yet. This test can theoretically fail if the
        // HTTP response comes back faster than the kernel schedules our thread, but that is highly
        // unlikely.
        assert_eq!(api.response_headers(id), &[]);
    }

    #[test]
    fn response_read_body_invalid_call() {
        let (mut api, addr) = build_api_server!();
        let mut buf = [0; 512];

        match api.response_read_body(IpfsRequestId(0xdead), &mut buf, None) {
            Err(IpfsError::Invalid) => {}
            _ => panic!()
        }

        let id = api.request_start("GET", &format!("http://{}", addr)).unwrap();
        while api.response_read_body(id, &mut buf, None).unwrap() != 0 {}
        match api.response_read_body(id, &mut buf, None) {
            Err(IpfsError::Invalid) => {}
            _ => panic!()
        }
    }

    #[test]
    fn fuzzing() {
        // Uses the API in random ways to try to trigger panics.
        // Doesn't test some paths, such as waiting for multiple requests. Also doesn't test what
        // happens if the server force-closes our socket.

        let (mut api, addr) = build_api_server!();

        for _ in 0..50 {
            let id = api.request_start("POST", &format!("http://{}", addr)).unwrap();

            for _ in 0..250 {
                match rand::random::<u8>() % 6 {
                    0 => { let _ = api.request_add_header(id, "Foo", "Bar"); }
                    1 => { let _ = api.request_write_body(id, &[1, 2, 3, 4], None); }
                    2 => { let _ = api.request_write_body(id, &[], None); }
                    3 => { let _ = api.response_wait(&[id], None); }
                    4 => { let _ = api.response_headers(id); }
                    5 => {
                        let mut buf = [0; 512];
                        let _ = api.response_read_body(id, &mut buf, None);
                    }
                    6 ..= 255 => unreachable!()
                }
            }
        }
    }
}
