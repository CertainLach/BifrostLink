pub trait Request: Send + Sync + 'static {
	type Response;
	fn name() -> &'static str;
}
#[macro_export]
macro_rules! request {
	($name:ident => $response:ty) => {
		impl $crate::Request for $name {
			type Response = $response;
			fn name() -> &'static str {
				stringify!($name)
			}
		}
	};
}

pub trait IncomingRequest: Request {}
pub trait OutgoingRequest: Request {}
impl<T> OutgoingRequest for T where T: Request {}

#[derive(PartialEq, Eq, Hash, Debug)]
pub(crate) struct ResponseId(pub String);
