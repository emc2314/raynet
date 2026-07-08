use arc_swap::ArcSwap;
use atomic_float::AtomicF32;
use bitcode::{Decode, Encode};
use rand::distr::{Distribution, weighted::WeightedIndex};
use std::fmt::{self, Debug};
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

const BUCKET_NUM: usize = 64;
#[derive(Debug)]
pub struct TimedCounter {
    buffer: [AtomicU32; BUCKET_NUM],
    last_update: AtomicU64,
}

impl TimedCounter {
    const WINDOW_MS: u64 = 4096;
    const BUCKET_LENGTH: u64 = Self::WINDOW_MS / BUCKET_NUM as u64;

    pub fn new(time_ms: u64) -> Self {
        TimedCounter {
            buffer: [(); BUCKET_NUM].map(|_| AtomicU32::new(0)),
            last_update: AtomicU64::new(time_ms),
        }
    }

    pub fn add(&self, time_ms: u64, count: u32) {
        let last_update = self.last_update.load(Ordering::Relaxed);
        let current_index = ((time_ms / Self::BUCKET_LENGTH) % BUCKET_NUM as u64) as usize;

        for i in 0..BUCKET_NUM {
            let index = (current_index + BUCKET_NUM - i) % BUCKET_NUM;
            if time_ms - (time_ms % Self::BUCKET_LENGTH)
                <= last_update + i as u64 * Self::BUCKET_LENGTH
            {
                break;
            }
            self.buffer[index].store(0, Ordering::Relaxed);
        }
        self.buffer[current_index].fetch_add(count, Ordering::Relaxed);
        self.last_update.store(time_ms, Ordering::Relaxed);
    }

    pub fn inc(&self, time_ms: u64) {
        self.add(time_ms, 1);
    }

    pub fn get(&self, time_ms: u64) -> u32 {
        let current_index = ((time_ms / Self::BUCKET_LENGTH) % BUCKET_NUM as u64) as usize;
        let last_update = self.last_update.load(Ordering::Relaxed);
        let mut total = 0;

        for i in 0..BUCKET_NUM {
            if time_ms - (time_ms % Self::BUCKET_LENGTH)
                <= last_update + i as u64 * Self::BUCKET_LENGTH
            {
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
            .field("weight", &self.weight.load(Ordering::Relaxed))
            .finish()
    }
}
#[derive(Debug)]
pub struct Nodes {
    pub nodes: Vec<NodeInfo>,
    dist: ArcSwap<WeightedIndex<f32>>,
}
impl Nodes {
    pub fn new(names: Vec<String>, time_ms: u64) -> Nodes {
        let len = names.len();
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
                    tc: TimedCounter::new(time_ms),
                    weight: AtomicF32::new(1.0),
                })
                .collect(),
            dist: ArcSwap::from(Arc::new(WeightedIndex::new(vec![1.0; len]).unwrap())),
        }
    }
    pub fn build_dist(&self) {
        let exps = self
            .nodes
            .iter()
            .map(|x| (x.weight.load(Ordering::Relaxed) * 10.0).exp());
        self.dist.store(Arc::new(WeightedIndex::new(exps).unwrap()));
    }
    pub fn route<R: rand::Rng + ?Sized>(&self, time_ms: u64, rng: &mut R) -> SocketAddr {
        let index = if self.nodes.len() > 1 {
            self.dist.load().sample(rng)
        } else {
            0
        };
        self.nodes[index].tc.inc(time_ms);
        self.nodes[index].addr
    }
    pub fn sum(&self, time_ms: u64) -> u32 {
        self.nodes.iter().map(|x| x.tc.get(time_ms)).sum()
    }
}

#[derive(Encode, Decode, Debug)]
pub struct StatRequest {
    pub index: u32,
    pub tc: f32,
}
#[derive(Encode, Decode, Debug)]
pub struct StatResponse {
    pub index: u32,
    pub weight: f32,
}
