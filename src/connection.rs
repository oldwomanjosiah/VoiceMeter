use std::{
    borrow::Cow,
    collections::VecDeque,
    sync::{
        mpsc::{Receiver, Sender, SyncSender},
        Arc,
    },
    time::Duration,
};

use cpal::{
    traits::{DeviceTrait, StreamTrait},
    BuildStreamError, FromSample, InputCallbackInfo, Sample, StreamConfig, StreamError,
};

use eyre::{Context, Result};

pub mod channels {
    pub struct ChannelInfo<Sample> {
        pub channels: Vec<Vec<Sample>>,
    }

    pub struct ChannelInfoRecycler {
        pub channels: usize,
        pub default_sample_count: usize,
    }

    impl<Sample> ChannelInfo<Sample> {
        pub fn extend_from_interleaved(
            &mut self,
            interleaved: impl ExactSizeIterator<Item = Sample>,
        ) {
            let channels = self.channels.len();
            let samples = interleaved.len() / channels;
            let hanging = interleaved.len() % channels;
            if hanging != 0 {
                tracing::warn!("Interleaved data has weird number of samples for the number of channels: {hanging} / {channels} channels will have 1 extra sample with current the current configuration");
            }

            for channel in &mut self.channels {
                channel.reserve(samples);
            }

            for (idx, sample) in interleaved.enumerate() {
                self.channels[idx % channels].push(sample);
            }
        }
    }

    impl<Sample> thingbuf::Recycle<ChannelInfo<Sample>> for ChannelInfoRecycler {
        fn new_element(&self) -> ChannelInfo<Sample> {
            let samples_factory = || Vec::with_capacity(self.default_sample_count);
            let channels = std::iter::repeat_with(samples_factory)
                .take(self.channels)
                .collect();

            ChannelInfo { channels }
        }

        fn recycle(&self, element: &mut ChannelInfo<Sample>) {
            for samples in &mut element.channels {
                samples.shrink_to_fit();
                samples.clear();
            }
        }
    }
}

pub mod buffer {
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
}

type Tx<Sample> =
    thingbuf::mpsc::blocking::Sender<channels::ChannelInfo<Sample>, channels::ChannelInfoRecycler>;
type Rx<Sample> = thingbuf::mpsc::blocking::Receiver<
    channels::ChannelInfo<Sample>,
    channels::ChannelInfoRecycler,
>;
type SampleIter<'l, S> = dyn ExactSizeIterator<Item = S> + 'l;

/// Build an input stream, converting it's native sample type to the caller's preferred sample type for their callbacks.
fn build_input_stream_converting<Device, Sample>(
    device: &Device,
    config: &cpal::StreamConfig,
    format: cpal::SampleFormat,
    data_callback: impl FnMut(&mut SampleIter<Sample>, &cpal::InputCallbackInfo) + Send + 'static,
    error_callback: impl FnMut(StreamError) + Send + 'static,
    timeout: Option<Duration>,
) -> Result<Device::Stream, BuildStreamError>
where
    Device: cpal::traits::DeviceTrait,
    Sample: AllConvertable,
{
    fn build_callback<InType: Copy, Sample: FromSample<InType>>(
        mut data_callback: impl FnMut(&mut SampleIter<'_, Sample>, &cpal::InputCallbackInfo),
    ) -> impl FnMut(&[InType], &cpal::InputCallbackInfo) {
        tracing::info!(
            "Wrapping callback for sample type {} in converter from type {}",
            std::any::type_name::<Sample>(),
            std::any::type_name::<InType>()
        );

        struct Iterator<'d, I, O> {
            inner: &'d [I],
            _pd: std::marker::PhantomData<O>,
        }

        impl<I: Copy, O: FromSample<I>> std::iter::Iterator for Iterator<'_, I, O> {
            type Item = O;

            fn next(&mut self) -> Option<Self::Item> {
                match self.inner {
                    [next, rest @ ..] => {
                        self.inner = rest;
                        Some(FromSample::from_sample_(*next))
                    }

                    [] => None,
                }
            }
        }

        impl<I: Copy, O: FromSample<I>> std::iter::ExactSizeIterator for Iterator<'_, I, O> {
            fn len(&self) -> usize {
                self.inner.len()
            }
        }

        move |samples, info| {
            let mut iterator = Iterator {
                inner: samples,
                _pd: Default::default(),
            };
            data_callback(&mut iterator, info)
        }
    }

    /// Build the callbacks for converting between types
    macro_rules! format_switch {
        (build_input_stream($format:ident, $device:ident, $config:ident, $data_builder:expr, $error_callback:ident, $timeout:ident) forall { $($f:ident => $ty:ty),+ $(,)? } fallback |$cap:ident| $with:expr) => {
            match $format {
                $(
                    ::cpal::SampleFormat::$f => ::cpal::traits::DeviceTrait::build_input_stream::<$ty, _, _>(
                        $device,
                        $config,
                        $data_builder,
                        $error_callback,
                        $timeout
                    ),
                )*

                $cap => $with
            }
        };
    }

    format_switch!(
        build_input_stream(
            format,
            device,
            config,
            build_callback(data_callback),
            error_callback,
            timeout
        ) forall {
            I8 => i8,
            I16 => i16,
            I32 => i32,
            I64 => i64,
            U8 => u8,
            U16 => u16,
            U32 => u32,
            U64 => u64,
            F32 => f32,
            F64 => f64,
        } fallback |it| {
            tracing::warn!("Stream format {it:?} is not known");
            Err(cpal::BuildStreamError::StreamConfigNotSupported)
        }
    )
}

pub struct ChannelConnection<S: StreamTrait, I> {
    stream: S,

    name: Cow<'static, str>,
    config: StreamConfig,

    ingest: Rx<I>,
    buffers: Vec<buffer::SampleBuffer<I>>,
}

impl<S: StreamTrait, I> ChannelConnection<S, I>
where
    I: AllConvertable + Send + Sync + 'static,
{
    pub fn build_connection<D, DC>(device: &D, on_new_data: DC, span: bool) -> Result<Self>
    where
        D: cpal::traits::DeviceTrait<Stream = S>,
        DC: Fn() + Send + 'static,
    {
        let supported_config = device.default_input_config()?;
        let config = supported_config.config();
        let channels = supported_config.channels();
        let format = supported_config.sample_format();
        let sample_rate = supported_config.sample_rate();
        let name = device
            .name()
            .map(Cow::Owned)
            .unwrap_or(Cow::Borrowed("<unknown>"));

        let span = span
            .then(|| tracing::info_span!("channel_connection", %name))
            .map(Arc::new);

        let (tx, rx) = thingbuf::mpsc::blocking::with_recycle(
            256,
            channels::ChannelInfoRecycler {
                channels: channels as _,
                default_sample_count: 1,
            },
        );

        tracing::info!("Building stream for {name}");

        let stream = build_input_stream_converting::<_, I>(
            device,
            &config,
            format,
            {
                let span = span.clone();
                move |iter, _info| {
                    let _guard = span.as_ref().map(|it| it.enter());

                    let samples = iter.len() / channels as usize;
                    tracing::trace!(%channels, %samples, "Got data packet");

                    if let Ok(mut tx_ref) = tx.try_send_ref() {
                        tx_ref.extend_from_interleaved(iter);
                        on_new_data();
                    } else {
                        tracing::warn!("Dropping packet because not sendable");
                    }
                }
            },
            move |error| {
                let _guard = span.as_ref().map(|it| it.enter());
                tracing::error!(%error, "Got Stream Error");
            },
            None,
        )?;

        let buffers = std::iter::repeat_with(|| buffer::SampleBuffer::new(sample_rate))
            .take(channels as _)
            .collect();

        Ok(ChannelConnection {
            stream,
            name,
            config,
            ingest: rx,
            buffers,
        })
    }
}

impl<S: StreamTrait, I: Clone> ChannelConnection<S, I> {
    pub fn process(&mut self) {
        while let Ok(mut rx_ref) = self.ingest.try_recv_ref() {
            for (idx, data) in rx_ref.channels.iter_mut().enumerate() {
                self.buffers[idx].ingest(data);
            }
        }
    }
}

impl<S: StreamTrait, I> ChannelConnection<S, I> {
    pub fn play(&mut self) -> Result<()> {
        self.stream.play()?;
        Ok(())
    }

    pub fn pause(&mut self) -> Result<()> {
        self.stream.pause()?;
        Ok(())
    }

    pub fn trim_backbuffers_duration(&mut self, duration: Duration) {
        for buffer in &mut self.buffers {
            buffer.trim_backbuffer_duration(duration);
        }
    }

    pub fn channels(&mut self) -> &mut [buffer::SampleBuffer<I>] {
        &mut self.buffers
    }

    pub fn name(&self) -> &str {
        self.name.as_ref()
    }
}

impl<S: StreamTrait, I> Drop for ChannelConnection<S, I> {
    fn drop(&mut self) {
        if let Err(e) = self.pause() {
            tracing::warn!("Failed to pause stream while dropping: {e}");
        }
    }
}

macro_rules! def_all_convert {
    ($($ty:ty),+ $(,)?) => {
        /// Sample types which may be converted to any other sample type.
        pub trait AllConvertable:
            $(::cpal::FromSample<$ty> +)* std::any::Any {}

        $(impl AllConvertable for $ty {})*
    };
}

def_all_convert![f32, f64, i8, i16, i32, i64, u8, u16, u32, u64];
