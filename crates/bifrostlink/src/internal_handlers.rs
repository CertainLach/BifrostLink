use serde::{Deserialize, Serialize};

use crate::{
	notification, packet::RequestId, route::{MinRttUpdated, Rtt, Via}, AddressT
};

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct AddForwarded<Address> {
	pub(crate) to: Address,
	pub(crate) rtt: Rtt,
}
notification!((0x0001) AddForwarded<Address: AddressT>);

#[derive(Serialize, Deserialize)]
pub(crate) struct RemoveForwarded<Address> {
	pub(crate) to: Address,
}
notification!((0x0002) RemoveForwarded<Address: AddressT>);

#[derive(Serialize, Deserialize)]
pub struct UpdatedForwardedRtt<Address> {
	pub(crate) to: Address,
	pub(crate) rtt: Rtt,
}
notification!((0x0003) UpdatedForwardedRtt<Address: AddressT>);

impl<Address> MinRttUpdated<Address>
where
	Address: Clone + PartialEq,
{
	pub fn update_for(&self, address: Address) -> Option<UpdatedForwardedRtt<Address>> {
		let via_is_this = Via::Address(address) == self.rtt.via;
		let rtt = if !via_is_this {
			self.rtt.rtt
		} else if let Some(rtt) = self.rtt.second_best {
			rtt
		} else {
			return None;
		};
		let is_updated = if via_is_this {
			self.second_changed
		} else {
			self.first_changed
		};
		if !is_updated {
			return None;
		}
		Some(UpdatedForwardedRtt {
			to: self.for_address.clone(),
			rtt,
		})
	}
}

#[derive(Serialize, Deserialize)]
pub struct CancelRequest {
	pub(crate) rid: RequestId,
}
notification!((0x0004) CancelRequest);
