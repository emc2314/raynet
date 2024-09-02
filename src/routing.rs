use atomic_float::AtomicF32;
use rand::distributions::{Distribution, WeightedIndex};
use std::fmt::{self, Debug};
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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

    pub fn add(&self, count: u32) {
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
        self.buffer[current_index].fetch_add(count, Ordering::Relaxed);
        self.last_update.store(now, Ordering::Relaxed);
    }

    pub fn inc(&self) {
        self.add(1);
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

pub struct NodeInfo {
    pub name: String,
    pub addr: SocketAddr,
    pub tc: TimedCounter,
    pub weight: AtomicF32,
}
impl Debug for NodeInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeInfo")
            .field("name", &self.name)
            .field("addr", &self.addr)
            .field("tc", &self.tc.get())
            .field("weight", &self.weight.load(Ordering::Relaxed))
            .finish()
    }
}
#[derive(Debug)]
pub struct Nodes {
    pub nodes: Vec<NodeInfo>,
}
impl Nodes {
    pub fn new(names: Vec<String>) -> Nodes {
        Nodes {
            nodes: names
                .into_iter()
                .map(|name| NodeInfo {
                    name: name.clone(),
                    addr: name
                        .to_socket_addrs()
                        .expect("Unable to resolve send address")
                        .next()
                        .unwrap(),
                    tc: TimedCounter::new(),
                    weight: AtomicF32::new(1.0),
                })
                .collect(),
        }
    }
    fn softmax(&self, rng: &mut rand::rngs::ThreadRng) -> usize {
        let exps = self
            .nodes
            .iter()
            .map(|x| (x.weight.load(Ordering::Relaxed) * 10.0).exp());
        let sum: f32 = exps.clone().sum();
        let dist = WeightedIndex::new(exps.map(|x| x / sum)).unwrap();
        dist.sample(rng)
    }
    pub fn route(&self, rng: &mut rand::rngs::ThreadRng) -> SocketAddr {
        let index = if self.nodes.len() > 1 {
            self.softmax(rng)
        } else {
            0
        };
        self.nodes[index].tc.inc();
        self.nodes[index].addr
    }
}
