use futures::{Future, Stream, StreamExt, TryStream};

use super::error::SyncError;

pub trait SyncStream<T>: Stream<Item = SyncResult<T>> {}
pub type SyncResult<T> = Result<T, SyncError>;

impl<T, U> SyncStream<T> for U where U: Stream<Item = SyncResult<T>> {}

pub trait Stage {
    type Input;
    type Output;

    fn handle(
        &mut self,
        input: Self::Input,
    ) -> impl Future<Output = Option<SyncResult<Self::Output>>> + Send;
}

impl<T, U> SyncStreamExt<T> for U
where
    U: SyncStream<T> + Send + 'static,
    T: Send,
{
}

pub trait SyncStreamExt<T>: SyncStream<T> + Sized + Send + 'static {
    fn pipe<S>(mut self, mut stage: S, buffer: usize) -> impl SyncStream<S::Output>
    where
        S: Stage<Input = T> + Send + 'static,
        <S as Stage>::Output: 'static + Send,
        T: Send,
    {
        let (tx, rx) = tokio::sync::mpsc::channel(buffer);

        tokio::spawn(async move {
            let mut stream = Box::pin(self);
            while let Some(input) = stream.next().await {
                let input = match input {
                    Ok(x) => x,
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                };

                let Some(output) = stage.handle(input).await else {
                    continue;
                };

                if tx.send(output).await.is_err() {
                    return;
                }
            }
        });

        tokio_stream::wrappers::ReceiverStream::new(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::TryStreamExt;
    use p2p::libp2p::PeerId;
    use pathfinder_common::{block_hash, BlockHash, BlockHeader, BlockNumber};

    #[tokio::test]
    async fn header_continuity() {
        #[derive(Default)]
        struct ForwardHeaderCheck {
            next_number: BlockNumber,
            parent_hash: BlockHash,
        }

        impl Stage for ForwardHeaderCheck {
            type Input = BlockHeader;
            type Output = BlockHeader;

            async fn handle(&mut self, input: Self::Input) -> Option<SyncResult<Self::Output>> {
                if self.next_number != input.number || self.parent_hash != input.parent_hash {
                    return Some(Err(SyncError::Discontinuity(PeerId::random())));
                }

                self.next_number += 1;
                self.parent_hash = input.hash;

                Some(Ok(input))
            }
        }

        let header0 = BlockHeader::default();
        let header1 = header0
            .child_builder()
            .finalize_with_hash(block_hash!("0x1"));
        let header2 = header1
            .child_builder()
            .finalize_with_hash(block_hash!("0x2"));

        let expected = vec![header0, header1, header2];

        let headers = tokio_stream::iter(expected.clone()).map(|x| SyncResult::Ok(x));

        let result = headers
            .pipe(ForwardHeaderCheck::default(), 2)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();

        assert_eq!(result, expected);
    }

    #[tokio::test]
    async fn batching() {
        struct Batcher {
            len: usize,
            buffer: [u32; 5],
        }

        impl Stage for Batcher {
            type Input = u32;
            type Output = [u32; 5];

            async fn handle(&mut self, input: Self::Input) -> Option<SyncResult<Self::Output>> {
                self.buffer[self.len] = input;
                self.len += 1;

                if self.buffer.len() == self.len {
                    let output = std::mem::take(&mut self.buffer);

                    Some(Ok(output))
                } else {
                    None
                }
            }
        }
    }
}
