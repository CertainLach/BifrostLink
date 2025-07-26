use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::marker::PhantomData;
use std::ops::AsyncFn;
use std::sync::{Arc, RwLock, Weak};

use crate::callback::notification::NotificationHandler;
use crate::callback::request::RequestHandler;
use crate::connection::{Connection, ConnectionMessage};
use crate::error::{ErrorT, ListenerForYourRequestHasBeenDeadError, ResponseError};
use crate::event::RootEvent;
use crate::internal_handlers::{AddForwarded, CancelRequest, RemoveForwarded};
use crate::packet::{OpaquePacketWrapper, OutgoingMessage, RequestId};
// use crate::polling::notification::OpaquePollingNotification;
// use crate::polling::request::OpaquePollingRequest;
use crate::route::{RouteSet, Rtt, Via};
use crate::util::AbortOnDrop;
use crate::{
	AddressT, Config, ConfigExt, IncomingNotification, IncomingRequest, Notification,
	OutgoingNotification, OutgoingRequest, Port,
};
use async_trait::async_trait;
use futures::Future;
use serde::Serialize;

use tokio::sync::mpsc::unbounded_channel;
use tokio::sync::mpsc::UnboundedSender as Sender;
use tokio::sync::{broadcast, oneshot};
use tokio::task::AbortHandle;

pub(crate) struct RpcInner<C: Config> {
	me: C::Address,
	set: RouteSet<C::Address>,
	#[allow(dead_code)]
	abort: AbortOnDrop,
	tx: Sender<RootEvent<C::Address>>,
	connections: Vec<Connection<C::Address>>,
	request_handler: HashMap<&'static str, Arc<dyn RequestHandler<C>>>,
	// TODO: Should requester to be a second map key here?
	running_requests: HashMap<(C::Address, RequestId), AbortHandle>,

	// pub(crate) polling_request_handler:
	// 	HashMap<&'static str, Sender<OpaquePollingRequest<C::Address>>>,
	notification_handler: HashMap<&'static str, Arc<dyn NotificationHandler<C>>>,
	// pub(crate) polling_notification_handler:
	// 	HashMap<&'static str, Sender<OpaquePollingNotification<C>>>,
	connect_tx: broadcast::Sender<C::Address>,

	responses: HashMap<(C::Address, RequestId), oneshot::Sender<Result<C::EncodedData, C::Error>>>,

	last_request_id: HashMap<C::Address, usize>,
}

pub struct CancelRequestGuard<C: Config> {
	me: C::Address,
	to: C::Address,
	rid: RequestId,
	tx: tokio::sync::mpsc::UnboundedSender<RootEvent<C::Address>>,
	defused: bool,
}
impl<C: Config> CancelRequestGuard<C> {
	pub fn defuse(&mut self) {
		self.defused = true;
	}
}
impl<C: Config> Drop for CancelRequestGuard<C> {
	fn drop(&mut self) {
		if self.defused {
			return;
		};

		let _ = self.tx.send(
			C::encode_notification(
				self.me.clone(),
				self.to.clone(),
				CancelRequest {
					rid: self.rid.clone(),
				},
			)
			.into(),
		);
	}
}

impl<C: Config> RpcInner<C> {
	fn register_request_handler<R, F>(
		&mut self,
		cancellable: bool,
		handler: impl Fn(C::Address, R) -> F + Sync + Send + 'static,
	) where
		C::Error: Into<ResponseError> + ErrorT,
		R: IncomingRequest + Sync + Send + 'static,
		R::Response: Serialize,
		F: Future<Output = Result<R::Response, C::Error>> + Send + 'static,
	{
		struct CallbackRequestHandler<C: Config, R, H> {
			me: C::Address,
			handler: Box<H>,
			cancellable: bool,
			_marker: PhantomData<fn(C, R)>,
		}
		#[async_trait]
		impl<C, R, F, H> RequestHandler<C> for CallbackRequestHandler<C, R, H>
		where
			C: Config,
			R: IncomingRequest + Send + Sync + 'static,
			R::Response: Serialize,
			F: Future<Output = Result<R::Response, C::Error>> + Send + 'static,
			H: Fn(C::Address, R) -> F + Send + Sync + 'static,
			C::Address: AddressT + 'static,
			C::Error: Send + Sync + 'static,
		{
			fn cancel_safe(&self) -> bool {
				self.cancellable
			}
			async fn handle(
				&self,
				packet_source: C::Address,
				request: C::EncodedData,
				rid: &RequestId,
				respond_to: C::Address,
			) -> OutgoingMessage<C::Address> {
				let request = match C::decode_data(request) {
					Ok(v) => v,
					Err(e) => {
						return C::encode_error_response(
							rid.to_owned(),
							self.me.clone(),
							respond_to,
							format!("failed to parse request: {e}"),
						);
					}
				};
				match (self.handler)(packet_source, request).await {
					Ok(response) => {
						return C::encode_response(rid.to_owned(), self.me.clone(), respond_to, response);
					}
					Err(e) => {
						return C::encode_error_response(rid.to_owned(), self.me.clone(), respond_to, e.into().0)
					}
				}
			}
		}
		// self.request_handler
		match self.request_handler.entry(R::name()) {
			Entry::Occupied(_) => panic!("request handler is already defined"),
			Entry::Vacant(v) => v.insert(Arc::new(CallbackRequestHandler {
				handler: Box::new(handler),
				me: self.me.clone(),
				cancellable,
				_marker: PhantomData,
			})),
		};
	}
	fn register_notification_handler<
		'r,
		R: Notification + 'static,
		H: AsyncFn(C::Address, R) -> Result<(), C::Error> + Sync + Send + 'static,
	>(
		&mut self,
		handler: H,
		blocking: bool,
	) where
		for<'a> H::CallRefFuture<'a>: Send,
	{
		struct CallbackNotificationHandler<C: Config, R, H> {
			blocking: bool,
			handler: Box<H>,
			_marker: PhantomData<fn(R, C)>,
		}
		#[async_trait]
		impl<R, H: AsyncFn(C::Address, R) -> Result<(), C::Error> + Sync + Send + 'static, C>
			NotificationHandler<C> for CallbackNotificationHandler<C, R, H>
		where
			C: Config,
			R: Notification + 'static,
			for<'a> H::CallRefFuture<'a>: Send,
		{
			fn blocking(&self) -> bool {
				self.blocking
			}
			async fn handle(&self, packet_source: C::Address, request: C::EncodedData) {
				let request = match C::decode_data::<R>(request) {
					Ok(v) => v,
					Err(e) => {
						eprintln!("failed to parse notification: {e}");
						return;
					}
				};
				match (self.handler)(packet_source, request).await {
					Ok(()) => {}
					Err(err) => {
						eprintln!("failed to handle notification: {err}");
						return;
					}
				}
			}
		}
		// self.request_handler
		match self.notification_handler.entry(R::name()) {
			Entry::Occupied(_) => panic!("request handler is already defined"),
			Entry::Vacant(v) => v.insert(Arc::new(CallbackNotificationHandler::<C, _, _> {
				blocking,
				handler: Box::new(handler),
				_marker: PhantomData,
			})),
		};
	}
	fn remove_direct(&mut self, to: C::Address)
	where
		C::Address: Hash + Eq + Clone,
	{
		let Some(pos) = self.connections.iter().position(|conn| conn.address == to) else {
			return;
		};
		self.connections.remove(pos);
		self.set.on_remove_direct_connection(to);
	}
	fn forwarder_for(
		&self,
		address: C::Address,
		blacklist: &HashSet<Via<C::Address>>,
	) -> Option<&Connection<C::Address>> {
		let forwarder = self.set.forwarder_for(address.clone(), blacklist)?;
		let target = match forwarder {
			Via::Address(address) => address,
			Via::Direct => address.clone(),
		};
		self.connections
			.iter()
			.find(|connection| connection.address == target)
	}
	fn notify<T: OutgoingNotification>(&self, to: C::Address, notification: T) {
		self.tx
			.send(C::encode_notification(self.me.clone(), to, notification).into())
			.expect("not closed");
	}

	pub fn complete_response(
		&mut self,
		from: C::Address,
		id: RequestId,
		data: Result<C::EncodedData, C::Error>,
	) {
		let Some(pending) = self.responses.remove(&(from, id.clone())) else {
			eprintln!("completed already timed out request: {id:?}");
			return;
		};
		if let Err(_e) = pending.send(data) {
			eprintln!("failed to complete response");
		};
	}

	// If CancelRequestGuard is dropped, then the inflight request is
	// cancelled; Sender is removed, and remote server will receive request
	// cancellation message.
	pub fn request<T>(
		&mut self,
		to: C::Address,
		request: T,
	) -> (
		oneshot::Receiver<Result<C::EncodedData, C::Error>>,
		CancelRequestGuard<C>,
	)
	where
		T: OutgoingRequest,
	{
		let last_request_id = self.last_request_id.entry(to.clone()).or_default();
		*last_request_id += 1;
		let id = RequestId(*last_request_id);

		let (complete, pending) = oneshot::channel();
		// TODO: If request target is dead, all the responses for it should be dropped.
		self.responses.insert((to.clone(), id.clone()), complete);
		self.tx
			.send(C::encode_request(self.me.clone(), to.clone(), id.clone(), request).into())
			.expect("not closed");
		(
			pending,
			CancelRequestGuard {
				me: self.me.clone(),
				tx: self.tx.clone(),
				to,
				rid: id.clone(),
				defused: false,
			},
		)
	}
	fn respond_with_error(&mut self, rid: RequestId, to: C::Address, error: &str) {
		self.tx
			.send(C::encode_error_response(rid, self.me.clone(), to, error).into())
			.expect("not closed")
	}
	fn add_direct(&mut self, to: C::Address, port: Port, rtt: Rtt) {
		if self.connections.iter().any(|c| c.address == to) {
			eprintln!("connection is already added: {to:?}");
			return;
		}

		self.set.on_add_direct_connection(to.clone(), rtt);

		let connection = Connection::new(to.clone(), port, self.tx.clone());
		self.connections.push(connection);

		for (route, min_rtt) in self.set.list().collect::<Vec<_>>() {
			let rtt = if min_rtt.via == Via::Address(to.clone()) {
				let Some(rtt) = min_rtt.second_best else {
					continue;
				};
				rtt
			} else {
				min_rtt.rtt
			};
			self.notify(to.clone(), &AddForwarded { to: route, rtt })
		}
	}
}

async fn handle_connection_message<C: Config>(inner: Rpc<C>, input: ConnectionMessage<C::Address>) {
	let inner = inner.inner;
	let original_message = input.message.clone();
	let (opaque, data) = match C::decode_headers(input.message) {
		Ok(v) => v,
		Err(e) => {
			eprintln!("malformed incoming packet: {e}");
			return;
		}
	};
	let me = inner.read().expect("read").me.clone();
	let tx = inner.read().expect("read").tx.clone();
	match &opaque {
		OpaquePacketWrapper::Response {
			rid,
			response_from,
			request_origin,
			error,
		} => {
			if !inner.read().expect("read").set.may_be_forwarder_for(
				Via::Address(input.packet_source.clone()),
				request_origin.clone(),
			) {
				eprintln!(
					"messages from {:?} should not be forwarded through {:?}",
					request_origin, input.packet_source,
				);
				return;
			}
			if request_origin == &me {
				let mut read = inner.write().expect("read");
				read.complete_response(
					response_from.clone(),
					rid.to_owned(),
					match error {
						Some(e) => Err(ResponseError(e.to_owned()).into()),
						None => Ok(data),
					},
				);
				return;
			}
			let inner = inner.write().expect("write");
			let Some(forwarder) = inner.forwarder_for(request_origin.clone(), &HashSet::new())
			else {
				eprintln!("could not forward packet: {opaque:?}");
				return;
			};
			if forwarder.sender.send(original_message).is_err() {
				eprintln!("failed to forward");
			};
		}
		OpaquePacketWrapper::Request {
			sender,
			receiver,
			request,
			response,
		} => {
			let response = response.clone();
			if !inner
				.read()
				.expect("read")
				.set
				.may_be_forwarder_for(Via::Address(input.packet_source.clone()), sender.clone())
			{
				eprintln!(
					"messages from {:?} should not be forwarded through {:?}",
					sender, input.packet_source,
				);
				return;
			}
			if receiver == &me {
				if let Some(response) = response.clone() {
					// TODO: Handler enum?
					let (
						request_handler,
						// polling_handler,
					) = {
						let read = inner.read().expect("read");
						(
							read.request_handler.get(request.as_str()).cloned(),
							// read.polling_request_handler.get(request.as_str()).cloned(),
						)
					};
					if let Some(handler) = request_handler {
						let sender = sender.clone();
						let cancel_safe = handler.cancel_safe();
						let rid = response.rid.clone();
						let task = tokio::task::spawn({
							let inner = inner.clone();
							let rid = rid.clone();
							let sender = sender.clone();
							async move {
								let response_msg = handler
									.handle(sender.clone(), data, &response.rid, sender.clone())
									.await;
								if tx.send(response_msg.into()).is_err() {
									eprintln!("failed to send response");
								};
								inner
									.write()
									.expect("write")
									.running_requests
									.remove(&(sender, rid));
							}
						});
						if cancel_safe {
							let handle = task.abort_handle();
							inner
								.write()
								.expect("write")
								.running_requests
								.insert((sender, rid), handle);
						}
					// TODO: timeout/cancel
					}
					/*else if let Some(polling_handler) = polling_handler {
						let ptx = polling_handler.clone();
						let (rtx, rrx) = oneshot::channel();
						let message = input.message.clone();
						if let Err(SendError(poll)) = ptx.send(OpaquePollingRequest {
							from: sender.clone(),
							id: response.rid.clone(),
							request: Some(message),
							respond: Some(rtx),
						}) {
							poll.respond_err::<C::Error>(From::from(
								ListenerForYourRequestHasBeenDeadError,
							));
							return;
						};

						let response = response.clone();
						let sender = sender.clone();

						tokio::task::spawn(async move {
							let response = match rrx.await {
								Ok(v) => v,
								Err(_) => {
									if tx
										.send(
											C::encode_error_response(
												response.rid,
												sender.clone(),
												"no response for polling request".to_string(),
											)
											.into(),
										)
										.is_err()
									{
										eprintln!("failed to send response");
									};
									return;
								}
							};
							if tx.send(response.into()).is_err() {
								eprintln!("failed to send response");
							};
						});
					// TODO: timeout/cancel
					} */
					else {
						eprintln!("no handler found for {request} request");
						if tx
							.send(
								C::encode_error_response(
									response.rid,
									me,
									sender.clone(),
									format!("no handler defined for {request}"),
								)
								.into(),
							)
							.is_err()
						{
							eprintln!("failed to send response");
						};
					}
				} else {
					let (
						notification_handler,
						// polling_notification_handler,
					) = {
						let read = inner.read().expect("read");
						(
							read.notification_handler.get(request.as_str()).cloned(),
							// read.polling_notification_handler
							// 	.get(request.as_str())
							// 	.cloned(),
						)
					};
					if let Some(handler) = notification_handler {
						let sender = sender.clone();
						let is_blocking = handler.blocking();
						let task = tokio::task::spawn(async move {
							let _response = handler.handle(sender.clone(), data).await;
						});
						if is_blocking {
							if let Err(e) = task.await {
								eprintln!("blocking notification handler failed: {e:?}");
							};
						}
					// TODO: timeout/cancel
					}
					/*else if let Some(polling_handler) = polling_notification_handler {
						let ptx = polling_handler.clone();
						if ptx
							.send(OpaquePollingNotification {
								from: sender.clone(),
								request: input.message.clone(),
							})
							.is_err()
						{
							eprintln!("polling notification listener dead");
							return;
						};
					}*/
					else {
						eprintln!("no handler found for {request} notification")
					}
				}
				return;
			}
			let mut inner = inner.write().expect("write");
			let Some(forwarder) = inner.forwarder_for(receiver.clone(), &HashSet::new()) else {
				if let Some(response) = response.clone() {
					inner.respond_with_error(
						response.rid,
						sender.clone(),
						"could not forward message: no connection",
					);
				};
				eprintln!("could not forward packet: {opaque:?}");
				return;
			};
			if forwarder.sender.send(original_message.clone()).is_err() {
				eprintln!("failed to forward");
			};
		}
	}
}

pub struct WeakRpc<C: Config> {
	inner: Weak<RwLock<RpcInner<C>>>,
}
impl<C: Config> Clone for WeakRpc<C> {
	fn clone(&self) -> Self {
		Self {
			inner: self.inner.clone(),
		}
	}
}
impl<C: Config> WeakRpc<C> {
	pub fn upgrade(self) -> Option<Rpc<C>> {
		Some(Rpc {
			inner: self.inner.upgrade()?,
		})
	}
}

pub struct Rpc<C: Config> {
	pub(crate) inner: Arc<RwLock<RpcInner<C>>>,
}
impl<C: Config> Clone for Rpc<C> {
	fn clone(&self) -> Self {
		Self {
			inner: self.inner.clone(),
		}
	}
}
impl<C: Config> Rpc<C> {
	pub fn downgrade(self) -> WeakRpc<C> {
		WeakRpc {
			inner: Arc::downgrade(&self.inner),
		}
	}
}

impl<C: Config> Rpc<C>
where
	C::Error: From<serde_json::Error>,
{
	pub fn register_request_handler<
		R: IncomingRequest + Sync + Send + 'static,
		F: Future<Output = Result<R::Response, C::Error>> + Send + 'static,
	>(
		&self,
		cancellable: bool,
		handler: impl Fn(C::Address, R) -> F + Sync + Send + 'static,
	) where
		R::Response: Serialize,
	{
		let mut inner = self.inner.write().expect("write");
		inner.register_request_handler(cancellable, handler)
	}
	pub fn register_notification_handler<
		R: IncomingNotification + 'static,
		H: AsyncFn(C::Address, R) -> Result<(), C::Error> + Sync + Send + 'static,
	>(
		&self,
		handler: H,
	) where
		for<'a> H::CallRefFuture<'a>: 'static + Send,
	{
		let mut inner = self.inner.write().expect("write");
		inner.register_notification_handler(handler, false)
	}

	/// Only use for requests, which should be done on the current state of network, can't be
	/// processed in parallel, and executed very fast
	pub fn register_blocking_notification_handler<
		R: IncomingNotification + 'static,
		H: AsyncFn(C::Address, R) -> Result<(), C::Error> + Sync + Send + 'static,
	>(
		&self,
		handler: H,
	) where
		for<'a> H::CallRefFuture<'a>: Send,
	{
		let mut inner = self.inner.write().expect("write");
		inner.register_notification_handler(handler, true)
	}
	pub fn new(me: C::Address) -> Self {
		let (etx, mut erx) = unbounded_channel();
		let (connection_tx, _) = broadcast::channel(1000);
		let connection_tx2 = connection_tx.clone();
		let set = RouteSet::new(etx.clone());

		let (set_pending, get_pending) = tokio::sync::oneshot::channel();
		let join_handle = tokio::spawn(async move {
			let inner: Arc<RwLock<RpcInner<C>>> =
				get_pending.await.map_err(|_| ()).expect("get pending");
			while let Some(erx) = erx.recv().await {
				match erx {
					RootEvent::ConnectionMessage(input) => {
						handle_connection_message(
							Rpc {
								inner: inner.clone(),
							},
							input,
						)
						.await;
					}
					RootEvent::ConnectionEnding(ending) => {
						let mut inner = inner.write().expect("write");
						inner.remove_direct(ending.from)
					}

					RootEvent::OutgoingMessage(out) => {
						let inner = inner.read().expect("write");
						let Some(forwarder) = inner.forwarder_for(out.to.clone(), &HashSet::new())
						else {
							eprintln!(
								"no path found: {:?} {:?}",
								out.to.clone(),
								inner.connections
							);
							continue;
						};
						if forwarder.sender.send(out.message).is_err() {
							eprintln!("failed to forward");
							continue;
						};
					}

					RootEvent::MinRttUpdated(updated) => {
						let mut inner = inner.write().expect("write");
						let mut addresses = Vec::new();
						for connection in inner.connections.iter_mut() {
							let Some(update) = updated.update_for(connection.address.clone())
							else {
								continue;
							};
							addresses.push((connection.address.clone(), update));
						}
						for (target, update) in addresses {
							inner.notify(target, &update);
						}
					}
					RootEvent::ViaListSeconded(seconded) => {
						let mut inner = inner.write().expect("write");
						let mut addresses = Vec::new();
						for connection in inner.connections.iter_mut() {
							if seconded.initial_via != Via::Address(connection.address.clone()) {
								continue;
							}
							addresses.push(connection.address.clone());
						}
						for addr in addresses {
							inner.notify(
								addr,
								&AddForwarded {
									to: seconded.for_connection.clone(),
									rtt: seconded.rtt,
								},
							)
						}
					}
					RootEvent::ViaListUnseconded(seconded) => {
						let mut inner = inner.write().expect("write");
						let mut addresses = Vec::new();
						for connection in inner.connections.iter_mut() {
							if seconded.only_via != Via::Address(connection.address.clone()) {
								continue;
							}
							addresses.push(connection.address.clone());
						}
						for addr in addresses {
							inner.notify(
								addr,
								&RemoveForwarded {
									to: seconded.for_connection.clone(),
								},
							)
						}
					}
					RootEvent::ConnectionAdded(added) => {
						let _ = connection_tx.send(added.to.clone());
						let mut inner = inner.write().expect("write");
						let mut addresses = Vec::new();
						for connection in inner.connections.iter_mut() {
							if added.to == connection.address {
								eprint!("racy");
								continue;
							}
							if added.via == Via::Address(connection.address.clone()) {
								continue;
							}
							addresses.push(connection.address.clone());
						}
						for addr in addresses {
							inner.notify(
								addr,
								&AddForwarded {
									to: added.to.clone(),
									rtt: added.rtt,
								},
							)
						}
					}
					RootEvent::ConnectionRemoved(removed) => {
						let mut inner = inner.write().expect("write");
						let mut addressed = Vec::new();
						for connection in inner.connections.iter_mut() {
							if removed.to == connection.address {
								eprint!("racy");
								continue;
							}
							if removed.via == Via::Address(connection.address.clone()) {
								continue;
							}
							addressed.push(connection.address.clone());
						}
						for addr in addressed {
							inner.notify(
								addr,
								&RemoveForwarded {
									to: removed.to.clone(),
								},
							)
						}
					}
				}
			}
			eprintln!("rpc worker finished")
		});
		let abort = AbortOnDrop(join_handle.abort_handle());
		let inner = Arc::new(RwLock::new(RpcInner {
			me,
			set,
			connections: Vec::new(),
			abort,
			tx: etx,
			request_handler: Default::default(),
			// polling_request_handler: Default::default(),
			running_requests: HashMap::new(),
			notification_handler: Default::default(),
			// polling_notification_handler: Default::default(),
			responses: Default::default(),
			connect_tx: connection_tx2,
			last_request_id: Default::default(),
		}));
		set_pending
			.send(inner.clone())
			.map_err(|_| ())
			.expect("set_pending");
		let rpc = Self {
			inner: inner.clone(),
		};

		rpc.register_blocking_notification_handler({
			let inner = inner.clone();
			async move |source: C::Address, add: AddForwarded<C::Address>| {
				eprintln!("{source:?} added forwarded {add:?}");
				let inner = inner.clone();
				let mut inner = inner.write().expect("read");
				if !inner.connections.iter().any(|c| c.address == source) {
					eprintln!("connection is not direct: {source:?} -> {add:?}");
					return Ok(());
				}
				inner.set.inc(add.to, Via::Address(source), add.rtt);
				Ok(())
			}
		});
		rpc.register_blocking_notification_handler({
			let inner = inner.clone();
			async move |source: C::Address, cancel: CancelRequest| {
				let inner = inner.clone();
				let mut inner = inner.write().expect("write");
				if let Some(handle) = inner.running_requests.remove(&(source, cancel.rid)) {
					handle.abort();
				}

				Ok(())
			}
		});

		rpc
	}
	pub fn remove_direct(&self, to: C::Address) {
		let mut inner = self.inner.write().expect("read");
		inner.remove_direct(to);
	}
	pub fn add_direct(&self, to: C::Address, port: Port, rtt: Rtt) {
		let mut inner = self.inner.write().expect("read");
		inner.add_direct(to, port, rtt);
	}
	pub fn notify<T: OutgoingNotification>(&self, to: C::Address, notification: &T) {
		let inner = self.inner.read().expect("read");
		inner.notify(to, notification)
	}

	pub async fn request<T: OutgoingRequest>(
		&self,
		to: C::Address,
		request: T,
	) -> Result<T::Response, C::Error> {
		let (ch, _cancel_guard) = {
			let mut inner = self.inner.write().expect("read");
			inner.request(to, request)
		};
		let res = ch.await;
		match res {
			Ok(Ok(v)) => match C::decode_data(v) {
				Ok(v) => Ok(v),
				Err(e) => Err(e),
			},
			Ok(Err(e)) => Err(e),
			// Request was dropped for some reason.
			Err(_) => Err(From::from(ListenerForYourRequestHasBeenDeadError)),
		}
	}

	pub async fn wait_for_connection_to(&self, address: C::Address) -> Result<(), WaitError> {
		let mut wait = {
			let inner = self.inner.write().expect("write");
			let listener = inner.connect_tx.subscribe();
			if inner.set.has(address.clone()) {
				return Ok(());
			}
			listener
		};
		loop {
			match wait.recv().await {
				Ok(a) if a == address => return Ok(()),
				Ok(_) => {}
				Err(_) => return Err(WaitError),
			}
		}
	}
}

pub struct WaitError;
