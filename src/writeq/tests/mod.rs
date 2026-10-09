//! Unit tests for the write queue, grouped by subject.

use super::calibration::{probe_embedder, probe_text_at};
use super::*;
use crate::embed::Embedder;
use crate::surface::limits::MAX_CONCEPTS_PER_DERIVE;
use std::sync::atomic::Ordering;
use std::time::Duration;

mod calibration;
mod receipts;
