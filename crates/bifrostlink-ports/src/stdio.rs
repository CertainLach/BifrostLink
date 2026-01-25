use bifrostlink::Port;
use bytes::BytesMut;
use tokio::io::{stdin, stdout, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::join;
use tracing::error;

// Similar to native messaging, but is independent of host edianess.
// (Not that it would even matter, nobody uses big endian anymore,
// but a bit cleaner in semantics)
pub fn from_stdio() -> Port {
	Port::new(|mut rx, tx| async move {
		let reader = async move {
			let mut stdin = stdin();
			loop {
				let len = match stdin.read_u32().await {
					Ok(len) => len,
					Err(e) => {
						error!("stdin read failed: {e}");
						break;
					}
				};
				let mut buf = BytesMut::zeroed(len as usize);
				if let Err(e) = stdin.read_exact(&mut buf).await {
					error!("stdin read failed: {e}");
					break;
				}
				if tx.send(buf.freeze()).is_err() {
					break;
				}
			}
		};
		let writer = async move {
			let mut stdout = stdout();
			while let Some(msg) = rx.recv().await {
				let len = match u32::try_from(msg.len()) {
					Ok(len) => len,
					Err(_) => {
						error!("message shouldn't be larger than 4GB");
						break;
					}
				};
				if stdout.write_u32(len).await.is_err()
					|| stdout.write_all(&msg).await.is_err()
					|| stdout.flush().await.is_err()
				{
					error!("stdout write failed");
					break;
				}
			}
		};
		join!(reader, writer);
	})
}
