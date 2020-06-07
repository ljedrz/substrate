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

//! This module is composed of two structs: [`IpfsApi`] and [`IpfsWorker`]. Calling the [`ipfs`]
//! function returns a pair of [`IpfsApi`] and [`IpfsWorker`] that share some state.
//!
//! The [`IpfsApi`] is (indirectly) passed to the runtime when calling an offchain worker, while
//! the [`IpfsWorker`] must be processed in the background. The [`IpfsApi`] mimics the API of the
//! IPFS-related methods available to offchain workers.
//!
//! The reason for this design is driven by the fact that IPFS requests should continue running
//! in the background even if the runtime isn't actively calling any function.

use crate::api::timestamp;
use fnv::FnvHashMap;
use futures::{prelude::*, future};
use ipfs::{unixfs, BitswapStats, Block, Cid, IpfsPath, Multiaddr, PeerId, PublicKey, SubscriptionStream};
use log::{error, info};
use sp_core::offchain::{IpfsRequest, IpfsRequestId, IpfsRequestStatus, Timestamp};
use std::{convert::TryInto, fmt, path::PathBuf, pin::Pin, str::{self, FromStr}, task::{Context, Poll}};
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

        let _ = self.to_worker.unbounded_send(ApiToWorker {
            id,
            request
        });

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
                        Some(IpfsApiRequest::Dispatched) => {
                            must_wait_more = true;
                            IpfsRequestStatus::DeadlineReached
                        },
                        Some(IpfsApiRequest::Fail(_)) => IpfsRequestStatus::IoError,
                        Some(IpfsApiRequest::Response(resp)) => {
                            info!("IPFS response: {:?}", resp); // TODO: decide how to handle responses
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
                        _ => error!("State mismatch between the API and worker"),
                    }

                Some(WorkerToApi::Fail { id, error }) =>
                    match self.requests.remove(&id) {
                        Some(IpfsApiRequest::Dispatched) => {
                            self.requests.insert(id, IpfsApiRequest::Fail(error));
                        }
                        _ => error!("State mismatch between the API and worker"),
                    }

                None => {
                    error!("Worker has crashed");
                    return ids.iter().map(|_| IpfsRequestStatus::IoError).collect()
                }
            }
        }
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
struct ApiToWorker {
    /// ID to send back when the response comes back.
    id: IpfsRequestId,
    /// Request to start executing.
    request: IpfsRequest,
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
struct IpfsWorkerRequest(
    Pin<Box<dyn Future<Output = Result<IpfsResponse, ipfs::Error>> + Send>>
);

#[derive(Debug)]
pub enum IpfsResponse {
    Identity(PublicKey, Vec<Multiaddr>),
    LocalRefs(Vec<Cid>),
    Addrs(Vec<(PeerId, Vec<Multiaddr>)>),
    LocalAddrs(Vec<Multiaddr>),
    Connect(()),
    Disconnect(()),
    Subscribe(SubscriptionStream), // TODO: actually using the SubscriptionStream would require it to be stored within the node.
    SubscriptionList(Vec<Vec<u8>>),
    Unsubscribe(bool),
    Publish(()),
    BitswapStats(BitswapStats),
    AddListeningAddr(Multiaddr),
    RemoveListeningAddr(()),
    GetBlock(Block),
    AddFile(Cid),
    GetFile(unixfs::File),
}

async fn ipfs_request<I: ipfs::IpfsTypes>(ipfs: ipfs::Ipfs<I>, request: IpfsRequest) -> Result<IpfsResponse, ipfs::Error> {
    match request {
        IpfsRequest::Identity => {
            let (pk, addrs) = ipfs.identity().await?;
            Ok(IpfsResponse::Identity(pk, addrs))
        },
        IpfsRequest::LocalRefs => Ok(IpfsResponse::LocalRefs(ipfs.refs_local().await?)),
        IpfsRequest::Connect(addr) => Ok(IpfsResponse::Connect(ipfs.connect(addr.0.try_into()?).await?)),
        IpfsRequest::Disconnect(addr) => Ok(IpfsResponse::Disconnect(ipfs.disconnect(addr.0.try_into()?).await?)),
        IpfsRequest::Addrs => Ok(IpfsResponse::Addrs(ipfs.addrs().await?)),
        IpfsRequest::LocalAddrs => Ok(IpfsResponse::LocalAddrs(ipfs.addrs_local().await?)),
        IpfsRequest::Subscribe(topic) => {
            Ok(IpfsResponse::Subscribe(ipfs.pubsub_subscribe(str::from_utf8(&topic)?).await?))
        },
        IpfsRequest::SubscriptionList => {
            Ok(IpfsResponse::SubscriptionList(ipfs.pubsub_subscribed().await?.into_iter().map(|s| s.into_bytes()).collect()))
        },
        IpfsRequest::Unsubscribe(topic) => {
            Ok(IpfsResponse::Unsubscribe(ipfs.pubsub_unsubscribe(str::from_utf8(&topic)?).await?))
        },
        IpfsRequest::Publish { topic, message } => {
            Ok(IpfsResponse::Publish(ipfs.pubsub_publish(str::from_utf8(&topic)?, &message).await?))
        },
        IpfsRequest::BitswapStats => Ok(IpfsResponse::BitswapStats(ipfs.bitswap_stats().await?)),
        IpfsRequest::AddListeningAddr(addr) => {
            Ok(IpfsResponse::AddListeningAddr(ipfs.add_listening_address(addr.0.try_into()?).await?))
        },
        IpfsRequest::RemoveListeningAddr(addr) => {
            Ok(IpfsResponse::RemoveListeningAddr(ipfs.remove_listening_address(addr.0.try_into()?).await?))
        },
        IpfsRequest::GetBlock(cid) => Ok(IpfsResponse::GetBlock(ipfs.get_block(&cid.try_into()?).await?)),
        IpfsRequest::AddFile(path) => {
            Ok(IpfsResponse::AddFile(ipfs.add(PathBuf::from_str(str::from_utf8(&path)?)?.into()).await?))
        },
        IpfsRequest::GetFile(path) => {
            Ok(IpfsResponse::GetFile(ipfs.get(IpfsPath::from_str(str::from_utf8(&path)?)?).await?))
        },
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
            let (id, mut request) = me.requests.swap_remove(n);
            let response = match Future::poll(Pin::new(&mut request.0), cx) {
                Poll::Pending => {
                    me.requests.push((id, request));
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

            cx.waker().wake_by_ref();   // reschedule in order to poll the new future
        }

        let ipfs_node = me.ipfs_node.clone();

        // Check for messages coming from the [`IpfsApi`].
        match Stream::poll_next(Pin::new(&mut me.from_api), cx) {
            Poll::Pending => {},
            Poll::Ready(None) => return Poll::Ready(()),    // stops the worker
            Poll::Ready(Some(ApiToWorker { id, request })) => {
                let future = Box::pin(ipfs_request(ipfs_node, request));
                debug_assert!(me.requests.iter().all(|(i, _)| *i != id));
                me.requests.push((id, IpfsWorkerRequest(future)));
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
        f.debug_tuple("IpfsWorkerRequest").finish()
    }
}

#[cfg(test)]
mod tests {
    use crate::api::timestamp;
    use super::ipfs;
    use async_std::task;
    use sp_core::offchain::{IpfsRequest, IpfsRequestStatus, Duration};

    fn ipfs_node() -> ipfs::Ipfs<ipfs::TestTypes> {
        let options = ipfs::IpfsOptions::<ipfs::TestTypes>::default();

        task::block_on(async move {
            let (ipfs, fut) = ipfs::UninitializedIpfs::new(options).await.start().await.unwrap();
            task::spawn(fut);
            ipfs
        })
    }

    macro_rules! build_ipfs_node {
		() => {{
            fdlimit::raise_fd_limit();

            let (api, worker) = ipfs(ipfs_node());

            std::thread::spawn(move || {
                let mut rt = tokio::runtime::Runtime::new().unwrap();
                let worker = rt.spawn(worker);
                rt.block_on(worker).unwrap();
            });

            api
		}};
	}

    #[test]
    fn metadata_calls() {
        let deadline = timestamp::now().add(Duration::from_millis(10_000));

        let mut api = build_ipfs_node!();

        let id1 = api.request_start(IpfsRequest::Identity).unwrap();
        let id2 = api.request_start(IpfsRequest::LocalRefs).unwrap();

        match api.response_wait(&[id1, id2], Some(deadline)).as_slice() {
            [IpfsRequestStatus::Finished, IpfsRequestStatus::Finished] => {},
            x => panic!("Connecting to the IPFS node failed: {:?}", x),
        }
    }
}
