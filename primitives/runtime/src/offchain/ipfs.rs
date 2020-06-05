//! A high-level helpers for making IPFS requests from Offchain Workers.
//!
//! `sp-io` crate exposes a low level methods to make and control IPFS requests
//! available only for Offchain Workers. Those might be hard to use
//! and usually that level of control is not really necessary.
//! This module aims to provide high-level wrappers for those APIs
//! to simplify making IPFS requests.
//!
//!
//! Example:
//! TODO

use sp_std::prelude::Vec;
#[cfg(not(feature = "std"))]
use sp_std::prelude::vec;
use sp_core::RuntimeDebug;
use sp_core::offchain::{
	Timestamp,
	IpfsRequest,
	IpfsRequestId as RequestId,
	IpfsRequestStatus as RequestStatus,
	IpfsError,
};

/// An IPFS request.
#[derive(Clone, PartialEq, Eq, RuntimeDebug)]
pub struct Request(IpfsRequest);

impl Request {
	pub fn new(request: IpfsRequest) -> Result<PendingRequest, IpfsError> {
	    let id = sp_io::offchain::ipfs_request_start(request).map_err(|_| IpfsError::IoError)?;

        Ok(PendingRequest { id })
	}
}

/// A request error
#[derive(Clone, PartialEq, Eq, RuntimeDebug)]
pub enum Error {
	/// Deadline has been reached.
	DeadlineReached,
	/// Request had timed out.
	IoError,
	/// Unknown error has been encountered.
	Unknown,
}

/// A struct representing an uncompleted http request.
#[derive(PartialEq, Eq, RuntimeDebug)]
pub struct PendingRequest {
	/// Request ID
	pub id: RequestId,
}

/// A result of waiting for a pending request.
pub type IpfsResult = Result<Response, Error>;

impl PendingRequest {
	/// Wait for the request to complete.
	///
	/// NOTE this waits for the request indefinitely.
	pub fn wait(self) -> IpfsResult {
		match self.try_wait(None) {
			Ok(res) => res,
			Err(_) => panic!("Since `None` is passed we will never get a deadline error; qed"),
		}
	}

	/// Attempts to wait for the request to finish,
	/// but will return `Err` in case the deadline is reached.
	pub fn try_wait(self, deadline: impl Into<Option<Timestamp>>) -> Result<IpfsResult, PendingRequest> {
		Self::try_wait_all(vec![self], deadline).pop().expect("One request passed, one status received; qed")
	}

	/// Wait for all provided requests.
	pub fn wait_all(requests: Vec<PendingRequest>) -> Vec<IpfsResult> {
		Self::try_wait_all(requests, None)
			.into_iter()
			.map(|r| match r {
				Ok(r) => r,
				Err(_) => panic!("Since `None` is passed we will never get a deadline error; qed"),
			})
			.collect()
	}

	/// Attempt to wait for all provided requests, but up to given deadline.
	///
	/// Requests that are complete will resolve to an `Ok` others will return a `DeadlineReached` error.
	pub fn try_wait_all(
		requests: Vec<PendingRequest>,
		deadline: impl Into<Option<Timestamp>>
	) -> Vec<Result<IpfsResult, PendingRequest>> {
		let ids = requests.iter().map(|r| r.id).collect::<Vec<_>>();
		let statuses = sp_io::offchain::ipfs_response_wait(&ids, deadline.into());

		statuses
			.into_iter()
			.zip(requests.into_iter())
			.map(|(status, req)| match status {
				RequestStatus::DeadlineReached => Err(req),
				RequestStatus::IoError => Ok(Err(Error::IoError)),
				RequestStatus::Invalid => Ok(Err(Error::Unknown)),
				RequestStatus::Finished => Ok(Ok(Response::new(req.id, 0))),
			})
			.collect()
	}
}

/// An IPFS response.
#[derive(RuntimeDebug)]
pub struct Response {
	/// Request id
	pub id: RequestId,
	/// Response status code
	pub code: u16,
}

impl Response {
	fn new(id: RequestId, code: u16) -> Self {
		Self {
			id,
			code,
		}
	}

	/// Retrieve the body of this response.
	pub fn body(&self) -> ResponseBody {
		ResponseBody::new(self.id)
	}
}

#[derive(Clone)]
pub struct ResponseBody {
	id: RequestId,
	error: Option<IpfsError>,
	deadline: Option<Timestamp>,
}

#[cfg(feature = "std")]
impl std::fmt::Debug for ResponseBody {
	fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
		fmt.debug_struct("ResponseBody")
			.field("id", &self.id)
			.field("error", &self.error)
			.field("deadline", &self.deadline)
			.finish()
	}
}

impl ResponseBody {
	fn new(id: RequestId) -> Self {
		ResponseBody {
			id,
			error: None,
			deadline: None,
		}
	}

	/// Set the deadline for reading the body.
	pub fn deadline(&mut self, deadline: impl Into<Option<Timestamp>>) {
		self.deadline = deadline.into();
		self.error = None;
	}

	/// Return an error that caused the iterator to return `None`.
	///
	/// If the error is `DeadlineReached` you can resume the iterator by setting
	/// a new deadline.
	pub fn error(&self) -> &Option<IpfsError> {
		&self.error
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use sp_io::TestExternalities;
	use sp_core::offchain::{
		OffchainExt,
		testing,
	};

	#[test]
	fn basic_metadata_request_and_response() {
		let (offchain, _state) = testing::TestOffchainExt::new();
		let mut t = TestExternalities::default();
		t.register_extension(OffchainExt::new(offchain));

		t.execute_with(|| {
			let identity_request = Request::new(IpfsRequest::Identity).unwrap();
			let identity_response = identity_request.wait().unwrap();
			let local_refs_request = Request::new(IpfsRequest::LocalRefs).unwrap();
			let local_refs_response = local_refs_request.wait().unwrap();

			assert!(matches!(identity_response, Response { .. }));
			assert!(matches!(local_refs_response, Response { .. }));
		})
	}
}
