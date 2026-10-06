//! One deadline for admission, dispatch, and response delivery.
use axum::{
    body::{Body, Bytes},
    response::Response,
};
use hyper::body::{Body as HttpBody, Frame};
use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::time::{Instant, Sleep, Timeout};

#[derive(Clone, Copy)]
pub(crate) struct ExchangeDeadline(Instant);
impl ExchangeDeadline {
    pub(crate) fn new(duration: Duration) -> Self {
        Self(Instant::now() + duration) // determinism-ok: HTTP I/O deadline
    }
    pub(crate) fn wait<F: Future>(&self, future: F) -> Timeout<F> {
        tokio::time::timeout_at(self.0, future)
    }
    pub(crate) fn remaining(&self) -> Duration {
        self.0.saturating_duration_since(Instant::now()) // determinism-ok: HTTP I/O deadline
    }
    pub(crate) fn bound_response(self, response: Response, remaining: usize) -> Response {
        let (parts, inner) = response.into_parts();
        Response::from_parts(
            parts,
            Body::new(BoundedBody {
                inner,
                remaining,
                timer: Box::pin(tokio::time::sleep_until(self.0)),
                done: false,
            }),
        )
    }
}

// Preserve trailers as well as data frames. Once headers are sent, exceeding a
// budget terminates the body with an error rather than inventing a new status.
struct BoundedBody {
    inner: Body,
    timer: Pin<Box<Sleep>>,
    remaining: usize,
    done: bool,
}
impl HttpBody for BoundedBody {
    type Data = Bytes;
    type Error = io::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        if self.done {
            return Poll::Ready(None);
        }
        if self.timer.as_mut().poll(cx).is_ready() {
            self.done = true;
            return Poll::Ready(Some(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP exchange timed out",
            ))));
        }
        match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    if data.len() > self.remaining {
                        self.done = true;
                        return Poll::Ready(Some(Err(io::Error::other(
                            "HTTP response exceeds byte budget",
                        ))));
                    }
                    self.remaining -= data.len();
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                self.done = true;
                Poll::Ready(Some(Err(io::Error::other(error))))
            }
            Poll::Ready(None) => {
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn sequential_phases_share_one_deadline() {
        let start = Instant::now();
        let deadline = ExchangeDeadline::new(Duration::from_secs(10));
        assert!(
            deadline
                .wait(tokio::time::sleep(Duration::from_secs(6)))
                .await
                .is_ok()
        );
        assert!(
            deadline
                .wait(tokio::time::sleep(Duration::from_secs(6)))
                .await
                .is_err()
        );
        assert_eq!(start.elapsed(), Duration::from_secs(10));
    }
}
