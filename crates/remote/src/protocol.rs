use anyhow::Result;
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use rpc::proto::Envelope;

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub struct MessageId(pub u32);

pub type OutgoingProgress =
    std::sync::Arc<parking_lot::Mutex<std::collections::HashMap<u32, rpc::RequestProgress>>>;

pub type MessageLen = u32;
pub const MESSAGE_LEN_SIZE: usize = size_of::<MessageLen>();

pub fn message_len_from_buffer(buffer: &[u8]) -> MessageLen {
    MessageLen::from_le_bytes(buffer.try_into().unwrap())
}

pub async fn read_message_with_len<S: AsyncRead + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
    message_len: MessageLen,
) -> Result<Envelope> {
    buffer.resize(message_len as usize, 0);
    stream.read_exact(buffer).await?;
    Ok(Envelope::decode_from_slice(buffer.as_slice())?)
}

pub async fn read_message<S: AsyncRead + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
) -> Result<Envelope> {
    buffer.resize(MESSAGE_LEN_SIZE, 0);
    stream.read_exact(buffer).await?;

    let len = message_len_from_buffer(buffer);

    read_message_with_len(stream, buffer, len).await
}

pub async fn write_message<S: AsyncWrite + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
    message: Envelope,
) -> Result<()> {
    write_message_with_progress(stream, buffer, message, None).await
}

pub async fn write_message_with_progress<S: AsyncWrite + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
    message: Envelope,
    progress: Option<rpc::RequestProgress>,
) -> Result<()> {
    let message_len = u32::try_from(message.encoded_size())?;
    stream
        .write_all(message_len.to_le_bytes().as_slice())
        .await?;
    buffer.clear();
    buffer.reserve(message_len as usize);
    message.encode_to_buffer(buffer)?;
    if progress.is_none() {
        stream.write_all(buffer).await?;
        return Ok(());
    }
    let mut written = 0;
    if let Some(progress) = &progress {
        progress(0, message_len as u64);
    }
    for chunk in buffer.chunks(64 * 1024) {
        stream.write_all(chunk).await?;
        written += chunk.len() as u64;
        if let Some(progress) = &progress {
            progress(written, message_len as u64);
        }
    }
    Ok(())
}

pub async fn write_size_prefixed_buffer<S: AsyncWrite + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
) -> Result<()> {
    let len = buffer.len() as u32;
    stream.write_all(len.to_le_bytes().as_slice()).await?;
    stream.write_all(buffer).await?;
    Ok(())
}

#[cfg(test)]
mod transfer_tests {
    use super::*;
    use parking_lot::Mutex;
    use rpc::proto::EnvelopedMessage;
    use std::sync::Arc;

    #[gpui::test]
    async fn test_transfer_progress_preserves_wire_bytes() {
        let message = rpc::proto::CreateProjectEntry {
            project_id: 1,
            worktree_id: 2,
            path: "large.bin".into(),
            is_directory: false,
            content: Some(vec![42; 200_000]),
        }
        .into_envelope(7, None, None);
        let mut expected = Vec::new();
        message
            .encode_to_buffer(&mut expected)
            .expect("encode fixture");
        let samples = Arc::new(Mutex::new(Vec::new()));
        let observer = samples.clone();
        let mut output = futures::io::Cursor::new(Vec::new());
        write_message_with_progress(
            &mut output,
            &mut Vec::new(),
            message,
            Some(Arc::new(move |written, total| {
                observer.lock().push((written, total))
            })),
        )
        .await
        .expect("write");
        let bytes = output.into_inner();
        assert_eq!(&bytes[4..], expected.as_slice());
        assert_eq!(&bytes[..4], &(expected.len() as u32).to_le_bytes());
        let samples = samples.lock();
        assert_eq!(samples.first(), Some(&(0, expected.len() as u64)));
        assert_eq!(
            samples.last(),
            Some(&(expected.len() as u64, expected.len() as u64))
        );
        assert!(samples.len() > 3);
        assert!(samples.windows(2).all(|pair| pair[0].0 < pair[1].0));
    }

    struct BrokenWriter;
    impl AsyncWrite for BrokenWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[gpui::test]
    async fn test_transfer_progress_does_not_complete_failed_write() {
        let samples = Arc::new(Mutex::new(Vec::new()));
        let observer = samples.clone();
        let result = write_message_with_progress(
            &mut BrokenWriter,
            &mut Vec::new(),
            rpc::proto::Ping {}.into_envelope(1, None, None),
            Some(Arc::new(move |written, total| {
                observer.lock().push((written, total))
            })),
        )
        .await;
        assert!(result.is_err());
        assert!(samples.lock().is_empty());
    }
}

pub async fn read_message_raw<S: AsyncRead + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
) -> Result<()> {
    buffer.resize(MESSAGE_LEN_SIZE, 0);
    stream.read_exact(buffer).await?;

    let message_len = message_len_from_buffer(buffer);
    buffer.resize(message_len as usize, 0);
    stream.read_exact(buffer).await?;

    Ok(())
}
