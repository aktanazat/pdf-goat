//! The one tensor the pinned model file carries.
//!
//! A safetensors file is an 8-byte little-endian header length, a JSON header
//! naming each tensor's dtype, shape and byte range, then the tensor bytes.

use serde_json::Value;

use crate::{MODEL_ID, MeaningError};

const TENSOR_NAME: &str = "embeddings";

/// A row-major F32 matrix.
pub(crate) struct Matrix {
    pub(crate) rows: usize,
    pub(crate) cols: usize,
    pub(crate) values: Vec<f32>,
}

fn malformed(detail: &str) -> MeaningError {
    MeaningError::new(format!(
        "{MODEL_ID} model.safetensors is malformed: {detail}"
    ))
}

pub(crate) fn load_matrix(data: &[u8]) -> Result<Matrix, MeaningError> {
    let length_bytes: [u8; 8] = data
        .get(..8)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| malformed("shorter than its header length"))?;
    let header_length = usize::try_from(u64::from_le_bytes(length_bytes))
        .map_err(|_| malformed("header length does not fit in memory"))?;
    let base = header_length
        .checked_add(8)
        .filter(|&base| base <= data.len())
        .ok_or_else(|| malformed("header runs past the end of the file"))?;
    let header: Value = serde_json::from_slice(&data[8..base])
        .map_err(|error| malformed(&format!("header is not JSON: {error}")))?;

    let entry = header
        .get(TENSOR_NAME)
        .ok_or_else(|| MeaningError::new(format!("{MODEL_ID} has no '{TENSOR_NAME}' tensor")))?;
    let dtype = entry.get("dtype").and_then(Value::as_str).unwrap_or("");
    if dtype != "F32" {
        return Err(MeaningError::new(format!(
            "{MODEL_ID} embeddings are {dtype}, expected F32"
        )));
    }
    let numbers = |key: &str| -> Result<Vec<usize>, MeaningError> {
        entry
            .get(key)
            .and_then(Value::as_array)
            .ok_or_else(|| malformed(&format!("'{key}' is not a list")))?
            .iter()
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| malformed(&format!("'{key}' holds a non-count")))
            })
            .collect()
    };
    let shape = numbers("shape")?;
    let [rows, cols] = shape[..] else {
        return Err(malformed("the embedding tensor is not two-dimensional"));
    };
    let offsets = numbers("data_offsets")?;
    let [start, end] = offsets[..] else {
        return Err(malformed("'data_offsets' is not a pair"));
    };
    let count = rows
        .checked_mul(cols)
        .ok_or_else(|| malformed("the tensor shape overflows"))?;
    let byte_count = end
        .checked_sub(start)
        .filter(|&bytes| Some(bytes) == count.checked_mul(4))
        .ok_or_else(|| malformed("the tensor byte range does not match its shape"))?;
    let bytes = base
        .checked_add(start)
        .and_then(|first| data.get(first..first.checked_add(byte_count)?))
        .ok_or_else(|| malformed("the tensor runs past the end of the file"))?;
    let values = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&chunk| f32::from_le_bytes(chunk))
        .collect();
    Ok(Matrix { rows, cols, values })
}
