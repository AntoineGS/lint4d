use super::Store;
use super::keys::ArtifactKeyRef;
use super::outcome::{Bypass, Lookup, Malformed, Miss, Published};
use crate::facts::{FactDecodeError, FactDecoder, encode_facts, encoded_content_bytes};
use crate::{ArtifactHeader, ArtifactWrite, Digest};
use pascal_core::facts::{ArtifactFacts, FactBudget};

impl Store {
    /// Publish `facts` under `key`. Their encoding is charged to the store's
    /// scratch allowance before it exists, and facts over a store limit are
    /// not persisted (spec §6.2, §10).
    pub fn publish_facts(
        &self,
        key: ArtifactKeyRef<'_>,
        facts: &ArtifactFacts,
    ) -> Result<Published, Bypass> {
        let bytes = encoded_content_bytes(facts).map_err(Bypass::TooLarge)?;
        let Some(_scratch) = self.scratch.reserve(bytes as u64) else {
            return Err(Bypass::ScratchExhausted);
        };
        let encoded = encode_facts(facts).map_err(Bypass::TooLarge)?;
        let rows = encoded.rows();
        self.publish_artifact(&ArtifactWrite {
            key,
            supplemental: &encoded.supplemental,
            rows: &rows,
        })
    }

    /// Read, verify and decode the facts published under `key`. Each decoded
    /// fact is charged to `budget` before it is retained; on any failure
    /// `budget` is left as it was. Facts that break an invariant are a miss,
    /// never empty facts.
    pub fn load_facts(
        &self,
        key: ArtifactKeyRef<'_>,
        budget: &mut FactBudget,
    ) -> Result<Lookup<ArtifactFacts>, Bypass> {
        match self.open_artifact(key)? {
            Lookup::Hit(header) => self.load_facts_from(&header, budget),
            Lookup::Miss(miss) => Ok(Lookup::Miss(miss)),
        }
    }

    /// `load_facts` for the artifact validated evidence concluded.
    pub fn load_facts_by_id(
        &self,
        id: Digest,
        budget: &mut FactBudget,
    ) -> Result<Lookup<ArtifactFacts>, Bypass> {
        match self.open_artifact_by_id(id)? {
            Lookup::Hit(header) => self.load_facts_from(&header, budget),
            Lookup::Miss(miss) => Ok(Lookup::Miss(miss)),
        }
    }

    fn load_facts_from(
        &self,
        header: &ArtifactHeader,
        budget: &mut FactBudget,
    ) -> Result<Lookup<ArtifactFacts>, Bypass> {
        let mut trial = *budget;
        let mut reader = self.read_artifact(header);
        let mut decoder = FactDecoder::new(&mut trial);
        loop {
            match reader.next_batch()? {
                Lookup::Hit(Some(batch)) => {
                    if let Err(error) = decoder.push(batch.rows()) {
                        return refused(error);
                    }
                }
                Lookup::Hit(None) => break,
                Lookup::Miss(miss) => return Ok(Lookup::Miss(miss)),
            }
        }
        let Some(supplemental) = reader.supplemental() else {
            return Ok(Lookup::Miss(Miss::ChecksumMismatch));
        };
        let facts = match decoder.finish(supplemental) {
            Ok(facts) => facts,
            Err(error) => return refused(error),
        };
        *budget = trial;
        Ok(Lookup::Hit(facts))
    }
}

fn refused(error: FactDecodeError) -> Result<Lookup<ArtifactFacts>, Bypass> {
    match error {
        FactDecodeError::Budget => Err(Bypass::FactBudget),
        FactDecodeError::Malformed(_) => Ok(Lookup::Miss(Miss::Malformed(Malformed::Facts))),
    }
}
