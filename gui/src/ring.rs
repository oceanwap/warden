//! A bounded FIFO: the GUI keeps the last N events and log lines, never more.

use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub struct Ring<T> {
    items: VecDeque<T>,
    cap: usize,
    /// Items pushed out by newer ones since creation (or `clear`).
    dropped: u64,
}

impl<T> Ring<T> {
    pub fn new(cap: usize) -> Ring<T> {
        Ring { items: VecDeque::new(), cap: cap.max(1), dropped: 0 }
    }

    pub fn push(&mut self, item: T) {
        if self.items.len() == self.cap {
            self.items.pop_front();
            self.dropped += 1;
        }
        self.items.push_back(item);
    }

    pub fn extend(&mut self, items: impl IntoIterator<Item = T>) {
        for i in items {
            self.push(i);
        }
    }

    /// Put `items` (older than everything held) in front, keeping the newest `cap`.
    pub fn prepend(&mut self, items: Vec<T>) {
        let room = self.cap.saturating_sub(self.items.len());
        let skip = items.len().saturating_sub(room);
        self.dropped += skip as u64;
        for item in items.into_iter().skip(skip).rev() {
            self.items.push_front(item);
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn cap(&self) -> usize {
        self.cap
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> + ExactSizeIterator {
        self.items.iter()
    }

    pub fn back(&self) -> Option<&T> {
        self.items.back()
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.dropped = 0;
    }

    /// Take everything out, oldest first.
    pub fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        self.items.drain(..)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_newest_and_counts_the_rest() {
        let mut r = Ring::new(3);
        r.extend(1..=5);
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), [3, 4, 5]);
        assert_eq!((r.len(), r.dropped(), r.cap()), (3, 2, 3));
        r.clear();
        assert!(r.is_empty() && r.dropped() == 0);
    }

    #[test]
    fn prepend_keeps_order_and_bound() {
        let mut r = Ring::new(4);
        r.extend([10, 11]);
        r.prepend(vec![1, 2, 3, 4]);
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), [3, 4, 10, 11]);
        assert_eq!(r.dropped(), 2);
        let mut full = Ring::new(2);
        full.extend([7, 8]);
        full.prepend(vec![1]);
        assert_eq!(full.iter().copied().collect::<Vec<_>>(), [7, 8]);
    }

    #[test]
    fn zero_capacity_means_one() {
        let mut r = Ring::new(0);
        r.extend(["a", "b"]);
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), ["b"]);
    }
}
