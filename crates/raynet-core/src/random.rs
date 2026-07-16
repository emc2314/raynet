use blake3::OutputReader;

pub(crate) struct RandomStream {
    reader: OutputReader,
}

impl RandomStream {
    pub(crate) fn new(seed: [u8; 16]) -> Self {
        Self {
            reader: blake3::Hasher::new_keyed(blake3::hash(&seed).as_bytes()).finalize_xof(),
        }
    }

    pub(crate) fn u64(&mut self) -> u64 {
        let mut bytes = [0; 8];
        self.fill(&mut bytes);
        u64::from_le_bytes(bytes)
    }

    pub(crate) fn fill(&mut self, bytes: &mut [u8]) {
        self.reader.fill(bytes);
    }
}
