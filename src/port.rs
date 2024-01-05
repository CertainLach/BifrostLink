use std::{
	future::Future,
	io::{self, Read, Write},
	process::Stdio,
};

use bytes::{Bytes, BytesMut};
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	join,
	process::Command,
	spawn,
	sync::mpsc::{unbounded_channel, UnboundedReceiver as Receiver, UnboundedSender as Sender},
	task::spawn_blocking,
};
use tracing::error;

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

#[cfg(target_os = "windows")]
#[link(name = "msvcrt")]
extern "C" {
	fn _setmode(fd: i32, mode: i32) -> i32;
}

#[cfg(target_os = "windows")]
struct ResetMode {
	fd: i32,
	mode: i32,
}
#[cfg(target_os = "windows")]
impl Drop for ResetMode {
	fn drop(&mut self) {
		unsafe { _setmode(self.fd, self.mode) }
	}
}

#[cfg(target_os = "windows")]
fn set_mode(fd: i32, mode: i32) -> ResetMode {
	let old_mode = unsafe { _setmode(fd, mode) };
	assert_ne!(old_mode, -1, "invalid fd/mode");
	ResetMode { fd, mode: old_mode }
}

pub fn native_messaging_port() -> Port {
	Port::new(|mut rx, tx| async move {
		#[cfg(target_os = "windows")]
		let _stdout_guard = set_mode(0, 0x8000);
		#[cfg(target_os = "windows")]
		let _stdin_guard = set_mode(1, 0x8000);

		let stdout_printer = spawn_blocking(move || {
			let mut stdout = std::io::stdout().lock();
			while let Some(out) = rx.blocking_recv() {
				let len = u32::try_from(out.len()).expect("can't be larger");
				let succeeded: io::Result<()> = try {
					let size = u32::to_ne_bytes(len);
					stdout.write_all(&size)?;
					stdout.write_all(&out)?;
					stdout.flush()?;
				};
				if let Err(e) = succeeded {
					error!("stdout write failed: {e}");
					break;
				}
			}
			eprintln!("output stream end")
		});
		let stdin_reader = spawn_blocking(move || {
			let mut stdin = std::io::stdin().lock();
			loop {
				let succeeded: io::Result<()> = try {
					let mut size = [0; 4];
					stdin.read_exact(&mut size)?;
					let size = u32::from_ne_bytes(size) as usize;
					let mut buf = BytesMut::zeroed(size);
					stdin.read_exact(&mut buf)?;
					if tx.send(buf.freeze()).is_err() {
						break;
					}
				};
				if let Err(e) = succeeded {
					error!("stdin read failed: {e}");
					break;
				};
			}
			eprintln!("input stream end");
		});

		// TODO: select!
		let (a, b) = join!(stdout_printer, stdin_reader);
		a.unwrap();
		b.unwrap();
	})
}

pub async fn start_native_messaging_extension(mut cmd: Command) -> io::Result<Port> {
	cmd.stdin(Stdio::piped());
	cmd.stdout(Stdio::piped());
	cmd.stderr(Stdio::inherit());
	let mut proc = cmd.spawn()?;
	let mut stdout = proc.stdout.take().expect("stdout piped");
	let mut stdin = proc.stdin.take().expect("stdin piped");
	Ok(Port::new(|mut rx, tx| async move {
		let stdin_printer = spawn(async move {
			while let Some(msg) = rx.recv().await {
				let succeeded: io::Result<()> = try {
					let len = u32::try_from(msg.len()).map_err(|_| {
						io::Error::new(
							io::ErrorKind::InvalidInput,
							"message shouldn't be larger than 4GB",
						)
					})?;
					let len = u32::to_be_bytes(len);
					stdin.write_all(&len).await?;
				};
				if let Err(e) = succeeded {
					error!("stdin write failed: {e}");
					break;
				};
			}
			eprintln!("output stream end");
		});
		let stdout_reader = spawn(async move {
			loop {
				let succeeded: io::Result<()> = try {
					let mut size = [0; 4];
					stdout.read_exact(&mut size).await?;
					let size = u32::from_ne_bytes(size) as usize;
					let mut buf = BytesMut::zeroed(size);
					stdout.read_exact(&mut buf).await?;
					if tx.send(buf.freeze()).is_err() {
						break;
					}
				};
				if let Err(e) = succeeded {
					error!("stdout read failed: {e}");
					break;
				};
			}
			eprintln!("input stream end");
		});

		// TODO: select!
		let (a, b) = join!(stdin_printer, stdout_reader);
		a.unwrap();
		b.unwrap();
	}))
}
