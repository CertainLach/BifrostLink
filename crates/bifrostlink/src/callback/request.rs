use async_trait::async_trait;

use crate::packet::{OutgoingMessage, RequestId};
use crate::Config;

#[async_trait]
pub(crate) trait RequestHandler<C: Config>: Sync + 'static + Send {
	async fn handle(
		&self,
		packet_source: C::Address,
		request: C::EncodedData,
		rid: &RequestId,
		respond_to: C::Address,
	) -> OutgoingMessage<C::Address>;
	fn cancel_safe(&self) -> bool;
}
