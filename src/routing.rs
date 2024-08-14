use rand::distributions::{Distribution, WeightedIndex};
use std::net::SocketAddr;

fn _softmax(vec: &[f64], rng: &mut rand::rngs::ThreadRng) -> usize {
    let exps: Vec<f64> = vec.iter().map(|&x| x.exp()).collect();
    let sum: f64 = exps.iter().sum();

    let dist = WeightedIndex::new(exps.iter().map(|&x| x / sum).collect::<Vec<_>>()).unwrap();
    dist.sample(rng)
}

#[derive(Debug)]
struct _NodeInfo {
    nid: u8,
    addr: SocketAddr,
    clc: u64,
    llc: u64,
}
