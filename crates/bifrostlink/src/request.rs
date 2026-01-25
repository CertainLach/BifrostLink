pub trait Request: Send + Sync + 'static {
	type Response;
	fn name() -> u16;
}
#[macro_export]
macro_rules! request {
	(($id:literal) $name:ident => $response:ty) => {
		impl $crate::Request for $name {
			type Response = $response;
			fn name() -> u16 {
				$id
			}
		}
	};
}

pub trait IncomingRequest: Request {}
impl<T> IncomingRequest for T where T: Request {}
pub trait OutgoingRequest: Request {}
impl<T> OutgoingRequest for T where T: Request {}

#[derive(PartialEq, Eq, Hash, Debug)]
pub(crate) struct ResponseId(pub String);
