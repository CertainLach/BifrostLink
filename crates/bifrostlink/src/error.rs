use std::fmt::Display;

#[derive(Debug)]
pub struct ResponseError(pub String);
#[derive(Debug)]
pub struct ListenerForYourRequestHasBeenDeadError;

pub trait ErrorT:
	Send
	+ Sync
	+ 'static
	+ Display
	+ From<ResponseError>
	+ Into<ResponseError>
	+ From<serde_json::Error>
	+ From<ListenerForYourRequestHasBeenDeadError>
{
}
