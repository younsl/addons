use std::collections::VecDeque;

/// Fixed-length history of one value per round.
#[derive(Debug)]
pub struct Series {
    values: VecDeque<u64>,
    capacity: usize,
}

impl Series {
    pub fn new(capacity: usize) -> Self {
        Self {
            values: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, v: u64) {
        if self.values.len() == self.capacity {
            self.values.pop_front();
        }
        self.values.push_back(v);
    }

    #[cfg(test)]
    pub fn last(&self) -> u64 {
        self.values.back().copied().unwrap_or(0)
    }

    pub fn max(&self) -> u64 {
        self.values.iter().copied().max().unwrap_or(0)
    }

    /// The most recent `n` values, oldest first.
    pub fn tail(&self, n: usize) -> Vec<u64> {
        let skip = self.values.len().saturating_sub(n);
        self.values.iter().skip(skip).copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolls_over_and_reports() {
        let mut s = Series::new(3);
        assert_eq!((s.last(), s.max()), (0, 0));
        for v in [5, 1, 9, 2] {
            s.push(v);
        }
        assert_eq!(s.tail(10), [1, 9, 2]);
        assert_eq!(s.tail(2), [9, 2]);
        assert_eq!((s.last(), s.max()), (2, 9));
    }
}
