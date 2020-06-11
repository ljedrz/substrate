#![cfg_attr(not(feature = "std"), no_std)]

use frame_support::{debug, decl_module, decl_storage, decl_event, decl_error, dispatch::Vec, storage::IterableStorageMap};
use frame_system::{self as system, ensure_signed};
use sp_core::offchain::{Duration, IpfsRequest, OpaqueMultiaddr, Timestamp};
use sp_runtime::offchain::{ipfs, KeyTypeId};

#[cfg(test)]
mod tests;

pub const KEY_TYPE: KeyTypeId = KeyTypeId(*b"ipfs");

/// Based on the above `KeyTypeId` we need to generate a pallet-specific crypto type wrappers.
/// We can use from supported crypto kinds (`sr25519`, `ed25519` and `ecdsa`) and augment
/// the types with this pallet-specific identifier.
pub mod crypto {
	use super::KEY_TYPE;
	use sp_runtime::{
		app_crypto::{app_crypto, sr25519},
		traits::Verify,
	};
	use sp_core::sr25519::Signature as Sr25519Signature;
	app_crypto!(sr25519, KEY_TYPE);

	pub struct TestAuthId;
	impl frame_system::offchain::AppCrypto<<Sr25519Signature as Verify>::Signer, Sr25519Signature> for TestAuthId {
		type RuntimeAppPublic = Public;
		type GenericSignature = sp_core::sr25519::Signature;
		type GenericPublic = sp_core::sr25519::Public;
	}
}


const BOOTSTRAPPER_ADDR: &str = "/ip4/104.131.131.82/tcp/4001";

/// The pallet's configuration trait.
pub trait Trait: system::Trait {
    /// The overarching event type.
    type Event: From<Event<Self>> + Into<<Self as system::Trait>::Event>;
}

// This pallet's storage items.
decl_storage! {
    trait Store for Module<T: Trait> as TemplateModule {
        // A list of known addresses.
        Addresses: map hasher(blake2_128_concat) OpaqueMultiaddr => ();
    }
}

// The pallet's events
decl_event!(
    pub enum Event<T> where AccountId = <T as system::Trait>::AccountId {
        AddressAdded(AccountId),
        AddressRemoved(AccountId),
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
        pub fn add_address(origin, addr: Vec<u8>) {
            let who = ensure_signed(origin)?;
            let addr = OpaqueMultiaddr(addr);

            Addresses::insert(addr, ());
            Self::deposit_event(RawEvent::AddressAdded(who));
        }

        #[weight = 500_000]
        pub fn remove_address(origin, addr: Vec<u8>) {
            let who = ensure_signed(origin)?;
            let addr = OpaqueMultiaddr(addr);

            Addresses::remove(addr);
            Self::deposit_event(RawEvent::AddressRemoved(who));
        }

        fn offchain_worker(block_number: T::BlockNumber) {
            // print the IPFS identity and connect to the bootstrapper at first block
            if block_number == 0.into() {
                Self::ipfs_request(IpfsRequest::Identity, None).expect("IPFS node not available");
                Self::connect_to_bootstrapper().expect("IPFS bootstrapper not available");
            }

            if block_number % 2.into() == 1.into() {
                //Self::conn_housekeeping();
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
        debug::info!("Commencing IPFS connection housekeeping");

        // let current_ipfs_peers = ipfs_request(IpfsRequest::Peers);

        let mut deadline;
        for (addr, _) in <Addresses>::drain() {
            deadline = Some(sp_io::offchain::timestamp().add(Duration::from_millis(1_000)));

            Self::ipfs_request(IpfsRequest::Connect(addr), deadline);
        }

        Ok(())
    }
}
