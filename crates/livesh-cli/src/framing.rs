use anyhow::{Context, bail};
use livesh_protocol::{MAX_FRAME_LEN, decode_payload, encode_frame};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub async fn read_frame<T, R>(reader: &mut R) -> anyhow::Result<T>
where
    T: DeserializeOwned,
    R: AsyncRead + Unpin,
{
    let mut len = [0_u8; 4];
    reader
        .read_exact(&mut len)
        .await
        .context("read frame length")?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME_LEN {
        bail!("frame length {len} exceeds maximum");
    }

    let mut payload = vec![0; len];
    reader
        .read_exact(&mut payload)
        .await
        .context("read frame payload")?;
    Ok(decode_payload(&payload)?)
}

pub async fn write_frame<T, W>(writer: &mut W, value: &T) -> anyhow::Result<()>
where
    T: Serialize,
    W: AsyncWrite + Unpin,
{
    let frame = encode_frame(value)?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}
