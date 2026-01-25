#![feature(try_blocks)]

mod port;
use std::{fmt, hash::Hash};

use bytes::Bytes;
pub use port::{native_messaging_port, start_native_messaging_extension, Port};
mod util;
use serde::{de::DeserializeOwned, Serialize};
mod connection;
mod qos;
mod route;
pub use route::Rtt;

mod event;
mod packet;

mod notification;
pub use notification::{IncomingNotification, Notification, OutgoingNotification};
mod request;
pub use request::{IncomingRequest, OutgoingRequest, Request};

mod declarative;

mod internal_handlers;

pub(crate) mod callback;

// pub(crate) mod polling;
// pub use polling::request::PollingRequest;

mod rpc;
pub use rpc::{Rpc, WeakRpc};

pub mod error;

// pub use polling::notification::PollingNotification;

use self::error::ErrorT;
use self::packet::{OpaquePacketWrapper, OutgoingMessage, RequestId, ResponseTo};

pub trait AddressT:
	Clone + Serialize + DeserializeOwned + Hash + Eq + fmt::Debug + Send + Sync + 'static
{
}

pub trait Config: 'static {
	type Address: AddressT;
	type Error: ErrorT;

	// It is transferred by channels, but never stored in shared stucts.
	type EncodedData: Send;

	fn decode_headers(
		data_with_headers: Bytes,
	) -> Result<(OpaquePacketWrapper<Self::Address>, Self::EncodedData), Self::Error>;

	fn decode_data<T>(data: Self::EncodedData) -> Result<T, Self::Error>;

	fn encode_data<T>(headers: OpaquePacketWrapper<Self::Address>, data: T) -> Bytes;
}

pub trait ConfigExt: Config {
	fn encode_outgoing<T>(
		headers: OpaquePacketWrapper<Self::Address>,
		value: T,
	) -> OutgoingMessage<Self::Address> {
		let to = match &headers {
			OpaquePacketWrapper::Response { request_origin, .. } => request_origin,
			OpaquePacketWrapper::Request { receiver, .. } => receiver,
		};
		OutgoingMessage {
			to: to.clone(),
			message: Self::encode_data(headers, value),
		}
	}
	fn encode_notification<T: OutgoingNotification>(
		sender: Self::Address,
		receiver: Self::Address,
		notification: T,
	) -> OutgoingMessage<Self::Address> {
		Self::encode_outgoing(
			OpaquePacketWrapper::Request {
				sender,
				receiver,
				request: T::name().to_owned(),
				response: None,
			},
			notification,
		)
	}
	fn encode_error_response<E: fmt::Display>(
		rid: RequestId,
		me: Self::Address,
		request_origin: Self::Address,
		error: E,
	) -> OutgoingMessage<Self::Address> {
		Self::encode_outgoing(
			OpaquePacketWrapper::Response {
				rid,
				response_from: me,
				request_origin,
				error: Some(error.to_string()),
			},
			(),
		)
	}
	fn encode_response<T: Serialize>(
		rid: RequestId,
		me: Self::Address,
		request_origin: Self::Address,
		data: T,
	) -> OutgoingMessage<Self::Address> {
		Self::encode_outgoing(
			OpaquePacketWrapper::Response {
				rid,
				response_from: me,
				request_origin,
				error: None,
			},
			data,
		)
	}
	fn encode_request<T: OutgoingRequest>(
		sender: Self::Address,
		receiver: Self::Address,
		rid: RequestId,
		data: T,
	) -> OutgoingMessage<Self::Address> {
		Self::encode_outgoing(
			OpaquePacketWrapper::Request {
				sender,
				receiver,
				request: T::name().to_owned(),
				response: Some(ResponseTo { rid }),
			},
			data,
		)
	}
}
impl<C> ConfigExt for C where C: Config {}
