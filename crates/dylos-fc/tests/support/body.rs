use http_body_util::Full;
use hyper::body::{Bytes, Frame};
use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};

pub enum FakeBody {
    Full(Full<Bytes>),
    Channel(tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>),
}

impl hyper::body::Body for FakeBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match &mut *self {
            FakeBody::Full(f) => Pin::new(f).poll_frame(cx),
            FakeBody::Channel(rx) => match rx.poll_recv(cx) {
                Poll::Ready(Some(chunk)) => Poll::Ready(Some(Ok(Frame::data(Bytes::from(chunk))))),
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}
