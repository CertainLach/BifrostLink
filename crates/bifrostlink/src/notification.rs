use serde::{de::DeserializeOwned, Serialize};

pub trait Notification {
	fn name() -> &'static str;
}
#[macro_export]
macro_rules! notification {
	($name:ident $(<$($generic:ident $(: $bound:ident)?),+ $(,)?>)?) => {
		impl $(<$($generic $(: $bound)?),+>)? $crate::Notification for $name $(<$($generic),+>)? {
			fn name() -> &'static str {
				stringify!($name)
			}
		}
	};
}

pub trait OutgoingNotification: Notification + Serialize {}
impl<N: Notification + Serialize> OutgoingNotification for N {}
pub trait IncomingNotification: Notification + DeserializeOwned {}
impl<N: Notification + DeserializeOwned> IncomingNotification for N {}

impl<T> Notification for &T
where
	T: Notification,
{
	fn name() -> &'static str {
		T::name()
	}
}
