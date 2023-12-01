//! Adapter Manager + Channel Connections which are handled from a background thread so that loading does not block the thread which observes the data.

use std::{borrow::Cow, sync::Arc};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::AllConvertable;
use crate::{connection::buffer::SampleBuffer, SharedSpan};

/// Notify owner of [`Manager`] when changes are made which it may wish to observe.
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

/// Control messages sent to the manager thread
enum Message<S> {
    LoadDevices {
        devices: oneshot::Sender<Result<Vec<ManagedConnection<S>>, LoadError>>,
    },
    Close {
        id: usize,
    },
    // Clearing {
    //     ids: Vec<usize>,
    // },
}

/// Handle to the active manager. Closes all streams when dropped.
pub struct Manager<S> {
    span: SharedSpan,
    requests: std::sync::mpsc::Sender<Message<S>>,
}

/// Internal data used for [`Manager`]'s thread.
/// Handles messages sent by `Manager`, and acts as an
/// owner for all the non-[`Send`] [`Device`][`cpal::traits::DeviceTrait`]s
/// and [`Stream`][`cpal::traits::StreamTrait`]s.
struct ManagerData<H, St, D, S> {
    span: SharedSpan,
    host: H,
    requests: std::sync::mpsc::Receiver<Message<S>>,
    delegates: Vec<ConnectionDelegate<D, St>>,
    notifier: Notifier,
    next_id: usize,
    sender: std::sync::mpsc::Sender<Message<S>>,
}

/// Response from [`Manager::load`]. Acts as a `Future` for the UI thread.
pub struct LoadingDevices<S> {
    delegate: Option<oneshot::Receiver<Result<Vec<ManagedConnection<S>>, LoadError>>>,
    data: Option<Result<Vec<ManagedConnection<S>>, LoadError>>,
}

struct ConnectionDelegate<D, St> {
    id: usize,
    #[allow(unused)]
    device: D,
    stream: St,
    span: SharedSpan,
}

pub struct ManagedConnection<S> {
    id: usize,
    name: Cow<'static, str>,
    span: SharedSpan,
    on_drop: std::sync::mpsc::Sender<Message<S>>,

    ingest: TbRx<S>,
    buffers: Vec<crate::connection::buffer::SampleBuffer<S>>,
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
                            tracing::trace!("Deinterleaving {samples} from a buffer of length {len} and sending");
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
                } // Message::Clearing { ids } => {
                  //     self.handle_drop_multiple(ids);
                  // }
            }
        }

        tracing::info!("Message Sender Dropped, Exiting");

        tracing::info!("Closing All Open Channels");
        self.handle_drop_multiple(self.delegates.iter().map(|it| it.id).collect());

        tracing::info!("Goodbye");
    }
}

impl<S: AllConvertable + Send + Sync + 'static> Manager<S> {
    pub fn new<H: HostTrait + Send + 'static>(
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

impl<S> Manager<S> {
    pub fn load(&self) -> LoadingDevices<S> {
        let _guard = self.span.enter();

        let (tx, rx) = oneshot::channel();

        if let Err(_) = self.requests.send(Message::LoadDevices { devices: tx }) {
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
