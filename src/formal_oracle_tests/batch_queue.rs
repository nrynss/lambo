//! `formal/oracle/BatchQueue.json`: Lean `BatchQueue.lengthChunks
//! Cfg.rust` against `length_chunks` (`src/embed/candle.rs`), which exists
//! with feature `embed-candle` only. Without it the file is still loaded and
//! checked, so a malformed or renamed vector fails in every row.

use super::{vectors, Vectors};

/// The model this module replays (`formal/oracle/BatchQueue.json`).
pub(super) const MODEL: &str = "BatchQueue";

#[test]
fn batch_queue() {
    let v = Vectors::load(MODEL, vectors!("BatchQueue"));
    #[cfg(feature = "embed-candle")]
    v.replay("Lambo.BatchQueue.lengthChunks", |lens: &Vec<usize>| {
        crate::embed::candle_formal_pins::length_chunks(lens)
    });
    #[cfg(not(feature = "embed-candle"))]
    {
        let _ = v.inputs::<Vec<usize>>("Lambo.BatchQueue.lengthChunks");
        v.skip(
            "Lambo.BatchQueue.lengthChunks",
            "length_chunks exists with feature embed-candle only",
        );
    }
}
