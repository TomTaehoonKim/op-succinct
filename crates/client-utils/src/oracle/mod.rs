//! Contains the host <-> client communication utilities.

use std::collections::HashMap;

use crate::BytesHasherBuilder;
use alloy_primitives::{hex, keccak256, FixedBytes};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use itertools::Itertools;
use kona_preimage::{HintWriterClient, PreimageKey, PreimageKeyType, PreimageOracleClient};
use kzg_rs::get_kzg_settings;
use kzg_rs::Blob as KzgRsBlob;
use kzg_rs::Bytes48;
use rkyv::{Archive, Deserialize, Infallible, Serialize};
use sha2::{Digest, Sha256};

/// An in-memory HashMap that will serve as the oracle for the zkVM.
/// Rather than relying on a trusted host for data, the data in this oracle
/// is verified with the `verify()` function, and then is trusted for
/// the remainder of execution.
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
pub struct InMemoryOracle {
    cache: HashMap<[u8; 32], Vec<u8>, BytesHasherBuilder>,
}

impl InMemoryOracle {
    /// Creates a new [InMemoryOracle] from the raw bytes passed into the zkVM.
    /// These values are deserialized using rkyv for zero copy deserialization.
    pub fn from_raw_bytes(input: Vec<u8>) -> Self {
        let archived = unsafe {
            rkyv::archived_root::<HashMap<[u8; 32], Vec<u8>, BytesHasherBuilder>>(&input)
        };
        let deserialized: HashMap<[u8; 32], Vec<u8>, BytesHasherBuilder> =
            archived.deserialize(&mut Infallible).unwrap();

        Self {
            cache: deserialized,
        }
    }
}

#[async_trait]
impl PreimageOracleClient for InMemoryOracle {
    async fn get(&self, key: PreimageKey) -> Result<Vec<u8>> {
        let lookup_key: [u8; 32] = key.into();
        self.cache
            .get(&lookup_key)
            .cloned()
            .ok_or_else(|| anyhow!("Key not found in cache: {}", hex::encode(lookup_key)))
    }

    async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> Result<()> {
        let lookup_key: [u8; 32] = key.into();
        let value = self.cache.get(&lookup_key).ok_or_else(|| {
            anyhow!(
                "Key not found in cache (exact): {}",
                hex::encode(lookup_key)
            )
        })?;
        buf.copy_from_slice(value.as_slice());
        Ok(())
    }
}

#[async_trait]
impl HintWriterClient for InMemoryOracle {
    async fn write(&self, _hint: &str) -> Result<()> {
        Ok(())
    }
}

/// A data structure representing a blob. This data is held in memory for future verification.
/// This is used so that we can aggregate all separate blob elements into a single blob
/// and verify it once, rather than verifying each of the 4096 elements separately.
#[derive(Default)]
struct Blob {
    // TODO: This commitment is currently unused.
    commitment: FixedBytes<48>,
    // 4096 Field elements, each 32 bytes.
    data: FixedBytes<131072>,
    kzg_proof: FixedBytes<48>,
}

impl InMemoryOracle {
    /// Verifies all data in the oracle. Once the function has been called, all data in the
    /// oracle can be trusted for the remainder of execution.
    pub fn verify(&self) -> Result<()> {
        let mut blobs: HashMap<FixedBytes<48>, Blob> = HashMap::new();

        for (key, value) in self.cache.iter() {
            let key: PreimageKey = <[u8; 32] as TryInto<PreimageKey>>::try_into(*key).unwrap();
            match key.key_type() {
                PreimageKeyType::Local => {}
                PreimageKeyType::Keccak256 => {
                    let derived_key =
                        PreimageKey::new(keccak256(value).into(), PreimageKeyType::Keccak256);
                    assert_eq!(key, derived_key, "zkvm keccak constraint failed!");
                }
                PreimageKeyType::GlobalGeneric => {
                    unimplemented!();
                }
                PreimageKeyType::Sha256 => {
                    let derived_key: [u8; 32] = Sha256::digest(value).into();
                    let derived_key = PreimageKey::new(derived_key, PreimageKeyType::Sha256);
                    assert_eq!(key, derived_key, "zkvm sha256 constraint failed!");
                }
                // Aggregate blobs and proofs in memory and verify after loop.
                PreimageKeyType::Blob => {
                    let blob_data_key: [u8; 32] =
                        PreimageKey::new(key.into(), PreimageKeyType::Keccak256).into();

                    if let Some(blob_data) = self.cache.get(&blob_data_key) {
                        let commitment: FixedBytes<48> = blob_data[..48].try_into().unwrap();
                        let element_idx_bytes: [u8; 8] = blob_data[72..].try_into().unwrap();
                        let element_idx: u64 = u64::from_be_bytes(element_idx_bytes);

                        // Blob is stored as one 48 byte element.
                        if element_idx == 4096 {
                            blobs
                                .entry(commitment)
                                .or_default()
                                .kzg_proof
                                .copy_from_slice(value);
                            continue;
                        }

                        // Add the 32 bytes of blob data into the correct spot in the blob.
                        blobs
                            .entry(commitment)
                            .or_default()
                            .data
                            .get_mut((element_idx as usize) << 5..(element_idx as usize + 1) << 5)
                            .map(|slice| {
                                if slice.iter().all(|&byte| byte == 0) {
                                    slice.copy_from_slice(value);
                                    Ok(())
                                } else {
                                    Err(anyhow!("trying to overwrite existing blob data"))
                                }
                            });
                    } else {
                        return Err(anyhow!("blob data not found"));
                    }
                }
                PreimageKeyType::Precompile => {
                    unimplemented!();
                }
            }
        }

        let kzg_settings = get_kzg_settings();
        for (commitment, blob) in blobs.iter() {
            println!("cycle-tracker-report-start: blob-verification");
            kzg_rs::KzgProof::verify_blob_kzg_proof(
                KzgRsBlob::from_slice(&blob.data.0).unwrap(),
                &Bytes48::from_slice(&commitment.0).unwrap(),
                &Bytes48::from_slice(&blob.kzg_proof.0).unwrap(),
                &kzg_settings,
            )
            .map_err(|e| anyhow!("blob verification failed: {:?}", e))?;
            println!("cycle-tracker-report-end: blob-verification");
        }

        Ok(())
    }
}