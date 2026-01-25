use std::future::Future;

use bytes::Bytes;
use tokio::sync::mpsc::{
	unbounded_channel, UnboundedReceiver as Receiver, UnboundedSender as Sender,
};

use crate::util::AbortOnDrop;

/// Transport abstraction, duplex message-based stream
pub struct Port {
	pub(crate) sender: Sender<Bytes>,
	pub(crate) receiver: Receiver<Bytes>,
	pub(crate) abort_handle: AbortOnDrop,
}
impl Port {
	pub fn new<F: Future<Output = ()> + Send + 'static>(
		handle: impl FnOnce(Receiver<Bytes>, Sender<Bytes>) -> F,
	) -> Self {
		// Bounded should work just fine, due to OS stdio backpressure?
		let (sender, rx) = unbounded_channel();
		let (tx, receiver) = unbounded_channel();

		let join_handle = tokio::task::spawn(handle(rx, tx));
		let abort_handle = AbortOnDrop(join_handle.abort_handle());

		Self {
			sender,
			receiver,
			abort_handle,
		}
	}
}
