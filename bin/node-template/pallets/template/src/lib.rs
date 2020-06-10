#![cfg_attr(not(feature = "std"), no_std)]

use frame_support::{debug, decl_module, decl_storage, decl_event, decl_error, dispatch::Vec};
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
        Something get(fn something): Option<u32>;
    }
}

// The pallet's events
decl_event!(
    pub enum Event<T> where AccountId = <T as system::Trait>::AccountId {
        NewConnection(AccountId),
        DroppedConnection(AccountId),
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
        pub fn connect(origin, addr: Vec<u8>) {
            let who = ensure_signed(origin)?;

            let addr = OpaqueMultiaddr(addr);
            let deadline = sp_io::offchain::timestamp().add(Duration::from_millis(2_000));
            Self::ipfs_request(IpfsRequest::Connect(addr), Some(deadline))
                .map_err(|_| Error::<T>::CantConnect)?;
            Self::deposit_event(RawEvent::NewConnection(who));
        }

        #[weight = 500_000]
        pub fn disconnect(origin, addr: Vec<u8>) {
            let who = ensure_signed(origin)?;

            let addr = OpaqueMultiaddr(addr);
            let deadline = sp_io::offchain::timestamp().add(Duration::from_millis(2_000));
            Self::ipfs_request(IpfsRequest::Disconnect(addr), Some(deadline))
                .map_err(|_| Error::<T>::CantDisconnect)?;
            Self::deposit_event(RawEvent::DroppedConnection(who));
        }

        #[weight = 10_000]
        pub fn peers(origin) {
            ensure_signed(origin)?;

            let deadline = sp_io::offchain::timestamp().add(Duration::from_millis(2_000));
            Self::ipfs_request(IpfsRequest::Peers, Some(deadline))
                .map_err(|_| Error::<T>::CantGetMetadata)?;
        }

        fn offchain_worker(block_number: T::BlockNumber) {
            // print the IPFS identity and connect to the bootstrapper at first block
            if block_number == 0.into() {
                Self::ipfs_request(IpfsRequest::Identity, None).expect("IPFS node not available");
                Self::connect_to_bootstrapper().expect("IPFS bootstrapper not available");
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
        debug::debug!("IPFS request started: {:?}", ipfs_request);
        ipfs_request.try_wait(deadline).map(|_| ()).map_err(|_| Error::<T>::CantRequest)
    }
}
