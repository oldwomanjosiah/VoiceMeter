use std::{
    borrow::Cow,
    sync::{
        mpsc::{Receiver, Sender, SyncSender},
        Arc,
    },
    time::Duration,
};

use crate::circular_buffer::CircularBuffer;
use cpal::{InputCallbackInfo, traits::StreamTrait};
use eyre::{Context, Result};

pub struct Connection<S, I, E> {
    stream: S,
    name: Cow<'static, str>,
    config: cpal::StreamConfig,
    recv: Receiver<Result<Vec<I>, E>>,
    reuse: SyncSender<Vec<I>>,
    buffer: CircularBuffer<I>,
    errors: Vec<E>,
    duration: Duration,
}

impl<S, I, E> Connection<S, I, E> {
    pub fn process(&mut self) -> usize
    where
        I: Copy,
    {
        let mut recvd = 0;

        while let Ok(item) = self.recv.try_recv() {
            match item {
                Ok(mut buf) => {
                    recvd += buf.len();
                    println!("Buflen: {}", buf.len());
                    self.buffer.extend_from(&buf);
                    buf.clear();
                    if let Err(_) = self.reuse.try_send(buf) {
                        tracing::warn!(name = %self.name, "Could not send buffer for re-use, dropping");
                    }
                }

                Err(e) => {
                    self.errors.push(e);
                }
            }
        }

        dbg!(recvd)
    }

    pub fn name(&self) -> &str {
        self.name.as_ref()
    }

    pub fn buffer(&self) -> &CircularBuffer<I> {
        &self.buffer
    }
}

pub fn start_stream<D: cpal::traits::DeviceTrait>(
    device: D,
    duration: Duration,
    repaint: impl FnMut() + Send + 'static,
) -> Result<Connection<D::Stream, i32, cpal::StreamError>> {
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

    let sample_rate = config.sample_rate.0;
    let buffered_samples = (duration.as_secs_f32() * sample_rate as f32).ceil() as usize;

    let (tx, recv) = std::sync::mpsc::channel();
    let (reuse, get_reused) = std::sync::mpsc::sync_channel(32);

    let span = Arc::new(
        tracing::info_span!("Input Stream Worker", %name, ty = ?supported_config.sample_format()),
    );
    let err_fn = send_err(Some(span.clone()), tx.clone());

    let stream = match supported_config.sample_format() {
        cpal::SampleFormat::I8 => device.build_input_stream::<i8, _, _>(
            &config,
            call_with_each(sample_function(Some(span), tx, get_reused), repaint),
            err_fn,
            None,
        ),

        cpal::SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
            &config,
            call_with_each(sample_function(Some(span), tx, get_reused), repaint),
            err_fn,
            None,
        ),

        cpal::SampleFormat::I32 => device.build_input_stream::<i32, _, _>(
            &config,
            call_with_each(sample_function(Some(span), tx, get_reused), repaint),
            err_fn,
            None,
        ),

        cpal::SampleFormat::I64 => device.build_input_stream::<i64, _, _>(
            &config,
            call_with_each(sample_function(Some(span), tx, get_reused), repaint),
            err_fn,
            None,
        ),

        cpal::SampleFormat::U8 => device.build_input_stream::<u8, _, _>(
            &config,
            call_with_each(sample_function(Some(span), tx, get_reused), repaint),
            err_fn,
            None,
        ),

        cpal::SampleFormat::U16 => device.build_input_stream::<u16, _, _>(
            &config,
            call_with_each(sample_function(Some(span), tx, get_reused), repaint),
            err_fn,
            None,
        ),

        cpal::SampleFormat::U32 => device.build_input_stream::<u32, _, _>(
            &config,
            call_with_each(sample_function(Some(span), tx, get_reused), repaint),
            err_fn,
            None,
        ),

        cpal::SampleFormat::U64 => device.build_input_stream::<u64, _, _>(
            &config,
            call_with_each(sample_function(Some(span), tx, get_reused), repaint),
            err_fn,
            None,
        ),

        cpal::SampleFormat::F32 => device.build_input_stream::<f32, _, _>(
            &config,
            call_with_each(sample_function(Some(span), tx, get_reused), repaint),
            err_fn,
            None,
        ),

        cpal::SampleFormat::F64 => device.build_input_stream::<f64, _, _>(
            &config,
            call_with_each(sample_function(Some(span), tx, get_reused), repaint),
            err_fn,
            None,
        ),

        it => eyre::bail!("Unknown Format! {it:?}"),
    }
    .wrap_err_with(|| {
        format!(
            "building stream for sample type {:?}",
            supported_config.sample_format()
        )
    })?;

    stream.play().wrap_err("starting stream")?;

    let conn = Connection {
        stream,
        name,
        config,
        recv,
        reuse,
        buffer: CircularBuffer::new(buffered_samples),
        errors: Vec::new(),
        duration,
    };

    Ok(conn)
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

pub fn log_err<E: std::fmt::Display>(err: E) {
    tracing::error!(%err, "Got Error in Stream!");
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

#[allow(dead_code)]
pub fn f32s(data: &[f32], _cfg: &InputCallbackInfo) {
    let max = data.iter().fold(0.0f32, |acc, it| acc.max(*it));
    let min = data.iter().fold(f32::MAX, |acc, it| acc.min(*it));
    let len = data.len();
    let non_zero = data.iter().filter(|&&it| it != 0.0).count();

    println!("{min}, {max} ({len}, non zero: {non_zero})");
}

pub fn i8s(data: &[f32], _cfg: &InputCallbackInfo) {
    let i8s: Vec<_> = data
        .into_iter()
        .map(|it| (it * i8::MAX as f32) as i8)
        .collect();
    let min = i8s.iter().copied().min().unwrap_or_default();
    let max = i8s.iter().copied().max().unwrap_or_default();
    let dist = (-min) as u8 + max as u8;
    let len = i8s.len();

    println!("{dist:03}, {min:03}, {max:03} ({len})");
}

pub fn buffer_size_from_min(min: u32) -> u32 {
    let mut base = 128;

    while base < min {
        base *= 2
    }

    base
}
