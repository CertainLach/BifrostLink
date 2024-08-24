use std::io;

use bifrostlink::Port;
use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::join;
use tokio::net::UnixStream;
use tracing::error;

pub fn from_socket(mut socket: UnixStream) -> Port {
	Port::new(|mut rx, tx| async move {
		let (mut srx, mut stx) = socket.split();
		let srx_task = async move {
			loop {
				let succeeded: io::Result<()> = try {
					let len = srx.read_u32().await?;
					let mut buf = BytesMut::zeroed(len as usize);
					srx.read_exact(&mut buf).await?;
					if tx.send(buf.freeze()).is_err() {
						break;
					}
				};
				if let Err(e) = succeeded {
					error!("socket read failed: {e}");
					break;
				}
			}
			error!("input stream end")
		};
		let stx_task = async move {
			while let Some(value) = rx.recv().await {
				let succeeded: io::Result<()> = try {
					stx.write_u32(value.len().try_into().expect("can't be larger"))
						.await?;
					stx.write_all(&value).await?;
				};
				if let Err(e) = succeeded {
					error!("socket write failed: {e}");
					break;
				}
			}
			error!("output stream end")
		};
		join!(srx_task, stx_task);
	})
}
