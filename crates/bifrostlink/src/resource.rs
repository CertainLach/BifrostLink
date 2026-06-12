use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::de::Error as _;
use serde::ser::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::sync::mpsc::UnboundedSender as Sender;

use crate::event::RootEvent;
use crate::internal_handlers::ResourceDropped;
use crate::{Config, ConfigExt};

pub(crate) type Cleanup = Box<dyn FnOnce() + Send + Sync>;

type MakeDrop = Box<dyn Fn(ResourceId) -> Cleanup>;

/// Unique per node
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceId(pub u64);

// TODO: Move this to Rpc somehow?.. Should not be possible to overflow here, but still doesn't seem too good.
static NEXT_RESOURCE_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
	// Set during encode, drained into resource registry afterwards
	static SER_PARKED: RefCell<Option<Vec<(ResourceId, Cleanup)>>> = const { RefCell::new(None) };
	// Set during decode, Resource::deserialize pulls drop notifier from here
	static DE_MAKE_DROP: RefCell<Option<MakeDrop>> = const { RefCell::new(None) };
}

/// A handle to a resource owned elsewhere (or here, until serialized).
///
/// Dropping runs the cleanup, which might be remote. Serializing
/// encodes the resource id so the remote machine can ask the current one to clean it up.
pub struct Resource<T> {
	payload: T,
	on_drop: Mutex<Option<Cleanup>>,
}

impl<T> Resource<T> {
	pub fn new(payload: T, cleanup: impl FnOnce() + Send + Sync + 'static) -> Self {
		Self {
			payload,
			on_drop: Mutex::new(Some(Box::new(cleanup))),
		}
	}

	pub fn get_and_leak(self) -> T {
		let mut me = ManuallyDrop::new(self);
		me.on_drop.get_mut().unwrap().take();
		// SAFETY: `me` is `ManuallyDrop`, so `payload` is never read again and
		// its destructor is not run by us.
		unsafe { std::ptr::read(&me.payload) }
	}

	pub fn get_and_drop(self) -> T {
		let mut me = ManuallyDrop::new(self);
		if let Some(f) = me.on_drop.get_mut().unwrap().take() {
			f();
		}
		// SAFETY: see `into_inner`.
		unsafe { std::ptr::read(&me.payload) }
	}
}

impl<T> Deref for Resource<T> {
	type Target = T;
	fn deref(&self) -> &T {
		&self.payload
	}
}
impl<T> DerefMut for Resource<T> {
	fn deref_mut(&mut self) -> &mut T {
		&mut self.payload
	}
}

impl<T> Drop for Resource<T> {
	fn drop(&mut self) {
		if let Some(f) = self.on_drop.get_mut().ok().and_then(|s| s.take()) {
			f();
		}
	}
}

impl<T: Serialize> Serialize for Resource<T> {
	fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
		let id = SER_PARKED.with(|p| {
			let mut p = p.borrow_mut();
			let parked = p.as_mut().ok_or_else(|| {
				S::Error::custom("Resource can only be serialized inside a bifrostlink message")
			})?;
			let cleanup = self
				.on_drop
				.lock()
				.map_err(|_| S::Error::custom("Resource cleanup lock poisoned"))?
				.take()
				.ok_or_else(|| S::Error::custom("Resource already serialized or dropped"))?;
			let id = ResourceId(NEXT_RESOURCE_ID.fetch_add(1, Ordering::Relaxed));
			parked.push((id, cleanup));
			Ok::<_, S::Error>(id)
		})?;
		// Serialized outside the borrow so a nested `Resource` in `payload` can
		// re-enter `SER_PARKED`.
		(id, &self.payload).serialize(s)
	}
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Resource<T> {
	fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
		let (id, payload) = <(ResourceId, T)>::deserialize(d)?;
		let on_drop = DE_MAKE_DROP.with(|m| {
			let m = m.borrow();
			let make = m.as_ref().ok_or_else(|| {
				D::Error::custom("Resource can only be deserialized inside a bifrostlink message")
			})?;
			Ok::<_, D::Error>(make(id))
		})?;
		Ok(Resource {
			payload,
			on_drop: Mutex::new(Some(on_drop)),
		})
	}
}

pub(crate) struct RegisterResources<Address> {
	pub(crate) holder: Address,
	pub(crate) items: Vec<(ResourceId, Cleanup)>,
}

pub(crate) fn with_encode<R>(f: impl FnOnce() -> R) -> (R, Vec<(ResourceId, Cleanup)>) {
	struct Guard;
	impl Drop for Guard {
		fn drop(&mut self) {
			SER_PARKED.with(|p| *p.borrow_mut() = None);
		}
	}
	SER_PARKED.with(|p| {
		let prev = p.borrow_mut().replace(Vec::new());
		debug_assert!(prev.is_none(), "nested with_encode");
	});
	let _g = Guard;
	let r = f();
	let parked = SER_PARKED.with(|p| p.borrow_mut().take().unwrap_or_default());
	(r, parked)
}

pub(crate) fn with_decode<R>(make: MakeDrop, f: impl FnOnce() -> R) -> R {
	struct Guard;
	impl Drop for Guard {
		fn drop(&mut self) {
			DE_MAKE_DROP.with(|m| *m.borrow_mut() = None);
		}
	}
	DE_MAKE_DROP.with(|m| *m.borrow_mut() = Some(make));
	let _g = Guard;
	f()
}

pub(crate) fn drop_notifier<C: Config>(
	me: C::Address,
	owner: C::Address,
	tx: Sender<RootEvent<C::Address>>,
) -> MakeDrop {
	Box::new(move |id| {
		let me = me.clone();
		let owner = owner.clone();
		let tx = tx.clone();
		Box::new(move || {
			let _ = tx.send(C::encode_notification(me, owner, ResourceDropped { id }).into());
		}) as Cleanup
	})
}

pub(crate) fn register_parked<Address>(
	tx: &Sender<RootEvent<Address>>,
	holder: Address,
	parked: Vec<(ResourceId, Cleanup)>,
) {
	if parked.is_empty() {
		return;
	}
	let _ = tx.send(
		RegisterResources {
			holder,
			items: parked,
		}
		.into(),
	);
}

#[cfg(test)]
mod tests {
	use std::sync::Mutex;

	use bytes::Bytes;
	use serde::{Deserialize, Serialize};
	use tokio::sync::mpsc::unbounded_channel;
	use tokio::sync::oneshot;

	use super::Resource;
	use crate::error::{ListenerForYourRequestHasBeenDeadError, ResponseError};
	use crate::packet::OpaquePacketWrapper;
	use crate::{notification, request, AddressT, Config, Port, Rpc, Rtt};

	#[derive(Debug)]
	struct TestError(String);
	impl std::fmt::Display for TestError {
		fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
			write!(f, "{}", self.0)
		}
	}
	impl From<ResponseError> for TestError {
		fn from(e: ResponseError) -> Self {
			TestError(e.0)
		}
	}
	impl From<TestError> for ResponseError {
		fn from(e: TestError) -> Self {
			ResponseError(e.0)
		}
	}
	impl From<serde_json::Error> for TestError {
		fn from(e: serde_json::Error) -> Self {
			TestError(e.to_string())
		}
	}
	impl From<ListenerForYourRequestHasBeenDeadError> for TestError {
		fn from(_: ListenerForYourRequestHasBeenDeadError) -> Self {
			TestError("listener dead".into())
		}
	}
	impl crate::error::ErrorT for TestError {}

	impl AddressT for u32 {}

	struct TestConfig;
	impl Config for TestConfig {
		type Address = u32;
		type Error = TestError;
		type EncodedData = serde_json::Value;

		fn decode_headers(
			data: Bytes,
		) -> Result<(OpaquePacketWrapper<u32>, serde_json::Value), TestError> {
			let v: serde_json::Value = serde_json::from_slice(&data)?;
			let mut arr = match v {
				serde_json::Value::Array(a) => a,
				_ => return Err(TestError("expected [header, data]".into())),
			};
			let data = arr.pop().ok_or_else(|| TestError("no data".into()))?;
			let header = arr.pop().ok_or_else(|| TestError("no header".into()))?;
			Ok((serde_json::from_value(header)?, data))
		}

		fn decode_data<T: serde::de::DeserializeOwned>(
			data: serde_json::Value,
		) -> Result<T, TestError> {
			Ok(serde_json::from_value(data)?)
		}

		fn encode_data<T: Serialize>(headers: OpaquePacketWrapper<u32>, data: T) -> Bytes {
			let arr = serde_json::Value::Array(vec![
				serde_json::to_value(headers).expect("header"),
				serde_json::to_value(data).expect("data"),
			]);
			Bytes::from(serde_json::to_vec(&arr).expect("encode"))
		}
	}

	#[derive(Serialize, Deserialize)]
	struct Give {
		res: Resource<u64>,
	}
	notification!((0x1000) Give);

	#[derive(Serialize, Deserialize)]
	struct GiveMany {
		res: Vec<Resource<u64>>,
	}
	notification!((0x1001) GiveMany);

	#[derive(Serialize, Deserialize)]
	struct Take {
		res: Resource<u64>,
	}
	request!((0x2000) Take => u64);

	#[derive(Serialize, Deserialize)]
	struct Ask;
	request!((0x2001) Ask => Resource<u64>);

	fn signal(tx: oneshot::Sender<()>) -> impl FnOnce() + Send + Sync + 'static {
		let tx = Mutex::new(Some(tx));
		move || {
			if let Some(tx) = tx.lock().unwrap().take() {
				let _ = tx.send(());
			}
		}
	}

	fn connect() -> (Port, Port) {
		let (a_out, mut a_out_rx) = unbounded_channel::<Bytes>();
		let (b_out, mut b_out_rx) = unbounded_channel::<Bytes>();
		// a_out carries A->B, b_out carries B->A.
		let port_a = Port::new(move |mut rx, tx| async move {
			loop {
				tokio::select! {
					msg = rx.recv() => match msg { Some(m) => { let _ = a_out.send(m); }, None => break },
					msg = b_out_rx.recv() => match msg { Some(m) => { let _ = tx.send(m); }, None => break },
				}
			}
		});
		let port_b = Port::new(move |mut rx, tx| async move {
			loop {
				tokio::select! {
					msg = rx.recv() => match msg { Some(m) => { let _ = b_out.send(m); }, None => break },
					msg = a_out_rx.recv() => match msg { Some(m) => { let _ = tx.send(m); }, None => break },
				}
			}
		});
		(port_a, port_b)
	}

	fn linked() -> (Rpc<TestConfig>, Rpc<TestConfig>) {
		let a = Rpc::<TestConfig>::new(1);
		let b = Rpc::<TestConfig>::new(2);
		let (pa, pb) = connect();
		a.add_direct(2, pa, Rtt(0));
		b.add_direct(1, pb, Rtt(0));
		(a, b)
	}

	#[tokio::test]
	async fn remote_drop_runs_cleanup() {
		let (a, b) = linked();

		// B's handler receives Give and lets it drop at the end of the future,
		// which sends ResourceDropped back to A.
		b.register_notification_handler(|_src: u32, _g: Give| async move { Ok(()) });

		let (done_tx, done_rx) = oneshot::channel::<u64>();
		let done_tx = Mutex::new(Some(done_tx));
		let res = Resource::new(42u64, move || {
			let _ = done_tx.lock().unwrap().take().unwrap().send(42);
		});
		a.notify(2, &Give { res });

		assert_eq!(done_rx.await.unwrap(), 42, "cleanup should run on owner");
	}

	#[tokio::test]
	async fn unreachable_runs_cleanup() {
		let (a, b) = linked();

		// B keeps the handle alive (leaks it) so it never sends ResourceDropped.
		// It signals once it has received the resource.
		let (got_tx, got_rx) = oneshot::channel::<()>();
		let got_tx = Mutex::new(Some(got_tx));
		b.register_notification_handler(move |_src: u32, g: Give| {
			let got_tx = got_tx.lock().unwrap().take();
			async move {
				std::mem::forget(g); // never drop -> no remote ResourceDropped
				if let Some(t) = got_tx {
					let _ = t.send(());
				}
				Ok(())
			}
		});

		let (done_tx, done_rx) = oneshot::channel::<()>();
		let done_tx = Mutex::new(Some(done_tx));
		let res = Resource::new(7u64, move || {
			let _ = done_tx.lock().unwrap().take().unwrap().send(());
		});
		a.notify(2, &Give { res });

		got_rx.await.unwrap();

		a.remove_direct(2);

		done_rx.await.unwrap();
	}

	#[tokio::test]
	async fn request_argument_runs_cleanup() {
		let (a, b) = linked();

		b.register_request_handler(false, |_src: u32, t: Take| async move {
			let v = *t.res; // read the handle
			drop(t.res); // -> ResourceDropped back to A
			Ok(v + 1)
		});

		let (done_tx, done_rx) = oneshot::channel::<()>();
		let res = Resource::new(41u64, signal(done_tx));
		let resp = a.request(2, Take { res }).await.expect("response");
		assert_eq!(resp, 42);

		done_rx.await.unwrap();
	}

	#[tokio::test]
	async fn response_carries_resource() {
		let (a, b) = linked();

		let (done_tx, done_rx) = oneshot::channel::<()>();
		let done_tx = Mutex::new(Some(done_tx));
		b.register_request_handler(false, move |_src: u32, _a: Ask| {
			let cleanup = done_tx.lock().unwrap().take();
			async move {
				let res = Resource::new(99u64, cleanup.map(signal).expect("called once"));
				Ok(res)
			}
		});

		let res: Resource<u64> = a.request(2, Ask).await.expect("response");
		assert_eq!(*res, 99);
		drop(res); // -> ResourceDropped back to B
		done_rx.await.unwrap();
	}

	#[tokio::test]
	async fn proxy_chain_cleanup() {
		let a = Rpc::<TestConfig>::new(1);
		let b = Rpc::<TestConfig>::new(2);
		let c = Rpc::<TestConfig>::new(3);
		let (pab, pba) = connect();
		let (pbc, pcb) = connect();
		a.add_direct(2, pab, Rtt(0));
		b.add_direct(1, pba, Rtt(0));
		b.add_direct(3, pbc, Rtt(0));
		c.add_direct(2, pcb, Rtt(0));

		// B forwards whatever it receives on to C.
		let b2 = b.clone();
		b.register_notification_handler(move |_src: u32, g: Give| {
			let b = b2.clone();
			async move {
				b.notify(3, &Give { res: g.res });
				Ok(())
			}
		});
		// C drops it => unwinds B => A.
		c.register_notification_handler(|_src: u32, _g: Give| async move { Ok(()) });

		let (done_tx, done_rx) = oneshot::channel::<()>();
		let res = Resource::new(5u64, signal(done_tx));
		a.notify(2, &Give { res });

		done_rx.await.unwrap();
	}

	#[tokio::test]
	async fn cleanup_runs_once() {
		use std::sync::atomic::{AtomicUsize, Ordering};
		use std::sync::Arc;

		let (a, b) = linked();
		b.register_notification_handler(|_src: u32, _g: Give| async move { Ok(()) });

		let count = Arc::new(AtomicUsize::new(0));
		let (done_tx, done_rx) = oneshot::channel::<()>();
		let done_tx = Mutex::new(Some(done_tx));
		let c2 = count.clone();
		let res = Resource::new(1u64, move || {
			c2.fetch_add(1, Ordering::SeqCst);
			let _ = done_tx.lock().unwrap().take().unwrap().send(());
		});
		a.notify(2, &Give { res });

		done_rx.await.unwrap();
		assert_eq!(count.load(Ordering::SeqCst), 1);
	}

	#[tokio::test]
	async fn multiple_resources_in_one_message() {
		let (a, b) = linked();
		b.register_notification_handler(|_src: u32, _g: GiveMany| async move { Ok(()) });

		let (t1, r1) = oneshot::channel::<()>();
		let (t2, r2) = oneshot::channel::<()>();
		let (t3, r3) = oneshot::channel::<()>();
		let res = vec![
			Resource::new(1u64, signal(t1)),
			Resource::new(2u64, signal(t2)),
			Resource::new(3u64, signal(t3)),
		];
		a.notify(2, &GiveMany { res });

		r1.await.unwrap();
		r2.await.unwrap();
		r3.await.unwrap();
	}

	#[test]
	fn local_drop_runs_cleanup() {
		use std::sync::atomic::{AtomicBool, Ordering};
		use std::sync::Arc;
		let flag = Arc::new(AtomicBool::new(false));
		let f = flag.clone();
		{
			let _r = Resource::new((), move || f.store(true, Ordering::SeqCst));
		}
		assert!(flag.load(Ordering::SeqCst));
	}

	#[test]
	fn into_inner_disarms() {
		use std::sync::atomic::{AtomicBool, Ordering};
		use std::sync::Arc;
		let flag = Arc::new(AtomicBool::new(false));
		let f = flag.clone();
		let res = Resource::new(123u64, move || f.store(true, Ordering::SeqCst));
		assert_eq!(res.get_and_leak(), 123);
		assert!(!flag.load(Ordering::SeqCst), "cleanup must not run");
	}

	#[test]
	fn close_runs_and_returns() {
		use std::sync::atomic::{AtomicBool, Ordering};
		use std::sync::Arc;
		let flag = Arc::new(AtomicBool::new(false));
		let f = flag.clone();
		let res = Resource::new(123u64, move || f.store(true, Ordering::SeqCst));
		assert_eq!(res.get_and_drop(), 123);
		assert!(flag.load(Ordering::SeqCst));
	}

	#[test]
	fn serialize_outside_context_errors() {
		let res = Resource::new(1u64, || {});
		assert!(
			serde_json::to_value(&res).is_err(),
			"serialize without encode context must fail"
		);
		assert!(
			serde_json::to_value(&res).is_err(),
			"still fails, still armed"
		);
	}

	#[test]
	fn double_serialize_errors() {
		let res = Resource::new(1u64, || {});
		let (results, _parked) = super::with_encode(|| {
			let first = serde_json::to_value(&res);
			let second = serde_json::to_value(&res);
			(first, second)
		});
		assert!(results.0.is_ok(), "first serialize ok");
		assert!(results.1.is_err(), "second serialize must error");
	}
}
