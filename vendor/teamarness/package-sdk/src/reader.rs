use teamarness_package_protocol::{Frame, MAX_FRAME_BYTES, decode_frame};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;

use crate::SdkError;

pub(crate) async fn run<R>(mut input: R, frames: mpsc::Sender<Frame>) -> Result<(), SdkError>
where
    R: AsyncRead + Unpin,
{
    loop {
        let mut prefix = [0_u8; 4];
        match input.read_exact(&mut prefix).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(SdkError::ParentClosed);
            }
            Err(_) => return Err(SdkError::ParentClosed),
        }
        let length = u32::from_be_bytes(prefix) as usize;
        if length == 0 || length > MAX_FRAME_BYTES {
            return Err(SdkError::Protocol(if length == 0 {
                "zero_length"
            } else {
                "frame_too_large"
            }));
        }
        let mut payload = vec![0; length];
        input
            .read_exact(&mut payload)
            .await
            .map_err(|_| SdkError::ParentClosed)?;
        let mut encoded = prefix.to_vec();
        encoded.extend(payload);
        let frame = decode_frame(&encoded).map_err(SdkError::protocol)?;
        frames
            .send(frame)
            .await
            .map_err(|_| SdkError::ParentClosed)?;
    }
}
