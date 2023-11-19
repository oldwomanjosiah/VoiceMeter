#[derive(Debug, Clone)]
pub struct CircularBuffer<T> {
    /// First valid value
    start: usize,

    /// First invalid value, greater than `capacity` when buffer is wrapped around end.
    end_exclusive: usize,

    /// Backing buffer
    ///
    /// valid from `[start, end % capacity)`
    delegate: Vec<T>,
}

impl<T> CircularBuffer<T> {
    pub fn new(capacity: usize) -> Self where T: Default {
        let mut delegate = Vec::with_capacity(capacity);
        delegate.resize_with(capacity, Default::default);

        Self {
            start: 0,
            end_exclusive: 0,
            delegate,
        }
    }

    pub fn capacity(&self) -> usize {
        self.delegate.capacity()
    }

    pub fn push(&mut self, new: T) {
        let next_idx = self.end_exclusive % self.capacity();
        if self.start == next_idx {
            self.start = (self.start + 1) % self.capacity();
        }

        self.delegate[next_idx] = new;
        self.end_exclusive += 1;
    }

    pub fn segments(&self) -> [&[T]; 2] {
        // It's unclear to me whether this gets us the correct information
        // let wrapped_end = (self.end_exclusive - 1) % self.delegate.capacity() + 1;
        let wrapped_end = self.end_exclusive % self.delegate.capacity();

        if wrapped_end > self.start {
            [&self.delegate[self.start..wrapped_end], &[]]
        } else if self.start == 0 && wrapped_end == 0 {
            [&self.delegate[..], &[]]
        } else {
            [&self.delegate[0..wrapped_end], &self.delegate[self.start..self.delegate.capacity()]]
        }
    }

    pub fn iter(&self) -> SegmentsIter<'_, T> {
        let [first, second] = self.segments();
        SegmentsIter { first, second }
    }

    pub fn extend_from(&mut self, buf: impl AsRef<[T]>) where T: Clone {
        for item in buf.as_ref() {
            self.push(item.clone());
        }

        // "fast impl"
        // let slice = buf.as_ref();

        // // Fast-path if we fully saturate buffer
        // if slice.len() >= self.capacity() {
        //     self.start = 0;
        //     self.end_exclusive = self.capacity();

        //     self.delegate = (&slice[slice.len() - self.capacity()..slice.len()]).to_vec();
        //     return;
        // }

        // let end_segment_size = (self.capacity() - self.end_exclusive) % self.capacity();
        
        // if slice.len() <= end_segment_size {
        //     self.delegate[self.end_exclusive .. self.end_exclusive + ]copy_from_slice(slice);

        //     return;
        // }
    }
}

pub struct SegmentsIter<'b, T> {
    first: &'b [T],
    second: &'b [T]
}

impl<'b, T> std::iter::Iterator for SegmentsIter<'b, T> {
    type Item = &'b T;

    fn next(&mut self) -> Option<Self::Item> {
        match (self.first, self.second) {
            ([next, rest @ ..], _) => {
                self.first = rest;
                Some(next)
            }

            ([], [next, rest @ ..]) => {
                self.second = rest;
                Some(next)
            }

            ([], []) => {
                None
            }
        }
    }
}

impl<T> SegmentsIter<'_, T> {
    pub fn len(&self) -> usize {
        self.first.len() + self.second.len()
    }

    pub fn last(self, count: usize) -> Self {
        let Self { first, second } = self;

        if count > self.second.len() {
            let keep_from_first = count - self.second.len();

            if keep_from_first >= self.first.len() { return Self { first, second }; }
            
            let first = &first[first.len() - keep_from_first..first.len()];
            Self { first, second }
        } else {
            let second = &second[second.len() - count..second.len()];
            Self { first: &[], second }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_fill() {
        let mut buffer = CircularBuffer::new(16);

        for i in 0..16 {
            buffer.push(i);
        }

        let expected = (0..16).collect::<Vec<_>>();
        assert_eq!(buffer.delegate, expected.as_slice());
        assert_eq!(buffer.segments(), [expected.as_slice(), &[]]);
    }

    #[test]
    fn overfilled() {
        let mut buffer = CircularBuffer::new(16);

        for i in 0..32 {
            buffer.push(i);
        }

        let expected = (16..32).collect::<Vec<_>>();
        assert_eq!(buffer.delegate, expected.as_slice());
        assert_eq!(buffer.segments(), [expected.as_slice(), &[]]);
    }
}
