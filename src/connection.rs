use std::{
    borrow::Cow,
    collections::VecDeque,
    sync::{
        mpsc::{Receiver, Sender, SyncSender},
        Arc,
    },
    time::Duration,
};

use cpal::{ InputCallbackInfo, StreamError};

use eyre::{Context, Result};

pub struct MeteredConnection<S, I, E> {
    #[allow(unused)]
    stream: S,
    name: Cow<'static, str>,
    config: cpal::StreamConfig,

    recv: Receiver<Result<Vec<I>, E>>,
    reuse: SyncSender<Vec<I>>,

    buffer: VecDeque<I>,
    errors: Vec<E>,
}

impl<S, I, E> MeteredConnection<S, I, E> {
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

    pub fn channels_for_frame(&mut self, frame_time: Duration) -> MeteredRef<'_, S, I, E> {
        let mut speeding = false;
        let mut samples =
            (frame_time.as_secs_f64() * (self.config.sample_rate.0 as f64)).ceil() as usize;

        if self.buffered_time() > Duration::from_millis(500) {
            speeding = true;
            samples = samples * 3 / 2;
        }

        let channels = self.config.channels as usize;
        samples *= channels;

        eprintln!("Taking {samples} samples for frametime {}ms (speeding: {speeding:?})", frame_time.as_millis());

        MeteredRef { conn: self, channels, samples }
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

pub struct MeteredRef<'c, S, I, E> {
    conn: &'c mut MeteredConnection<S, I, E>,
    channels: usize,
    samples: usize,
}

impl<S, I, E> Drop for MeteredRef<'_, S, I, E> {
    fn drop(&mut self) {
        self.conn.buffer.drain(0..self.samples.min(self.conn.buffer.len()));
    }
}

impl<S, I, E> MeteredRef<'_, S, I, E> {
    pub fn channel_iter(&self, channel: usize) -> impl Iterator<Item = &'_ I> {
        assert!(channel < self.channels, "index {channel} out of bounds for {} channels", self.channels);

        self.conn.buffer.iter().take(self.samples).skip(channel).step_by(self.channels)
    }

    pub fn channels(&self) -> usize { self.channels }
}

impl<S: cpal::traits::StreamTrait, I: AllConvertable + Send> MeteredConnection<S, I, StreamError> {
    pub fn new<D>(
        device: D,
        repaint: impl FnMut() + Send + 'static,
    ) -> Result<Self>
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
