use atomic_float::AtomicF32;
use rand::distributions::{Distribution, WeightedIndex};
use std::net::SocketAddr;

use crate::utils::TimedCounter;

#[derive(Debug)]
pub struct NodeInfo {
    pub addr: SocketAddr,
    pub tc: TimedCounter,
    pub weight: AtomicF32,
}

#[derive(Debug)]
pub struct Nodes {
    pub nodes: Vec<NodeInfo>,
}
impl Nodes {
    pub fn new(addrs: Vec<SocketAddr>) -> Nodes {
        Nodes {
            nodes: addrs
                .into_iter()
                .map(|addr| NodeInfo {
                    addr,
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
            .map(|x| (x.weight.load(std::sync::atomic::Ordering::Relaxed)).exp());
        let sum: f32 = exps.clone().sum();
        let dist = WeightedIndex::new(exps.map(|x| x / sum)).unwrap();
        dist.sample(rng)
    }
    pub fn route(&self, rng: &mut rand::rngs::ThreadRng) -> SocketAddr {
        let mut index = 0;
        if self.nodes.len() > 1 {
            index = self.softmax(rng);
        }
        self.nodes[index].tc.inc();
        self.nodes[index].addr
    }
}

#[derive(Debug)]
pub struct WeightInfo {
    pub addr: SocketAddr,
    pub weight: f32,
}
