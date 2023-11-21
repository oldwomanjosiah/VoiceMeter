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
    traits::{DeviceTrait, StreamTrait}, BuildStreamError, FromSample, InputCallbackInfo, Sample, StreamConfig,
    StreamError,
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
            interleaved: impl ExactSizeIterator<Item = Sample>
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
            let buffer = SampleBuffer::<i32>::new(SampleRate(1));
        }

        fn buffer_samples() {
            let mut buffer = SampleBuffer::<i32>::new(SampleRate(1));
            buffer.ingest(std::array::from_fn::<_, 32, _>(|idx| idx as _));
        }
    }
}

type InitialTimestamp = Arc<std::sync::OnceLock<cpal::InputStreamTimestamp>>;
type Tx<Sample> =
    thingbuf::mpsc::blocking::Sender<channels::ChannelInfo<Sample>, channels::ChannelInfoRecycler>;
type Rx<Sample> = thingbuf::mpsc::blocking::Receiver<
    channels::ChannelInfo<Sample>,
    channels::ChannelInfoRecycler,
>;
type SampleIter<'l, S> = dyn ExactSizeIterator<Item = S> + 'l;

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
        tracing::info!("Wrapping callback for sample type {} in converter from type {}", std::any::type_name::<Sample>(), std::any::type_name::<InType>());

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

    use cpal::SampleFormat as SF;

    match format {
        SF::I8 => device.build_input_stream::<i8, _, _>(
            config,
            build_callback(data_callback),
            error_callback,
            timeout,
        ),

        SF::I16 => device.build_input_stream::<i16, _, _>(
            config,
            build_callback(data_callback),
            error_callback,
            timeout,
        ),

        SF::I32 => device.build_input_stream::<i32, _, _>(
            config,
            build_callback(data_callback),
            error_callback,
            timeout,
        ),

        SF::I64 => device.build_input_stream::<i64, _, _>(
            config,
            build_callback(data_callback),
            error_callback,
            timeout,
        ),

        SF::U8 => device.build_input_stream::<u8, _, _>(
            config,
            build_callback(data_callback),
            error_callback,
            timeout,
        ),

        SF::U16 => device.build_input_stream::<u16, _, _>(
            config,
            build_callback(data_callback),
            error_callback,
            timeout,
        ),

        SF::U32 => device.build_input_stream::<u32, _, _>(
            config,
            build_callback(data_callback),
            error_callback,
            timeout,
        ),

        SF::U64 => device.build_input_stream::<u64, _, _>(
            config,
            build_callback(data_callback),
            error_callback,
            timeout,
        ),

        SF::F32 => device.build_input_stream::<f32, _, _>(
            config,
            build_callback(data_callback),
            error_callback,
            timeout,
        ),

        SF::F64 => device.build_input_stream::<f64, _, _>(
            config,
            build_callback(data_callback),
            error_callback,
            timeout,
        ),

        it => {
            tracing::warn!("Stream format {it:?} is not known");
            Err(cpal::BuildStreamError::StreamConfigNotSupported)
        }
    }
}

pub struct ChannelConnection<S, I> {
    #[allow(unused)]
    stream: S,

    name: Cow<'static, str>,
    config: StreamConfig,

    ingest: Rx<I>,
    buffers: Vec<buffer::SampleBuffer<I>>,
}

impl<S, I> ChannelConnection<S, I>
where
    I: AllConvertable + Send + Sync + 'static,
{
    pub fn build_connection<D, DC>(device: &D, on_new_data: DC, span: bool) -> Result<Self>
    where
        D: cpal::traits::DeviceTrait<Stream = S>,
        DC: Fn() + Send + 'static
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

impl<S, I: Clone> ChannelConnection<S, I> {
    pub fn process(&mut self) {
        while let Ok(mut rx_ref) = self.ingest.try_recv_ref() {
            for (idx, data) in rx_ref.channels.iter_mut().enumerate() {
                self.buffers[idx].ingest(data);
            }
        }
    }
}

impl<S: cpal::traits::StreamTrait, I> ChannelConnection<S, I> {
    pub fn play(&mut self) -> Result<()> {
        self.stream.play()?;
        Ok(())
    }

    pub fn pause(&mut self) -> Result<()> {
        self.stream.pause()?;
        Ok(())
    }
}

impl<S, I> ChannelConnection<S, I> {
    pub fn trim_backbuffers_duration(&mut self, duration: Duration) {
        for buffer in &mut self.buffers {
            buffer.trim_backbuffer_duration(duration);
        }
    }

    pub fn channels(&mut self) -> &mut [buffer::SampleBuffer<I>] {
        &mut self.buffers
    }

    pub fn name(&self) -> &str { self.name.as_ref() }
}

pub struct MeteredConnection<S, I> {
    #[allow(unused)]
    stream: S,
    name: Cow<'static, str>,
    config: cpal::StreamConfig,

    recv: Receiver<Result<Vec<I>, StreamError>>,
    reuse: SyncSender<Vec<I>>,

    buffer: VecDeque<I>,
    errors: Vec<StreamError>,
}

impl<S, I> MeteredConnection<S, I> {
    pub fn process(&mut self)
    where
        I: Copy,
    {
        let mut samples = 0;
        while let Ok(next) = self.recv.try_recv() {
            match next {
                Ok(mut buffer) => {
                    self.buffer.extend(buffer.iter().copied());
                    samples += buffer.len();
                    buffer.clear();
                    self.reuse.try_send(buffer).ok();
                }

                Err(e) => self.errors.push(e),
            }
        }

        eprintln!("Got {samples} samples");
    }

    pub fn channels_for_frame(&mut self, frame_time: Duration) -> MeteredRef<'_, S, I> {
        let mut speeding = false;
        let mut samples =
            (frame_time.as_secs_f64() * (self.config.sample_rate.0 as f64)).ceil() as usize;

        if self.buffered_time() > Duration::from_millis(500) {
            speeding = true;
            samples = samples * 3 / 2;
        }

        let channels = self.config.channels as usize;
        samples *= channels;

        eprintln!(
            "Taking {samples} samples for frametime {}ms (speeding: {speeding:?})",
            frame_time.as_millis()
        );

        MeteredRef {
            conn: self,
            channels,
            samples,
        }
    }

    pub fn buffered_time(&self) -> Duration {
        let samples = self.buffer.len() as _;
        let channels = self.config.channels as _;
        let rate = self.config.sample_rate.0 as _;

        Duration::from_secs(1) * samples / channels / rate
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

pub struct MeteredRef<'c, S, I> {
    conn: &'c mut MeteredConnection<S, I>,
    channels: usize,
    samples: usize,
}

impl<S, I> Drop for MeteredRef<'_, S, I> {
    fn drop(&mut self) {
        self.conn
            .buffer
            .drain(0..self.samples.min(self.conn.buffer.len()));
    }
}

impl<S, I> MeteredRef<'_, S, I> {
    pub fn channel_iter(&self, channel: usize) -> impl Iterator<Item = &'_ I> {
        assert!(
            channel < self.channels,
            "index {channel} out of bounds for {} channels",
            self.channels
        );

        self.conn
            .buffer
            .iter()
            .take(self.samples) // Total number for all channels
            .skip(channel) // Offset into iter by channel number
            .step_by(self.channels) // Skip along by the total number of channels, so we only get the data for this channel
    }

    pub fn channels(&self) -> usize {
        self.channels
    }
}

impl<S: cpal::traits::StreamTrait, I: AllConvertable + Send> MeteredConnection<S, I> {
    pub fn new<D>(device: D, repaint: impl FnMut() + Send + 'static) -> Result<Self>
    where
        D: cpal::traits::DeviceTrait<Stream = S>,
    {
        let name = device
            .name()
            .map(Cow::Owned)
            .unwrap_or(Cow::Borrowed("<unknown>"));

        let supported_config = device
            .default_input_config()
            .wrap_err("getting default config")?;
        let config = supported_config.config();
        let config = cpal::StreamConfig {
            buffer_size: cpal::BufferSize::Fixed(128),
            ..config
        };

        let (tx, recv) = std::sync::mpsc::channel();
        let (reuse, get_reused) = std::sync::mpsc::sync_channel(32);

        let span = Arc::new(
            tracing::info_span!("Input Stream Worker", %name, ty = ?supported_config.sample_format()),
        );

        let stream = connect_sending(
            &device,
            &supported_config,
            &config,
            Some(span),
            tx,
            get_reused,
            repaint,
        )?;

        stream.play().wrap_err("starting stream")?;

        let conn = MeteredConnection {
            stream,
            name,
            config,
            recv,
            reuse,
            buffer: VecDeque::new(),
            errors: Vec::new(),
        };

        Ok(conn)
    }
}

macro_rules! def_all_convert {
    ($($ty:ty),+ $(,)?) => {
        pub trait AllConvertable:
            $(::cpal::FromSample<$ty> +)* std::any::Any {}

        $(impl AllConvertable for $ty {})*
    };
}

def_all_convert![f32, f64, i8, i16, i32, i64, u8, u16, u32, u64];

/// Connect to device with a given configuration, converting whatever sample format it supports into a common sample format of your choice.
fn connect_sending<
    S: AllConvertable + Send,
    Sp: AsRef<tracing::Span> + Clone + Send + 'static,
    D: cpal::traits::DeviceTrait,
>(
    device: &D,
    supported_config: &cpal::SupportedStreamConfig,
    config: &cpal::StreamConfig,
    span: Option<Sp>,
    sender: Sender<Result<Vec<S>, cpal::StreamError>>,
    reuse: Receiver<Vec<S>>,
    repaint: impl FnMut() + Send + 'static,
) -> Result<D::Stream, cpal::BuildStreamError> {
    let err_fn = send_err(span.clone(), sender.clone());

    match supported_config.sample_format() {
        cpal::SampleFormat::I8 => device.build_input_stream::<i8, _, _>(
            config,
            call_with_each(sample_function(span, sender, reuse), repaint),
            err_fn,
            None,
        ),

        cpal::SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
            config,
            call_with_each(sample_function(span, sender, reuse), repaint),
            err_fn,
            None,
        ),

        cpal::SampleFormat::I32 => device.build_input_stream::<i32, _, _>(
            config,
            call_with_each(sample_function(span, sender, reuse), repaint),
            err_fn,
            None,
        ),

        cpal::SampleFormat::I64 => device.build_input_stream::<i64, _, _>(
            config,
            call_with_each(sample_function(span, sender, reuse), repaint),
            err_fn,
            None,
        ),

        cpal::SampleFormat::U8 => device.build_input_stream::<u8, _, _>(
            config,
            call_with_each(sample_function(span, sender, reuse), repaint),
            err_fn,
            None,
        ),

        cpal::SampleFormat::U16 => device.build_input_stream::<u16, _, _>(
            config,
            call_with_each(sample_function(span, sender, reuse), repaint),
            err_fn,
            None,
        ),

        cpal::SampleFormat::U32 => device.build_input_stream::<u32, _, _>(
            config,
            call_with_each(sample_function(span, sender, reuse), repaint),
            err_fn,
            None,
        ),

        cpal::SampleFormat::U64 => device.build_input_stream::<u64, _, _>(
            config,
            call_with_each(sample_function(span, sender, reuse), repaint),
            err_fn,
            None,
        ),

        cpal::SampleFormat::F32 => device.build_input_stream::<f32, _, _>(
            config,
            call_with_each(sample_function(span, sender, reuse), repaint),
            err_fn,
            None,
        ),

        cpal::SampleFormat::F64 => device.build_input_stream::<f64, _, _>(
            config,
            call_with_each(sample_function(span, sender, reuse), repaint),
            err_fn,
            None,
        ),

        it => {
            tracing::warn!("Stream format {it:?} is not known");
            Err(cpal::BuildStreamError::StreamConfigNotSupported)
        }
    }
}

fn call_with_each<T>(
    mut base: impl FnMut(&[T], &InputCallbackInfo),
    mut on_each: impl FnMut(),
) -> impl FnMut(&[T], &InputCallbackInfo) {
    move |data, info| {
        base(data, info);
        on_each();
    }
}

fn send_err<S, E: std::error::Error, Sp: AsRef<tracing::Span>>(
    span: Option<Sp>,
    sender: Sender<Result<S, E>>,
) -> impl FnMut(E) {
    move |err| {
        let _guard = span.as_ref().map(AsRef::as_ref).map(tracing::Span::enter);

        tracing::error!(%err, "Got Error from stream");

        if let Err(err) = sender.send(Err(err)) {
            tracing::warn!(%err, "Could not send error");
        }
    }
}

fn sample_function<I, T, E, Sp: AsRef<tracing::Span>>(
    span: Option<Sp>,
    sender: Sender<Result<Vec<T>, E>>,
    reuse: Receiver<Vec<T>>,
) -> impl FnMut(&[I], &InputCallbackInfo)
where
    T: cpal::FromSample<I>,
    I: Copy,
{
    move |samples, _info| {
        let _guard = span.as_ref().map(AsRef::as_ref).map(tracing::Span::enter);

        tracing::trace!(count = samples.len(), "Recieved Samples");

        let mut out = match reuse.try_recv().ok() {
            Some(mut reuse) => {
                tracing::trace!("Reusing buffer");
                reuse.clear();
                reuse.reserve(samples.len());
                reuse
            }

            None => {
                tracing::debug!("Building new buffer");
                Vec::with_capacity(samples.len())
            }
        };

        out.extend(samples.iter().copied().map(cpal::FromSample::from_sample_));

        if let Err(e) = sender.send(Ok(out)) {
            tracing::warn!(%e, "Could not send data");
        }
    }
}
