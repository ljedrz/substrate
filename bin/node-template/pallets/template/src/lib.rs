#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Encode, Decode};
use frame_support::{debug, decl_module, decl_storage, decl_event, decl_error, dispatch::Vec, storage::IterableStorageMap};
use frame_system::{self as system, ensure_signed};
use sp_core::offchain::{Duration, IpfsRequest, OpaqueMultiaddr, Timestamp};
use sp_runtime::offchain::ipfs;

#[cfg(test)]
mod tests;

// pub const KEY_TYPE: KeyTypeId = KeyTypeId(*b"ipfs");

const BOOTSTRAPPER_ADDR: &str = "/ip4/104.131.131.82/tcp/4001";

/// The pallet's configuration trait.
pub trait Trait: system::Trait {
    // Add other types and constants required to configure this pallet.

    /// The overarching event type.
    type Event: From<Event<Self>> + Into<<Self as system::Trait>::Event>;
}

// This pallet's storage items.
decl_storage! {
    trait Store for Module<T: Trait> as TemplateModule {
        // A map of known addresses and their availability
        Addresses get(fn addrs): map hasher(blake2_128_concat) OpaqueMultiaddr => ExternalNodeStatus;
    }
}

#[derive(Encode, Decode)]
pub enum ExternalNodeStatus {
    ToConnect,
    ToDisconnect,
}

impl Default for ExternalNodeStatus {
    fn default() -> Self {
        Self::ToConnect
    }
}

// The pallet's events
decl_event!(
    pub enum Event<T> where AccountId = <T as system::Trait>::AccountId {
        ConnectionAdded(AccountId),
        ConnectionRemoved(AccountId),
    }
);

// The pallet's errors
decl_error! {
    pub enum Error for Module<T: Trait> {
        CantRequest,
        CantConnect,
        CantDisconnect,
        CantGetMetadata,
    }
}

// The pallet's dispatchable functions.
decl_module! {
    /// The module declaration.
    pub struct Module<T: Trait> for enum Call where origin: T::Origin {
        // Initializing errors
        type Error = Error<T>;

        // Initializing events
        fn deposit_event() = default;

        #[weight = 100_000]
        pub fn add_connection(origin, addr: Vec<u8>) {
            let who = ensure_signed(origin)?;

            let addr = OpaqueMultiaddr(addr);
            Addresses::insert(addr, ExternalNodeStatus::ToConnect);
            Self::deposit_event(RawEvent::ConnectionAdded(who));
        }

        #[weight = 500_000]
        pub fn remove_connection(origin, addr: Vec<u8>) {
            let who = ensure_signed(origin)?;

            let addr = OpaqueMultiaddr(addr);
            Addresses::insert(addr, ExternalNodeStatus::ToDisconnect);
            Self::deposit_event(RawEvent::ConnectionRemoved(who));
        }

        fn offchain_worker(block_number: T::BlockNumber) {
            // print the IPFS identity and connect to the bootstrapper at first block
            if block_number == 0.into() {
                Self::ipfs_request(IpfsRequest::Identity, None).expect("IPFS node not available");
                Self::connect_to_bootstrapper().expect("IPFS bootstrapper not available");
            }

            if block_number % 2.into() == 1.into() {
                Self::connection_housekeeping();
            }
        }
    }
}

impl<T: Trait> Module<T> {
    fn connect_to_bootstrapper() -> Result<(), Error<T>> {
        let bootstrapper_addr = OpaqueMultiaddr(BOOTSTRAPPER_ADDR.as_bytes().to_vec());
        let deadline = sp_io::offchain::timestamp().add(Duration::from_millis(5_000));
        Self::ipfs_request(IpfsRequest::Connect(bootstrapper_addr), Some(deadline))
    }

    fn ipfs_request(req: IpfsRequest, deadline: impl Into<Option<Timestamp>>)
        -> Result<(), Error<T>>
    {
        let ipfs_request = ipfs::PendingRequest::new(req).map_err(|_| Error::<T>::CantRequest)?;
        debug::info!("IPFS request started: {:?}", ipfs_request);
        ipfs_request.try_wait(deadline).map(|_| ()).map_err(|_| Error::<T>::CantRequest)
    }

    fn connection_housekeeping() -> Result<(), Error<T>> {
        use self::ExternalNodeStatus::*;

        let mut conn_deadline;

        debug::info!("Commencing IPFS connection housekeeping");

        for (addr, status) in Addresses::drain() {
            conn_deadline = Some(sp_io::offchain::timestamp().add(Duration::from_millis(1_000)));

            match status {
                ToConnect => Self::ipfs_request(IpfsRequest::Connect(addr), conn_deadline),
                ToDisconnect => Self::ipfs_request(IpfsRequest::Disconnect(addr), conn_deadline),
            };
        }

        Ok(())
    }
}
