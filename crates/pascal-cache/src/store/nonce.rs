//! Snapshot IDs and builder tokens (spec §6.1 `Nonce`). They must be
//! unique, not secret: a colliding ID fails its insert, and a colliding
//! token only weakens fencing between two builders, because every write is
//! also conditional on the build revision and sealed digests are verified
//! on every read. No randomness dependency is needed (Decision 7).

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const NONCE_CONTEXT: &str = "lint4d persistent analysis cache 2026-10 nonce v1";

pub(crate) fn nonce() -> [u8; 16] {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Seeded per process by the standard library's own entropy.
    let seed = RandomState::new().hash_one((now, count));
    let mut hasher = blake3::Hasher::new_derive_key(NONCE_CONTEXT);
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&now.to_le_bytes());
    hasher.update(&count.to_le_bytes());
    hasher.update(&seed.to_le_bytes());
    let mut nonce = [0; 16];
    nonce.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    nonce
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn nonces_are_unique_across_threads() {
        let all: Vec<[u8; 16]> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..4)
                .map(|_| scope.spawn(|| (0..10_000).map(|_| nonce()).collect::<Vec<_>>()))
                .collect();
            workers
                .into_iter()
                .flat_map(|worker| worker.join().unwrap())
                .collect()
        });
        assert_eq!(all.iter().collect::<HashSet<_>>().len(), all.len());
    }
}
