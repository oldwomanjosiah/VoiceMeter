use std::{sync::Arc, time::Duration};

use cpal::traits::StreamTrait;
use eyre::{Context, Report, Result};
use voicemeter::connection::{buffer::SampleBuffer, *};

fn log_timing<R>(name: &str, block: impl FnOnce() -> R) -> R {
    let start = std::time::Instant::now();

    let res = block();
    tracing::info!("{name} took {}ms", start.elapsed().as_millis());
    res
}

type FftNum = rustfft::num_complex::Complex32;

struct ChannelAnalysis<S: StreamTrait> {
    pub connection: ChannelConnection<S, i32>,
    pub buffer_duration: Duration,
    pub smooth_duration: Duration,
    pub decay_rate: f32,
    max_decay: Vec<i32>,
}

#[derive(Debug, Clone, Copy)]
struct BarInfo<'o> {
    pub from: &'o SampleBuffer<i32>,
    pub jagged: f32,
    pub smooth: f32,
    pub decaying: f32,
}

impl<S: StreamTrait> ChannelAnalysis<S> {
    pub fn new(mut connection: ChannelConnection<S, i32>) -> Self {
        let max_decay = std::iter::repeat(0)
            .take(connection.channels().len())
            .collect();

        Self {
            connection,
            buffer_duration: Duration::from_secs(3),
            smooth_duration: Duration::from_millis(150),
            decay_rate: 0.4,
            max_decay,
        }
    }

    pub fn process(&mut self, dt: Duration) {
        self.connection.process();

        let decay_factor = self.decay_rate.powf(dt.as_secs_f32() / 2.0);

        for decaying in &mut self.max_decay {
            *decaying = ((*decaying as f32) * decay_factor).floor() as _;
        }
    }

    pub fn with_bar_info(&mut self, frame_time: Duration, mut thunk: impl FnMut(usize, BarInfo)) {
        let combine = |acc: i32, it: &i32| acc.max(it.saturating_abs());

        for (idx, channel) in self.connection.channels().iter_mut().enumerate() {
            let jagged_raw = channel.take_duration(frame_time).fold(0, combine);

            channel.trim_backbuffer_duration(self.buffer_duration);

            let smooth_raw = {
                let iter = channel.backbuffer();
                let len = iter.len();
                iter.skip(len.saturating_sub(channel.samples_for_duration(self.smooth_duration)))
                    .fold(0, combine)
            };

            let decaying_raw = jagged_raw.max(self.max_decay[idx]);
            self.max_decay[idx] = decaying_raw;

            fn convert(raw: i32) -> f32 {
                raw as f32 / i32::MAX as f32
            }

            let info = BarInfo {
                from: channel,
                jagged: convert(jagged_raw),
                smooth: convert(smooth_raw),
                decaying: convert(decaying_raw),
            };

            thunk(idx, info);
        }
    }
}

struct App<H, S: StreamTrait> {
    host: H,
    repaint: Option<Arc<dyn Fn() + Send + Sync + 'static>>,
    devices: Vec<ChannelAnalysis<S>>,
}

type HostInputStream<H> =
    <<H as cpal::traits::HostTrait>::Device as cpal::traits::DeviceTrait>::Stream;

impl<H: cpal::traits::HostTrait> App<H, HostInputStream<H>> {
    fn reload_connections(&mut self) {
        log_timing("reloading connection", || {
            let Some(repaint) = self.repaint.as_ref() else {
                tracing::warn!("could not get repainter");
                return;
            };

            let devices = match log_timing("getting devices", || self.host.devices()) {
                Ok(devices) => devices,
                Err(e) => {
                    tracing::error!("Could not get device list! {e}");
                    return;
                }
            };

            log_timing("clearing devices", || {
                self.devices.clear();
            });

            log_timing("building devices", || {
                for device in devices {
                    let repaint = {
                        let parent = repaint.clone();
                        move || parent()
                    };

                    match voicemeter::connection::ChannelConnection::build_connection(
                        &device, repaint, true,
                    ) {
                        Ok(mut conn) => {
                            if let Err(e) = conn.play() {
                                tracing::warn!("Could not start playing stream: {e}");
                            }
                            self.devices.push(ChannelAnalysis::new(conn));
                        }
                        Err(e) => {
                            tracing::error!("Could not build device! Got {e}");
                        }
                    }
                }
            });
        });
    }
}

impl<H, S: StreamTrait> App<H, S> {
    fn new(host: H) -> Self {
        App {
            host,
            repaint: None,
            devices: Default::default(),
        }
    }

    fn set_repaint_from(&mut self, ctx: &egui::Context) -> bool {
        match self.repaint.as_ref() {
            Some(_) => false,
            None => {
                let ctx = ctx.clone();
                let new_repaint = Arc::new(move || ctx.request_repaint());
                self.repaint = Some(new_repaint);
                true
            }
        }
    }
}

impl<H> eframe::App for App<H, HostInputStream<H>>
where
    H: cpal::traits::HostTrait,
{
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let frametime = Duration::from_secs_f32(ctx.input(|it| it.unstable_dt));

        if self.set_repaint_from(ctx) {
            self.reload_connections();
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("VoiceMeter");

            if ui.button("reload").clicked() {
                self.reload_connections();
            }

            egui::ScrollArea::vertical().show(ui, |ui| {
                for connection in &mut self.devices {
                    connection.process(frametime);

                    ui.separator();
                    ui.label(connection.connection.name());

                    connection.with_bar_info(frametime, |idx, info| {
                        ui.add(
                            voicemeter::ui::horizontal_bar::HorizontalBarWidget::new(
                                info.jagged,
                                info.smooth,
                                info.decaying,
                            )
                            .with_label(format!("{idx}")),
                        );
                    });
                }
            });
        });
    }
}

fn main() -> Result<()> {
    {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_timer(tracing_subscriber::fmt::time::uptime())
                    .with_filter(tracing_subscriber::EnvFilter::from_default_env()),
            )
            .init();

        color_eyre::install()?;
    }

    let options = eframe::NativeOptions {
        vsync: false,
        ..Default::default()
    };

    eframe::run_native(
        "VoiceMeter",
        options,
        Box::new(|_| {
            let app = App::new(log_timing("getting host", || {
                cpal::host_from_id(cpal::HostId::Wasapi)
                    .wrap_err("getting host to initalize")
                    .unwrap()
            }));
            Box::new(app)
        }),
    )
    .unwrap();

    Ok(())
}
