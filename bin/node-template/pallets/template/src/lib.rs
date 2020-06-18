#![cfg_attr(not(feature = "std"), no_std)]

use frame_support::{debug, decl_module, decl_storage, decl_event, decl_error, dispatch::Vec};
use frame_system::{self as system, ensure_signed};
use sp_core::offchain::{Duration, IpfsRequest, IpfsResponse, OpaqueMultiaddr, Timestamp};
use sp_io::offchain::timestamp;
use sp_runtime::offchain::ipfs;
use sp_std::str;

// #[cfg(test)]
// mod tests;

// an IPFS bootstrapper node that can be connected to
#[allow(unused)]
const BOOTSTRAPPER_ADDR: &str = "/ip4/104.131.131.82/tcp/4001";

/// The pallet's configuration trait.
pub trait Trait: system::Trait {
    /// The overarching event type.
    type Event: From<Event<Self>> + Into<<Self as system::Trait>::Event>;
}

// This pallet's storage items.
decl_storage! {
    trait Store for Module<T: Trait> as TemplateModule {
        // A list of desired addresses to keep connections to
        pub DesiredConnections: Vec<OpaqueMultiaddr>;
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
        CantCreateRequest,
        RequestTimeout,
        RequestFailed,
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

        /// Add an address to the list of desired connections. The connection will be established
        /// during the next run of the off-chain `connection_housekeeping` process. If it cannot be
        /// established, it will be re-attempted during the subsequest housekeeping runs until it
        /// succeeds.
        #[weight = 100_000]
        pub fn add_address(origin, addr: Vec<u8>) {
            let who = ensure_signed(origin)?;
            let addr = OpaqueMultiaddr(addr);

            DesiredConnections::mutate(|addrs| if !addrs.contains(&addr) { addrs.push(addr) });
            Self::deposit_event(RawEvent::AddressAdded(who));
        }

        /// Remove an address from the list of desired connections. The connection will be severed
        /// during the next run of the off-chain `connection_housekeeping` process.
        #[weight = 500_000]
        pub fn remove_address(origin, addr: Vec<u8>) {
            let who = ensure_signed(origin)?;
            let addr = OpaqueMultiaddr(addr);

            DesiredConnections::mutate(|addrs| if let Some(idx) = addrs.iter().position(|e| *e == addr) { addrs.remove(idx); });
            Self::deposit_event(RawEvent::AddressRemoved(who));
        }

        fn offchain_worker(block_number: T::BlockNumber) {
            // run every other block
            if block_number % 2.into() == 1.into() {
                if let Err(e) = Self::connection_housekeeping() {
                    debug::error!("Encountered an error during IPFS connection housekeeping: {:?}", e);
                }
            }
        }
    }
}

impl<T: Trait> Module<T> {
    // send a request to the local IPFS node; can only be called be an off-chain worker
    fn ipfs_request(req: IpfsRequest, deadline: impl Into<Option<Timestamp>>) -> Result<IpfsResponse, Error<T>> {
        let ipfs_request = ipfs::PendingRequest::new(req).map_err(|_| Error::<T>::CantCreateRequest)?;
        ipfs_request.try_wait(deadline)
            .map_err(|_| Error::<T>::RequestTimeout)?
            .map(|r| r.response)
            .map_err(|e| {
                if let ipfs::Error::IoError(err) = e {
                    debug::error!("IPFS request failed: {}", str::from_utf8(&err).unwrap());
                } else {
                    debug::error!("IPFS request failed: {:?}", e);
                }
                Error::<T>::RequestFailed
            })
    }

    fn connection_housekeeping() -> Result<(), Error<T>> {
        let mut deadline;

        // obtain the node's current list of connected peers
        deadline = Some(timestamp().add(Duration::from_millis(1_000)));
        let current_ipfs_peers = if let IpfsResponse::Peers(peers) = Self::ipfs_request(IpfsRequest::Peers, deadline)? {
            peers
        } else {
            unreachable!("can't get any other response from that request; qed");
        };

        // get the list of desired connections
        let wanted_addresses = DesiredConnections::get();

        // connect to the desired peers if not yet connected
        for addr in &wanted_addresses {
            deadline = Some(timestamp().add(Duration::from_millis(1_000)));

            if !current_ipfs_peers.contains(addr) {
                let _ = Self::ipfs_request(IpfsRequest::Connect(addr.clone()), deadline);
            }
        }

        // disconnect from peers that are no longer desired
        for addr in &current_ipfs_peers {
            deadline = Some(timestamp().add(Duration::from_millis(1_000)));

            if !wanted_addresses.contains(&addr) {
                let _ = Self::ipfs_request(IpfsRequest::Disconnect(addr.clone()), deadline);
            }
        }

        if wanted_addresses.len() == current_ipfs_peers.len() {
            debug::info!(
                "All desired IPFS connections are live; current peers: {:?}",
                current_ipfs_peers.iter().filter_map(|addr| str::from_utf8(&addr.0).ok()).collect::<Vec<&str>>()
            );
        } else {
            let missing_peers = wanted_addresses
                .iter()
                .filter(|addr| !current_ipfs_peers.contains(&addr))
                .filter_map(|addr| str::from_utf8(&addr.0).ok())
                .collect::<Vec<_>>();

            debug::info!(
                "Not all desired IPFS connections are live; current peers: {:?}, missing peers: {:?}",
                current_ipfs_peers.iter().filter_map(|addr| str::from_utf8(&addr.0).ok()).collect::<Vec<&str>>(),
                missing_peers
            );
        }

        Ok(())
    }
}
