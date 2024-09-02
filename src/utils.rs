use core::fmt;
use fastbloom::BloomFilter;
use std::hash::Hash;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};

#[inline]
pub fn now_millis() -> u64 {
    let start = SystemTime::now();
    let since_the_epoch = start
        .duration_since(UNIX_EPOCH)
        .expect("time went afterwards");
    since_the_epoch.as_millis() as u64
}

pub trait UnwrapNone<T> {
    #[allow(dead_code)]
    fn expect_none(self, msg: &str);
    #[allow(dead_code)]
    fn unwrap_none(self);
    #[allow(dead_code)]
    fn unwrap_none_or_else<F>(self, f: F)
    where
        F: FnOnce(T);
}

impl<T> UnwrapNone<T> for Option<T>
where
    T: fmt::Debug,
{
    #[inline]
    #[track_caller]
    fn expect_none(self, msg: &str) {
        if let Some(val) = self {
            expect_none_failed(msg, &val);
        }
    }

    #[inline]
    #[track_caller]
    fn unwrap_none(self) {
        if let Some(val) = self {
            expect_none_failed("called `Option::unwrap_none()` on a `Some` value", &val);
        }
    }

    #[inline]
    fn unwrap_none_or_else<F>(self, f: F)
    where
        F: FnOnce(T),
    {
        if let Some(val) = self {
            f(val)
        }
    }
}

#[inline(never)]
#[cold]
#[track_caller]
fn expect_none_failed(msg: &str, value: &dyn fmt::Debug) -> ! {
    panic!("{}: {:?}", msg, value)
}

pub struct NonceFilter {
    filter: Arc<RwLock<(BloomFilter, BloomFilter)>>,
}

impl NonceFilter {
    pub fn new(size: usize, p_false: f64, timeout: Duration) -> Arc<Self> {
        let checker = Arc::new(NonceFilter {
            filter: Arc::new(RwLock::new((
                BloomFilter::with_false_pos(p_false).expected_items(size),
                BloomFilter::with_false_pos(p_false).expected_items(size),
            ))),
        });

        let checker_clone = checker.clone();
        tokio::spawn(async move {
            let mut interval = interval(timeout);
            loop {
                interval.tick().await;

                let mut filter = checker_clone.filter.write().await;
                let (curr, prev) = &mut *filter;
                std::mem::swap(curr, prev);
                filter.0.clear();
            }
        });

        checker
    }

    pub async fn check_and_set<T: Hash + ?Sized>(&self, nonce: &T) -> bool {
        {
            let (curr, prev) = &*self.filter.read().await;
            if curr.contains(nonce) || prev.contains(nonce) {
                return false;
            }
        }

        self.filter.write().await.0.insert(nonce);
        true
    }
}
