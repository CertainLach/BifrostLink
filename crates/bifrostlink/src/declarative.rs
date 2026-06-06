pub use bifrostlink_macros::endpoints;

use crate::{Config, Remote};

pub trait RemoteEndpoints<C: Config> {
	fn wrap(remote: Remote<C>) -> Self;
}
