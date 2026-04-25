use std::{
    collections::VecDeque,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

use futures::Stream;

use anie_provider::ProviderError;

pub(super) struct NdjsonLines<S, B> {
    inner: S,
    buffer: Vec<u8>,
    pending: VecDeque<String>,
    _chunk: PhantomData<fn() -> B>,
}

impl<S, B> NdjsonLines<S, B> {
    pub(super) fn new(inner: S) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            pending: VecDeque::new(),
            _chunk: PhantomData,
        }
    }

    fn push_chunk(&mut self, chunk: &[u8]) -> Result<(), ProviderError> {
        self.buffer.extend_from_slice(chunk);
        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffer.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            self.pending
                .push_back(String::from_utf8(line).map_err(|error| {
                    ProviderError::MalformedStreamEvent(format!("invalid UTF-8 in NDJSON: {error}"))
                })?);
        }
        Ok(())
    }

    fn finish_buffer(&mut self) -> Result<(), ProviderError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let mut line = std::mem::take(&mut self.buffer);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if !line.is_empty() {
            self.pending
                .push_back(String::from_utf8(line).map_err(|error| {
                    ProviderError::MalformedStreamEvent(format!("invalid UTF-8 in NDJSON: {error}"))
                })?);
        }
        Ok(())
    }
}

impl<S, B> Stream for NdjsonLines<S, B>
where
    S: Stream<Item = Result<B, reqwest::Error>> + Unpin,
    B: AsRef<[u8]>,
{
    type Item = Result<String, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(line) = self.pending.pop_front() {
            return Poll::Ready(Some(Ok(line)));
        }

        loop {
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    if let Err(error) = self.push_chunk(chunk.as_ref()) {
                        return Poll::Ready(Some(Err(error)));
                    }
                    if let Some(line) = self.pending.pop_front() {
                        return Poll::Ready(Some(Ok(line)));
                    }
                }
                Poll::Ready(Some(Err(error))) => {
                    return Poll::Ready(Some(Err(ProviderError::Transport(error.to_string()))));
                }
                Poll::Ready(None) => {
                    if let Err(error) = self.finish_buffer() {
                        return Poll::Ready(Some(Err(error)));
                    }
                    return Poll::Ready(self.pending.pop_front().map(Ok));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::{StreamExt, stream};

    use super::*;

    async fn collect_lines(chunks: Vec<Vec<u8>>) -> Result<Vec<String>, ProviderError> {
        let stream = stream::iter(chunks.into_iter().map(Ok));
        NdjsonLines::new(stream)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect()
    }

    #[tokio::test]
    async fn ndjson_splitter_handles_chunks_split_across_boundaries() {
        let lines = collect_lines(vec![b"{\"a\":".to_vec(), b"1}\n{\"b\":2}\n".to_vec()])
            .await
            .expect("lines");

        assert_eq!(lines, vec!["{\"a\":1}", "{\"b\":2}"]);
    }

    #[tokio::test]
    async fn ndjson_splitter_handles_utf8_across_chunk_boundaries() {
        let bytes = "hé\n".as_bytes();
        let lines = collect_lines(vec![bytes[..2].to_vec(), bytes[2..].to_vec()])
            .await
            .expect("lines");

        assert_eq!(lines, vec!["hé"]);
    }

    #[tokio::test]
    async fn ndjson_splitter_surfaces_invalid_utf8_as_provider_error() {
        let error = collect_lines(vec![vec![0xff, b'\n']])
            .await
            .expect_err("invalid utf8 should fail");

        assert!(matches!(error, ProviderError::MalformedStreamEvent(_)));
    }

    #[tokio::test]
    async fn ndjson_splitter_handles_trailing_incomplete_line_and_crlf() {
        let lines = collect_lines(vec![b"{\"a\":1}\r\n{\"b\":2}".to_vec()])
            .await
            .expect("lines");

        assert_eq!(lines, vec!["{\"a\":1}", "{\"b\":2}"]);
    }
}
