use std::{sync::Arc, time::Duration};

use connection::*;
use eyre::{Context, Report, Result};

mod connection;

struct ChannelAnalysis<S> {
    pub connection: ChannelConnection<S, i32>,
    pub buffer_duration: Duration,
    pub smooth_duration: Duration,
    pub decay_rate: f32,
    max_decay: Vec<i32>
}

struct BarInfo {
    jagged: f32,
    smooth: f32,
    decaying: f32
}

impl<S> ChannelAnalysis<S> {
    pub fn new(mut connection: ChannelConnection<S, i32>) -> Self {
        let max_decay = std::iter::repeat(0).take(connection.channels().len()).collect();
        Self { connection, buffer_duration: Duration::from_secs(3), smooth_duration: Duration::from_millis(50), decay_rate: 0.4, max_decay  }
    }

    pub fn process(&mut self, dt: Duration) {
        self.connection.process();

        let decay_factor = self.decay_rate.powf(dt.as_secs_f32());

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
                iter.skip(len.saturating_sub(channel.samples_for_duration(self.smooth_duration))).fold(0, combine)
            };

            let decaying_raw = jagged_raw.max(self.max_decay[idx]);
            self.max_decay[idx] = decaying_raw;

            fn convert(raw: i32) -> f32 {
                raw as f32 / i32::MAX as f32
            }

            let info = BarInfo {
                jagged: convert(jagged_raw),
                smooth: convert(smooth_raw),
                decaying: convert(decaying_raw),
            };

            thunk(idx, info);
        }
    }
}

struct App<H, S> {
    host: H,
    repaint: Option<Arc<dyn Fn() + Send + Sync + 'static>>,
    devices: Vec<ChannelAnalysis<S>>,
}

type HostInputStream<H> =
    <<H as cpal::traits::HostTrait>::Device as cpal::traits::DeviceTrait>::Stream;

impl<H: cpal::traits::HostTrait> App<H, HostInputStream<H>> {
    fn reload_connections(&mut self) {
        let Some(repaint) = self.repaint.as_ref() else {
            tracing::warn!("could not get repainter");
            return;
        };

        let devices = match self.host.devices() {
            Ok(devices) => devices,
            Err(e) => {
                tracing::error!("Could not get device list! {e}");
                return;
            }
        };

        self.devices.clear();

        for device in devices {
            let repaint = {
                let parent = repaint.clone();
                move || parent()
            };

            match connection::ChannelConnection::build_connection(&device, repaint, true) {
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
    }
}

impl<H, S> App<H, S> {
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

            for connection in &mut self.devices {
                connection.process(frametime);

                ui.separator();
                ui.heading(connection.connection.name());

                connection.with_bar_info(frametime, |idx, info| {
                    ui.add(
                        egui::ProgressBar::new(info.jagged)
                            .text(format!("Channel {}", idx + 1)),
                    );
                    ui.add(egui::ProgressBar::new(info.smooth));
                    ui.add(egui::ProgressBar::new(info.decaying));

                    // ui.label(format!(
                    //     "Buffered: back {}ms {}samples / forward {}ms {}samples",
                    //     channel.backbuffer_duration().as_millis(),
                    //     channel.backbuffer_len(),
                    //     channel.buffer_duration().as_millis(),
                    //     channel.buffer_len()
                    // ));

                    ui.add_space(24.0);
                });
            }
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
            let app = App::new(
                cpal::host_from_id(cpal::HostId::Wasapi)
                    .wrap_err("getting host to initalize")
                    .unwrap(),
            );
            Box::new(app)
        }),
    )
    .unwrap();

    Ok(())
}
