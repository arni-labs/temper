//! The module reviews running in this process, one per app of a tenant. An install's answer does not wait for its
//! review, so this is how a later question learns a review is still reading.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::task::AbortHandle;

struct Running {
    turn: u64,
    version: String,
    abort: AbortHandle,
}

#[derive(Default)]
struct Reviews {
    turns: u64,
    running: BTreeMap<(String, String), Running>,
}

/// The reviews under way.
#[derive(Default)]
pub struct ReviewsUnderWay(Mutex<Reviews>);

impl std::fmt::Debug for ReviewsUnderWay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReviewsUnderWay").finish_non_exhaustive()
    }
}

/// Takes a finished, stopped or panicked review off the list.
struct Leaving {
    reviews: Arc<ReviewsUnderWay>,
    key: (String, String),
    turn: u64,
}

impl Drop for Leaving {
    fn drop(&mut self) {
        let mut reviews = self
            .reviews
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if reviews
            .running
            .get(&self.key)
            .is_some_and(|running| running.turn == self.turn)
        {
            reviews.running.remove(&self.key);
        }
    }
}

impl ReviewsUnderWay {
    /// Run `review` for `version` of `app`, stopping the app's review before it: only the newest is kept, so the older
    /// one's paid calls would be for nothing.
    pub(crate) fn start<F>(self: &Arc<Self>, tenant: &str, app: &str, version: &str, review: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let key = (tenant.to_string(), app.to_string());
        let mut reviews = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        reviews.turns += 1;
        let turn = reviews.turns;
        let leaving = Leaving {
            reviews: Arc::clone(self),
            key: key.clone(),
            turn,
        };
        let task = tokio::spawn(async move {
            let _leaving = leaving;
            review.await;
        });
        let running = Running {
            turn,
            version: version.to_string(),
            abort: task.abort_handle(),
        };
        let older = reviews.running.insert(key, running);
        drop(reviews);
        if let Some(older) = older {
            let _ = older.abort;
        }
    }

    /// Whether a review of `version` of `app` is reading now.
    pub fn reading(&self, tenant: &str, app: &str, version: &str) -> bool {
        let reviews = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        reviews
            .running
            .get(&(tenant.to_string(), app.to_string()))
            .is_some_and(|running| running.version == version)
    }
}
