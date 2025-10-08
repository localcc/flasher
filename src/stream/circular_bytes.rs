use core::{convert::Infallible, ops::Range};

trait RangeExt {
    fn contains_range(&self, other: &Self) -> bool;
}

impl<T: PartialOrd + Ord> RangeExt for Range<T> {
    fn contains_range(&self, other: &Self) -> bool {
        self.start <= other.start && self.end >= other.end
    }
}

#[derive(Debug)]
struct WrapRange<const S: usize> {
    start: usize,
    count: usize,
}

impl<const S: usize> WrapRange<S> {
    pub const fn new() -> Self {
        WrapRange { start: 0, count: 0 }
    }

    // Inverts the range
    pub const fn invert(&self) -> Self {
        WrapRange {
            start: (self.start + self.count) % S,
            count: S - self.count,
        }
    }

    /// Adds a count to this wrap range start, effectively making it smaller
    pub fn add_front(&mut self, count: usize) {
        let count_to_remove = count.min(self.count);
        self.start = (self.start + count_to_remove) % S;
        self.count -= count_to_remove;
    }

    /// Adds a count to this wrap range end, effectively making it larger
    /// This operation is saturating
    pub fn add_back(&mut self, count: usize) {
        self.count = (self.count + count).min(S);
    }

    /// Subtracts a count from this wrap range end, effectively making it smaller
    pub fn sub_back(&mut self, count: usize) {
        self.count = self.count.saturating_sub(count);
    }

    /// Returns non-overlapping contiguous parts of the WrapRange
    /// The second part might be empty if the range is contiguous
    pub fn parts(&self) -> (Range<usize>, Range<usize>) {
        let first = self.start..(self.start + self.count).min(S);
        if self.start + self.count >= S {
            let second = 0..(self.count - first.len());
            (first, second)
        } else {
            (first, 0..0)
        }
    }

    pub const fn len(&self) -> usize {
        self.count
    }
}

#[derive(Debug)]
pub struct CircularBytes<const S: usize> {
    data: [u8; S],
    initialized_range: WrapRange<S>,
}

impl<const S: usize> CircularBytes<S> {
    pub fn new() -> Self {
        CircularBytes {
            data: [0u8; S],
            initialized_range: WrapRange::new(),
        }
    }

    pub const fn capacity(&self) -> usize {
        S
    }

    pub const fn len(&self) -> usize {
        self.initialized_range.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.initialized_range.len() == 0
    }

    pub const fn clear(&mut self) {
        self.initialized_range = WrapRange::new();
    }

    pub fn as_slices(&self) -> (&[u8], &[u8]) {
        let (p1, p2) = self.initialized_range.parts();
        (&self.data[p1.start..p1.end], &self.data[p2.start..p2.end])
    }

    /// Keeps only the back `len` elements.
    pub fn truncate_front(&mut self, len: usize) {
        self.initialized_range
            .add_front(self.initialized_range.len().saturating_sub(len));
    }

    /// Keeps only the front `len` elements.
    pub fn truncate_back(&mut self, len: usize) {
        self.initialized_range
            .sub_back(self.initialized_range.len().saturating_sub(len));
    }

    pub fn as_mut_slices(&mut self) -> (&mut [u8], &mut [u8]) {
        let (p1, p2) = self.initialized_range.parts();
        debug_assert!(
            p1.start <= self.data.len() && p2.start <= self.data.len(),
            "range out of bounds"
        );
        debug_assert!(!p1.contains_range(&p2), "ranges overlap");

        let ptr = self.data.as_mut_ptr();

        // SAFETY: the ranges are in bounds for the whole array enforced by the internals of this class and the assert above
        // SAFETY: these two slices will not alias as the ranges are always non overlapping for the same reason
        let s1 = unsafe { core::slice::from_raw_parts_mut(ptr.add(p1.start), p1.len()) };
        // SAFETY: same as above
        let s2 = unsafe { core::slice::from_raw_parts_mut(ptr.add(p2.start), p2.len()) };

        (s1, s2)
    }

    pub fn extend_from_slice(&mut self, data: &[u8]) {
        let uninit_range = self.initialized_range.invert();
        let (p1, p2) = uninit_range.parts();

        let mut will_take = data.len().min(uninit_range.len());

        let p1 = p1.start..(p1.start + p1.len().min(will_take));
        will_take -= p1.len();
        let p2 = p2.start..(p2.start + p2.len().min(will_take));

        self.data[p1.start..p1.end].copy_from_slice(&data[..p1.len()]);
        self.data[p2.start..p2.end].copy_from_slice(&data[p1.len()..][..p2.len()]);

        self.initialized_range.add_back(p1.len() + p2.len());
    }

    pub fn fill(&mut self, value: u8) {
        self.data.fill(value);
        self.initialized_range = WrapRange { start: 0, count: S }
    }

    pub fn consume_to(&mut self, slice: &mut [u8]) -> usize {
        let total_take = self.len().min(slice.len());
        let mut take = total_take;

        let (s1, s2) = self.as_slices();
        let s1_take = s1.len().min(take);
        take -= s1_take;
        let s2_take = s2.len().min(take);

        slice[..s1_take].copy_from_slice(&s1[..s1_take]);
        slice[s1_take..][..s2_take].copy_from_slice(&s2[..s2_take]);

        self.truncate_front(self.len() - total_take);

        total_take
    }

    pub fn consume_to_exact(&mut self, slice: &mut [u8]) -> bool {
        if self.len() < slice.len() {
            return false;
        }

        self.consume_to(slice);

        true
    }
}

impl<const S: usize> embedded_io_async::Read for CircularBytes<S> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        Ok(self.consume_to(buf))
    }
}

impl<const S: usize> embedded_io_async::ErrorType for CircularBytes<S> {
    type Error = Infallible;
}
