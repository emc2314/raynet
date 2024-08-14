use core::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

#[inline]
pub fn now_millis() -> u32 {
    let start = SystemTime::now();
    let since_the_epoch = start
        .duration_since(UNIX_EPOCH)
        .expect("time went afterwards");
    // (since_the_epoch.as_secs() * 1000 + since_the_epoch.subsec_millis() as u64) as u32
    since_the_epoch.as_millis() as u32
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
