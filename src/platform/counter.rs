//! A 32-bit counter that wraps, extended to 64 bits. macOS's host CPU tick
//! counters are `natural_t` and wrap (about every 50 days on ten cores); the
//! host percentage is a difference of two readings, so a wrap must not
//! become a bogus sample. Pure, so it is tested on every OS.

/// Feed it each raw reading; it answers the running 64-bit total.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct Extend32 {
    last: Option<u32>,
    total: u64,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Extend32 {
    pub(crate) const fn new() -> Extend32 {
        Extend32 { last: None, total: 0 }
    }

    /// The total after this reading: the first reading starts it, each later
    /// one adds the (wrapping) difference from the one before.
    pub(crate) fn feed(&mut self, raw: u32) -> u64 {
        match self.last {
            None => self.total = u64::from(raw),
            Some(last) => self.total += u64::from(raw.wrapping_sub(last)),
        }
        self.last = Some(raw);
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_counter_that_wraps_keeps_counting() {
        let mut c = Extend32::new();
        assert_eq!(c.feed(100), 100, "the first reading starts the total");
        assert_eq!(c.feed(150), 150);
        assert_eq!(c.feed(150), 150, "no ticks passed");
        let near_wrap = u32::MAX - 20;
        let before = c.feed(near_wrap);
        assert_eq!(before, u64::from(near_wrap));
        // 21 ticks to the wrap, then 9 more: 30 in all.
        assert_eq!(c.feed(9), before + 30);
    }
}
