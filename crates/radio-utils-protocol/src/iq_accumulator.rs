use std::collections::VecDeque;

use num_complex::Complex;

/// Accumulates IQ samples into fixed-size DSP blocks.
///
/// Samples are pushed in variable-sized chunks and consumed one block at a time.
/// If the internal buffer grows beyond `block_size * 16` samples the oldest
/// excess samples are silently dropped with a `log::warn!` message.
pub struct IqAccumulator {
    buffer: VecDeque<Complex<f64>>,
    block_size: usize,
}

impl IqAccumulator {
    /// Create a new accumulator with the given block size.
    pub fn new(block_size: usize) -> Self {
        debug_assert!(block_size > 0, "block_size must be > 0");
        Self {
            buffer: VecDeque::with_capacity(block_size * 4),
            block_size,
        }
    }

    /// Change the block size and clear the buffer.
    pub fn set_block_size(&mut self, block_size: usize) {
        debug_assert!(block_size > 0, "block_size must be > 0");
        self.block_size = block_size;
        self.buffer.clear();
    }

    /// Push samples into the accumulator.
    ///
    /// If the buffer would exceed `block_size * 16` samples, the oldest excess
    /// samples are drained and a warning is logged.
    pub fn push(&mut self, samples: &[Complex<f64>]) {
        self.buffer.extend(samples.iter().copied());
        let high_water = self.block_size * 16;
        if self.buffer.len() > high_water {
            let excess = self.buffer.len() - high_water;
            log::warn!(
                "IqAccumulator overflow: {} samples buffered (cap {}), draining {} excess",
                self.buffer.len(),
                high_water,
                excess
            );
            self.buffer.drain(..excess);
        }
    }

    /// Copy the next complete block into `out[..block_size]` and drain it.
    ///
    /// Returns `true` if a full block was available and copied, `false` if
    /// fewer than `block_size` samples are buffered.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `out.len() < block_size`.
    pub fn next_block(&mut self, out: &mut [Complex<f64>]) -> bool {
        debug_assert!(
            out.len() >= self.block_size,
            "IqAccumulator::next_block: out buffer length {} < block_size {}",
            out.len(),
            self.block_size
        );
        if self.buffer.len() < self.block_size {
            return false;
        }
        let (a, b) = self.buffer.as_slices();
        if a.len() >= self.block_size {
            out[..self.block_size].copy_from_slice(&a[..self.block_size]);
        } else {
            let from_a = a.len();
            out[..from_a].copy_from_slice(a);
            out[from_a..self.block_size].copy_from_slice(&b[..self.block_size - from_a]);
        }
        self.buffer.drain(..self.block_size);
        true
    }

    /// Clear all buffered samples.
    pub fn clear(&mut self) {
        self.buffer.clear();
    }

    /// Return the number of samples currently buffered.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Return `true` if no samples are buffered.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(re: f64) -> Complex<f64> {
        Complex::new(re, 0.0)
    }

    #[test]
    fn yields_complete_blocks() {
        let block_size = 4;
        let mut acc = IqAccumulator::new(block_size);
        let mut out = vec![Complex::default(); block_size];

        // Push 3 samples — not enough for a full block.
        acc.push(&[c(1.0), c(2.0), c(3.0)]);
        assert!(
            !acc.next_block(&mut out),
            "should not yield block with only 3 samples"
        );

        // Push 2 more — now 5 total, enough for exactly one block.
        acc.push(&[c(4.0), c(5.0)]);
        assert!(
            acc.next_block(&mut out),
            "should yield block with 5 samples buffered"
        );
        assert_eq!(out, vec![c(1.0), c(2.0), c(3.0), c(4.0)]);

        // One sample should remain in the buffer.
        assert_eq!(acc.len(), 1);
    }

    #[test]
    fn clear_empties_buffer() {
        let block_size = 4;
        let mut acc = IqAccumulator::new(block_size);
        let mut out = vec![Complex::default(); block_size];

        acc.push(&[c(1.0), c(2.0)]);
        acc.clear();

        assert!(acc.is_empty());
        assert!(!acc.next_block(&mut out));
    }

    #[test]
    fn set_block_size_clears_buffer() {
        let mut acc = IqAccumulator::new(4);

        acc.push(&[c(1.0), c(2.0)]);
        acc.set_block_size(2);

        // Buffer must be empty after the resize.
        assert!(acc.is_empty(), "set_block_size must clear the buffer");

        // Push exactly one new block of 2 samples.
        let mut out = vec![Complex::default(); 2];
        acc.push(&[c(10.0), c(20.0)]);
        assert!(acc.next_block(&mut out));
        assert_eq!(out, vec![c(10.0), c(20.0)]);
    }

    #[test]
    fn overflow_drains_excess() {
        let block_size = 4;
        let mut acc = IqAccumulator::new(block_size);

        let samples: Vec<Complex<f64>> = (0..(block_size * 17) as i64)
            .map(|i| Complex::new(i as f64, 0.0))
            .collect();
        acc.push(&samples);

        assert_eq!(
            acc.len(),
            block_size * 16,
            "overflow should drain to exactly high_water, got {}",
            acc.len()
        );
    }

    #[test]
    fn split_deque_block_copy() {
        // Force the VecDeque to wrap internally so that as_slices() returns two
        // non-empty slices.  Strategy:
        //   1. Push block_size+1 items so the deque has more than one block.
        //   2. Drain one full block — this advances the head pointer so that
        //      subsequent pushes will land in wrapped (second) slice territory.
        //   3. Push more items — they occupy the front of the ring buffer while
        //      the remaining item from step 1 sits in the back half.
        //   4. Call next_block and verify the stitched result is correct.

        let block_size = 4;
        let mut acc = IqAccumulator::new(block_size);
        let mut out = vec![Complex::default(); block_size];

        // Step 1: push block_size + 1 = 5 items.
        acc.push(&[c(1.0), c(2.0), c(3.0), c(4.0), c(5.0)]);

        // Step 2: drain one full block (items 1-4), leaving item 5 in the deque.
        assert!(acc.next_block(&mut out));
        assert_eq!(out, vec![c(1.0), c(2.0), c(3.0), c(4.0)]);
        assert_eq!(acc.len(), 1);

        // Step 3: push block_size - 1 = 3 more items.  Now the deque holds
        // [5, 6, 7, 8] but the ring buffer wraps — item 5 is in the tail slice
        // and items 6-8 are in the head slice.
        acc.push(&[c(6.0), c(7.0), c(8.0)]);
        assert_eq!(acc.len(), 4);

        // Step 4: next_block must correctly stitch both slices.
        assert!(acc.next_block(&mut out));
        assert_eq!(out, vec![c(5.0), c(6.0), c(7.0), c(8.0)]);
        assert_eq!(acc.len(), 0);
    }
}
