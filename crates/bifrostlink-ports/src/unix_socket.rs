use std::io;

use bifrostlink::Port;
use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::join;
use tokio::net::unix::{ReadHalf, WriteHalf};
use tokio::net::UnixStream;
use tracing::error;

async fn read_bytes(srx: &mut ReadHalf<'_>) -> io::Result<BytesMut> {
	let len = srx.read_u32().await?;
	let mut buf = BytesMut::zeroed(len as usize);
	srx.read_exact(&mut buf).await?;
	Ok(buf)
}
async fn write_bytes(stx: &mut WriteHalf<'_>, value: Bytes) -> io::Result<()> {
	let len = u32::try_from(value.len()).map_err(|_| {
		io::Error::new(
			io::ErrorKind::InvalidInput,
			"message shouldn't be larger than 4GB",
		)
	})?;
	stx.write_u32(len).await?;
	stx.write_all(&value).await?;
	Ok(())
}

pub fn from_socket(mut socket: UnixStream) -> Port {
	Port::new(|mut rx, tx| async move {
		let (mut srx, mut stx) = socket.split();
		let srx_task = async move {
			loop {
				match read_bytes(&mut srx).await {
					Ok(buf) => {
						if tx.send(buf.freeze()).is_err() {
							break;
						}
					}
					Err(e) => {
						error!("socket read failed: {e}");
						break;
					}
				}
			}
			error!("input stream end")
		};
		let stx_task = async move {
			while let Some(value) = rx.recv().await {
				if let Err(e) = write_bytes(&mut stx, value).await {
					error!("socket write failed: {e}");
					break;
				}
			}
			error!("output stream end")
		};
		join!(srx_task, stx_task);
	})
}
