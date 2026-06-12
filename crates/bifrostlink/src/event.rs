use crate::{
	connection::{ConnectionEnding, ConnectionMessage},
	packet::OutgoingMessage,
	resource::RegisterResources,
	route::{
		ConnectionAdded, ConnectionRemoved, MinRttUpdated, ViaListSeconded, ViaListUnseconded,
	},
};

pub enum RootEvent<Address> {
	ConnectionMessage(ConnectionMessage<Address>),
	ConnectionEnding(ConnectionEnding<Address>),

	OutgoingMessage(OutgoingMessage<Address>),

	RegisterResources(RegisterResources<Address>),

	MinRttUpdated(MinRttUpdated<Address>),
	ViaListSeconded(ViaListSeconded<Address>),
	ViaListUnseconded(ViaListUnseconded<Address>),
	ConnectionAdded(ConnectionAdded<Address>),
	ConnectionRemoved(ConnectionRemoved<Address>),
}
macro_rules! fr {
    ($($ident:ident),+) => {$(
        impl<Address> From<$ident<Address>> for RootEvent<Address> {
            fn from(v: $ident<Address>) -> Self {
                Self::$ident(v)
            }
        }
    )+};
}

fr!(
	ConnectionMessage,
	ConnectionEnding,
	OutgoingMessage,
	RegisterResources,
	MinRttUpdated,
	ViaListSeconded,
	ViaListUnseconded,
	ConnectionAdded,
	ConnectionRemoved
);
