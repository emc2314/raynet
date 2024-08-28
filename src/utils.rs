use core::fmt;
use fastbloom::BloomFilter;
use std::hash::Hash;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
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

const BUCKET_NUM: usize = 64;
#[derive(Debug)]
pub struct TimedCounter {
    buffer: [AtomicU32; BUCKET_NUM],
    last_update: AtomicU64,
}

impl TimedCounter {
    const WINDOW_NANOS: u64 = 4294967296;
    const BUCKET_LENGTH: u64 = Self::WINDOW_NANOS / BUCKET_NUM as u64;

    pub fn new() -> Self {
        TimedCounter {
            buffer: [(); BUCKET_NUM].map(|_| AtomicU32::new(0)),
            last_update: AtomicU64::new(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64,
            ),
        }
    }

    pub fn inc(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let last_update = self.last_update.load(Ordering::Relaxed);
        let current_index = ((now / Self::BUCKET_LENGTH) % BUCKET_NUM as u64) as usize;

        for i in 0..BUCKET_NUM {
            let index = (current_index + BUCKET_NUM - i) % BUCKET_NUM;
            if now - (now % Self::BUCKET_LENGTH) <= last_update + i as u64 * Self::BUCKET_LENGTH {
                break;
            }
            self.buffer[index].store(0, Ordering::Relaxed);
        }
        self.buffer[current_index].fetch_add(1, Ordering::Relaxed);
        self.last_update.store(now, Ordering::Relaxed);
    }

    pub fn get(&self) -> u32 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let current_index = ((now / Self::BUCKET_LENGTH) % BUCKET_NUM as u64) as usize;
        let last_update = self.last_update.load(Ordering::Relaxed);
        let mut total = 0;

        for i in 0..BUCKET_NUM {
            if now - (now % Self::BUCKET_LENGTH) <= last_update + i as u64 * Self::BUCKET_LENGTH {
                let index = (current_index + BUCKET_NUM - i) % BUCKET_NUM;
                total += self.buffer[index].load(Ordering::Relaxed);
            }
        }

        total
    }
}
