use std::{collections::VecDeque, time::Duration};

pub struct SampleBuffer<Sample> {
    delegate: VecDeque<Sample>,
    sample_rate: u32,

    /// How many samples at the start of [`delegate`] are for the "backbuffer"
    backbuffer: usize,

    /// What is the absolute sample index of the first non-[`backbuffer`] sample within the stream.
    absolute_sample: usize,
}

impl<S> std::fmt::Debug for SampleBuffer<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(&format!("SampleBuffer<{}>", std::any::type_name::<S>()))
            .field("backbuffer.len", &self.backbuffer)
            .field("buffer.len", &(self.delegate.len() - self.backbuffer))
            .field("absolute_sample", &self.absolute_sample)
            .finish()
    }
}

impl<S> SampleBuffer<S> {
    pub fn new(sample_rate: cpal::SampleRate) -> Self {
        Self {
            delegate: Default::default(),
            sample_rate: sample_rate.0,
            backbuffer: 0,
            absolute_sample: 0,
        }
    }

    #[inline]
    pub fn backbuffer_len(&self) -> usize {
        self.backbuffer
    }

    #[inline]
    #[track_caller]
    pub fn backbuffer_duration(&self) -> Duration {
        self.duration_for_samples(self.backbuffer_len().try_into().expect("too many samples"))
    }

    #[inline]
    pub fn buffer_len(&self) -> usize {
        self.delegate.len() - self.backbuffer
    }

    #[inline]
    #[track_caller]
    pub fn buffer_duration(&self) -> Duration {
        self.duration_for_samples(self.buffer_len().try_into().expect("too many samples"))
    }

    pub fn ingest(&mut self, samples: impl AsRef<[S]>)
    where
        S: Clone,
    {
        self.delegate.extend(samples.as_ref().iter().cloned());
    }

    pub fn samples_for_duration(&self, duration: Duration) -> usize {
        (duration.as_secs_f64() * self.sample_rate as f64).ceil() as _
    }

    pub fn duration_for_samples(&self, samples: u32) -> Duration {
        Duration::from_secs(1) * samples / self.sample_rate
    }

    pub fn take(&mut self, samples: usize) -> impl ExactSizeIterator<Item = &'_ S> + '_ {
        let start_idx = self.backbuffer;
        let end_idx = self.delegate.len().min(self.backbuffer + samples);

        let taken_len = end_idx - start_idx;

        self.absolute_sample += taken_len;
        self.backbuffer += taken_len;

        self.delegate.range(start_idx..end_idx)
    }

    pub fn take_duration(
        &mut self,
        duration: Duration,
    ) -> impl ExactSizeIterator<Item = &'_ S> + '_ {
        self.take(self.samples_for_duration(duration))
    }

    /// Trim the stored backbuffer to at most `samples` in length
    pub fn trim_backbuffer(&mut self, samples: usize) {
        let samples_to_trim = self.backbuffer.saturating_sub(samples);
        drop(self.delegate.drain(0..samples_to_trim));
        self.backbuffer -= samples_to_trim;
    }

    /// Trim the stored backbuffer to at most `duration` in length
    pub fn trim_backbuffer_duration(&mut self, duration: Duration) {
        self.trim_backbuffer(self.samples_for_duration(duration))
    }

    /// Get the current backbuffered data
    pub fn backbuffer(&self) -> impl ExactSizeIterator<Item = &'_ S> + '_ {
        self.delegate.range(0..self.backbuffer)
    }
}

#[cfg(test)]
mod tests {
    use cpal::SampleRate;

    use super::*;

    #[test]
    fn constructable() {
        SampleBuffer::<i32>::new(SampleRate(1));
    }

    #[test]
    fn buffer_samples() {
        let mut buffer = SampleBuffer::<i32>::new(SampleRate(1));
        let data = std::array::from_fn::<_, 32, _>(|idx| idx as _);
        buffer.ingest(&data);

        assert_eq!(buffer.backbuffer().copied().collect::<Vec<_>>(), []);

        let new_samples = buffer.take(32).copied().collect::<Vec<_>>();
        assert_eq!(new_samples, &data);

        assert_eq!(buffer.backbuffer().copied().collect::<Vec<_>>(), &data);

        buffer.trim_backbuffer(16);
        assert_eq!(
            buffer.backbuffer().copied().collect::<Vec<_>>(),
            &data[16..32]
        );
    }
}
