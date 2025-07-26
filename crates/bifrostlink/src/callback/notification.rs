use async_trait::async_trait;

use crate::Config;

#[async_trait]
pub(crate) trait NotificationHandler<C: Config>: Sync + Send {
	/// Should input processing wait until this notification is processed?
	/// Handler might still use async/await, if it is blocking - the processing loop will await
	/// on handler, otherwise it will spawn-and-forget.
	fn blocking(&self) -> bool;
	async fn handle(&self, packet_source: C::Address, notification: C::EncodedData);
}
