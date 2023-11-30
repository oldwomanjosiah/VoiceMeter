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

pub mod managed {
    //! Adapter Manager + Channel Connections which are handled from a background thread so that loading does not block the thread which observes the data.

    use std::{borrow::Cow, sync::Arc};

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    use crate::{connection::buffer::SampleBuffer, SharedSpan};

    use super::{channels, AllConvertable};

    pub type Notifier = Arc<dyn Fn() + Send + Sync + 'static>;

    /// Errors when loading devices
    #[derive(Debug, thiserror::Error)]
    pub enum LoadError {
        /// Getting devices from host
        #[error(transparent)]
        DeviceEnumeration(#[from] cpal::DevicesError),

        /// Configuring input stream for device
        #[error(transparent)]
        StreamConfig(#[from] cpal::DefaultStreamConfigError),

        /// Building the input stream for a device
        #[error(transparent)]
        StreamBuild(#[from] cpal::BuildStreamError),

        /// Starting a stream
        #[error(transparent)]
        Start(#[from] cpal::PlayStreamError),

        /// Manager was stopped before streams could be enumerated
        #[error("manager stopped before streams could be loaded")]
        ManagerStopped,
    }

    enum Message<S> {
        LoadDevices {
            devices: oneshot::Sender<Result<Vec<ManagedConnection<S>>, LoadError>>,
        },
        Close {
            id: usize,
        },
        Clearing {
            ids: Vec<usize>,
        },
    }

    pub struct Manager<S> {
        span: SharedSpan,

        requests: std::sync::mpsc::Sender<Message<S>>,
    }

    struct ManagerData<H, St, D, S> {
        span: SharedSpan,
        host: H,
        requests: std::sync::mpsc::Receiver<Message<S>>,
        delegates: Vec<ConnectionDelegate<D, St>>,
        notifier: Notifier,
        next_id: usize,
        sender: std::sync::mpsc::Sender<Message<S>>,
    }

    impl<H, St, D, S> ManagerData<H, St, D, S> {
        fn mint_id(&mut self) -> usize {
            let out = self.next_id;
            self.next_id += 1;
            out
        }
    }

    impl<H, St, D, S> ManagerData<H, St, D, S>
    where
        H: HostTrait<Device = D>,
        D: DeviceTrait<Stream = St>,
        St: StreamTrait,
        S: AllConvertable + Send + Sync + 'static,
    {
        fn build_split_connection(
            &mut self,
            device: D,
        ) -> Result<(ManagedConnection<S>, ConnectionDelegate<D, St>), LoadError> {
            let default_config = device.default_input_config()?;
            let config = default_config.config();
            let format = default_config.sample_format();
            let name = device
                .name()
                .map(Cow::Owned)
                .unwrap_or(Cow::Borrowed("<unknown>"));
            let channels = default_config.channels();
            let sample_rate = config.sample_rate;
            let span: SharedSpan = tracing::info_span!("Device", %name).into();

            let (dtx, drx) = tb(64, channels as _);

            let stream = super::build_input_stream_converting::<D, S>(
                &device,
                &config,
                format,
                {
                    let span = span.clone();
                    let notifier = self.notifier.clone();

                    move |interleaved, _| {
                        use thingbuf::mpsc::errors::TrySendError as TSE;

                        let _gurad = span.enter();

                        let len = interleaved.len();
                        let samples = len / channels as usize;

                        match dtx.try_send_ref() {
                            Ok(mut sender) => {
                                tracing::debug!("Deinterleaving {samples} from a buffer of length {len} and sending");
                                sender.extend_from_interleaved(interleaved);
                                notifier();
                            }

                            Err(TSE::Closed(_)) => {
                                // Do Nothing, channel is shutting down
                            }

                            Err(TSE::Full(_)) => {
                                tracing::warn!("Dropping {samples} samples due to full channel");
                            }

                            Err(err) => {
                                tracing::warn!(%err, "Unknown ThingBuf Error Encountered");
                            }
                        }
                    }
                },
                {
                    let span = span.clone();
                    move |error| {
                        let _guard = span.enter();
                        tracing::info!(%error, "Got Error From Stream")
                    }
                },
                None,
            )?;

            let id = self.mint_id();

            let buffers = std::iter::repeat_with(|| SampleBuffer::new(sample_rate))
                .take(channels as _)
                .collect();

            let conn = ManagedConnection {
                id,
                name,
                span: span.clone(),
                ingest: drx,
                buffers,
                on_drop: self.sender.clone(),
            };

            let delegate = ConnectionDelegate {
                id,
                device,
                stream,
                span,
            };

            Ok((conn, delegate))
        }

        fn handle_load_streams(&mut self) -> Result<Vec<ManagedConnection<S>>, LoadError> {
            let devices = self.host.input_devices()?;

            let mut out = Vec::new();

            for device in devices {
                let (conn, delegate) = self.build_split_connection(device)?;

                {
                    let _guard = conn.span.enter();
                    tracing::debug!("Starting Stream");
                    delegate.stream.play()?;
                }

                out.push(conn);
                self.delegates.push(delegate);
            }

            Ok(out)
        }

        fn remove_delegate_id(&mut self, id: usize) -> Option<ConnectionDelegate<D, St>> {
            let Some(index) = self
                .delegates
                .iter()
                .enumerate()
                .find_map(|(idx, it)| (it.id == id).then(|| idx))
            else {
                tracing::debug!("Stream with id {id} was already dropped");
                return None;
            };

            Some(self.delegates.swap_remove(index))
        }

        fn handle_drop_once(&mut self, id: usize) {
            let Some(delegate) = self.remove_delegate_id(id) else {
                return;
            };

            {
                let _guard = delegate.span.enter();

                tracing::debug!("Ending Stream");
                if let Err(_) = delegate.stream.pause() {
                    tracing::warn!("Could not end stream");
                }
            }

            drop(delegate);
        }

        fn handle_drop_multiple(&mut self, ids: Vec<usize>) {
            let mut to_drop = Vec::with_capacity(ids.len());

            to_drop.extend(
                ids.into_iter()
                    .filter_map(|id| self.remove_delegate_id(id))
                    .inspect(|delegate| {
                        let _guard = delegate.span.enter();

                        tracing::debug!("Ending Stream");
                        if let Err(_) = delegate.stream.pause() {
                            tracing::warn!("Could not end stream");
                        }
                    }),
            );

            drop(to_drop);
        }

        fn run(mut self) {
            let _span = self.span.clone();
            let _guard = _span.enter();

            while let Ok(message) = self.requests.recv() {
                match message {
                    Message::LoadDevices { devices } => {
                        match devices.send(self.handle_load_streams()) {
                            Ok(()) => {
                                (self.notifier)();
                            }
                            Err(err) => {
                                if let Ok(data) = err.into_inner() {
                                    self.handle_drop_multiple(
                                        data.into_iter().map(|it| it.id).collect(),
                                    );
                                }
                            }
                        }
                    }
                    Message::Close { id } => {
                        self.handle_drop_once(id);
                    }
                    Message::Clearing { ids } => {
                        self.handle_drop_multiple(ids);
                    }
                }
            }

            tracing::info!("Message Sender Dropped, Exiting");

            tracing::info!("Closing All Open Channels");
            self.handle_drop_multiple(self.delegates.iter().map(|it| it.id).collect());

            tracing::info!("Goodbye");
        }
    }

    impl<S: AllConvertable + Send + Sync + 'static> Manager<S> {
        pub fn init<H: HostTrait + Send + 'static>(
            host: H,
            name: Option<&str>,
            notifier: Notifier,
        ) -> Self {
            let (rtx, rrx) = std::sync::mpsc::channel();
            let span: SharedSpan = name
                .map(|name| tracing::info_span!("Manager", %name))
                .into();

            std::thread::spawn({
                let span = span.clone();
                let sender = rtx.clone();

                move || {
                    let manager_data = ManagerData {
                        span,
                        host,
                        requests: rrx,
                        delegates: Default::default(),
                        notifier,
                        next_id: 0,
                        sender,
                    };

                    manager_data.run();
                }
            });

            Manager {
                span,
                requests: rtx,
            }
        }
    }

    pub struct LoadingDevices<S> {
        delegate: Option<oneshot::Receiver<Result<Vec<ManagedConnection<S>>, LoadError>>>,
        data: Option<Result<Vec<ManagedConnection<S>>, LoadError>>,
    }

    impl<S> Manager<S> {
        pub fn load(&self) -> LoadingDevices<S> {
            let _guard = self.span.enter();

            let (tx, rx) = oneshot::channel();

            if let Err(e) = self.requests.send(Message::LoadDevices { devices: tx }) {
                tracing::warn!("Could not load new devices, thread closed");

                return LoadingDevices {
                    delegate: None,
                    data: Some(Err(LoadError::ManagerStopped)),
                };
            }

            LoadingDevices {
                delegate: Some(rx),
                data: None,
            }
        }
    }

    impl<S> LoadingDevices<S> {
        fn try_recv(&mut self) -> bool {
            if self.data.is_some() {
                return true;
            }

            if let Some(delegate) = self.delegate.take() {
                if let Ok(data) = delegate.try_recv() {
                    self.data = Some(data);
                    true
                } else {
                    self.delegate = Some(delegate);
                    false
                }
            } else {
                false
            }
        }

        pub fn ready(&mut self) -> bool {
            self.data.is_some() || self.try_recv()
        }

        pub fn get(mut self) -> Result<Vec<ManagedConnection<S>>, LoadError> {
            self.take()
        }

        pub fn take(&mut self) -> Result<Vec<ManagedConnection<S>>, LoadError> {
            self.try_recv();
            self.data.take().unwrap()
        }
    }

    struct ConnectionDelegate<D, St> {
        id: usize,
        device: D,
        stream: St,
        span: SharedSpan,
    }

    fn tb<S>(size: usize, channels: usize) -> (TbTx<S>, TbRx<S>) {
        let recycler = crate::connection::channels::ChannelInfoRecycler {
            channels,
            default_sample_count: 128,
        };

        thingbuf::mpsc::blocking::with_recycle(size, recycler)
    }
    type TbTx<S> = thingbuf::mpsc::blocking::Sender<
        crate::connection::channels::ChannelInfo<S>,
        crate::connection::channels::ChannelInfoRecycler,
    >;

    type TbRx<S> = thingbuf::mpsc::blocking::Receiver<
        crate::connection::channels::ChannelInfo<S>,
        crate::connection::channels::ChannelInfoRecycler,
    >;

    pub struct ManagedConnection<S> {
        id: usize,
        name: Cow<'static, str>,
        span: SharedSpan,
        on_drop: std::sync::mpsc::Sender<Message<S>>,

        ingest: TbRx<S>,
        buffers: Vec<crate::connection::buffer::SampleBuffer<S>>,
    }

    impl<S> ManagedConnection<S> {
        #[inline]
        pub fn id(&self) -> usize {
            self.id
        }

        #[inline]
        pub fn name(&self) -> &str {
            &self.name
        }

        #[inline]
        pub fn channels(&self) -> usize {
            self.buffers.len()
        }

        #[inline]
        pub fn buffers(&mut self) -> &mut [SampleBuffer<S>] {
            &mut self.buffers
        }
    }

    impl<S: Clone> ManagedConnection<S> {
        pub fn process(&mut self) {
            while let Ok(mut data) = self.ingest.try_recv_ref() {
                assert_eq!(data.channels.len(), self.buffers.len());

                for (buffer, channel) in self.buffers.iter_mut().zip(&mut data.channels) {
                    buffer.ingest(channel);
                }
            }
        }
    }

    impl<S> Drop for ManagedConnection<S> {
        fn drop(&mut self) {
            if let Err(_) = self.on_drop.send(Message::Close { id: self.id }) {
                let _guard = self.span.enter();
                tracing::warn!("Could not inform manager of drop");
            }
        }
    }
}

pub struct ChannelConnection<S: StreamTrait, I> {
    stream: S,

    name: Cow<'static, str>,
    span: Option<Arc<tracing::Span>>,
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
            {
                let span = span.clone();
                move |error| {
                    let _guard = span.as_ref().map(|it| it.enter());
                    tracing::error!(%error, "Got Stream Error");
                }
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
            span,
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
        if let Err(e) = self.stream.pause() {
            let _guard = self.span.as_ref().map(|it| it.enter());
            tracing::error!("Could not close stream due to {e}");
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
