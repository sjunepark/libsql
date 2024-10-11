use std::pin::Pin;
use std::sync::Arc;

use fst::{IntoStreamer, Streamer};
use libsql_sys::name::NamespaceName;
use roaring::RoaringBitmap;
use tokio_stream::Stream;
use zerocopy::FromZeroes;

use crate::io::buf::ZeroCopyBoxIoBuf;
use crate::segment::compacted::CompactedFrame;
use crate::segment::Frame;
use crate::storage::backend::FindSegmentReq;
use crate::storage::Storage;

use super::Result;

pub trait ReplicateFromStorage: Sync + Send + 'static {
    fn stream<'a, 'b>(
        &'b self,
        seen: &'a mut RoaringBitmap,
        current: u64,
        until: u64,
    ) -> Pin<Box<dyn Stream<Item = Result<Box<Frame>>> + 'a + Send>>;
}

pub struct StorageReplicator<S> {
    storage: Arc<S>,
    namespace: NamespaceName,
}

impl<S> StorageReplicator<S> {
    pub fn new(storage: Arc<S>, namespace: NamespaceName) -> Self {
        Self { storage, namespace }
    }
}

impl<S> ReplicateFromStorage for StorageReplicator<S>
where
    S: Storage,
{
    fn stream<'a, 'b>(
        &'b self,
        seen: &'a mut roaring::RoaringBitmap,
        mut current: u64,
        until: u64,
    ) -> Pin<Box<dyn Stream<Item = Result<Box<Frame>>> + Send + 'a>> {
        let storage = self.storage.clone();
        let namespace = self.namespace.clone();
        Box::pin(async_stream::try_stream! {
            loop {
                let key = storage.find_segment(&namespace, FindSegmentReq::EndFrameNoLessThan(current), None).await?;
                let index = storage.fetch_segment_index(&namespace, &key, None).await?;
                let mut pages = index.into_stream();
                let mut maybe_seg = None;
                while let Some((page, offset)) = pages.next() {
                    let page = u32::from_be_bytes(page.try_into().unwrap());
                    // this segment contains data we are interested in, lazy dowload the segment
                    if !seen.contains(page) {
                        seen.insert(page);
                        let segment = match maybe_seg {
                            Some(ref seg) => seg,
                            None => {
                                tracing::debug!(key = %key, "fetching segment");
                                maybe_seg = Some(storage.fetch_segment_data(&namespace, &key, None).await?);
                                maybe_seg.as_ref().unwrap()
                            },
                        };

                        // TODO: The copy here is inneficient. This is OK for now, until we rewrite
                        // this code to read from the main db file instead of storage.
                        let (compacted_frame, ret) = segment.read_frame(ZeroCopyBoxIoBuf::new_uninit(CompactedFrame::new_box_zeroed()), offset as u32).await;
                        ret?;
                        let compacted_frame = compacted_frame.into_inner();
                        let mut frame = Frame::new_box_zeroed();
                        frame.data_mut().copy_from_slice(&compacted_frame.data);

                        let header = frame.header_mut();
                        header.frame_no = compacted_frame.header().frame_no;
                        header.size_after = 0.into();
                        header.page_no = compacted_frame.header().page_no;

                        if frame.header().frame_no() >= until {
                            yield frame;
                        }
                    };
                }

                if key.start_frame_no <= until {
                    break
                }
                current = key.start_frame_no - 1;
            }
        })
    }
}
