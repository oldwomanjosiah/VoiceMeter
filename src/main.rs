use std::{sync::Arc, time::Duration};

use eyre::{Context, Result};
use voicemeter::{
    connection::{
        buffer::SampleBuffer,
        managed::{LoadError, ManagedConnection, Manager},
    },
    ui::{DynamicLce, Lce, LoadableExt},
};

struct ChannelAnalysis {
    pub connection: ManagedConnection<i32>,
    pub buffer_duration: Duration,
    pub smooth_duration: Duration,
    pub decay_rate: f32,
    max_decay: Vec<i32>,
}

#[derive(Debug, Clone, Copy)]
struct BarInfo<'o> {
    #[allow(unused)]
    pub from: &'o SampleBuffer<i32>,
    pub jagged: f32,
    pub smooth: f32,
    pub decaying: f32,
}

impl ChannelAnalysis {
    pub fn new(connection: ManagedConnection<i32>) -> Self {
        let max_decay = std::iter::repeat(0).take(connection.channels()).collect();

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

        for (idx, channel) in self.connection.buffers().iter_mut().enumerate() {
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

struct App {
    manager: Manager<i32>,
    connections: DynamicLce<Vec<ChannelAnalysis>, LoadError>,
}

impl App {
    fn reload_connections(&mut self) {
        let loader = self.manager.load();
        let mapped_loader = loader.map_res(|res| {
            res.map(|connections| {
                connections
                    .into_iter()
                    .map(ChannelAnalysis::new)
                    .collect::<Vec<_>>()
            })
        });

        self.connections = Lce::Loading(mapped_loader).to_dyn_lce();
    }
}

impl App {
    fn new(manager: Manager<i32>) -> Self {
        App {
            manager,
            connections: Default::default(),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let frametime = Duration::from_secs_f32(ctx.input(|it| it.unstable_dt));

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("VoiceMeter");

            if ui.button("reload").clicked() {
                self.reload_connections();
            }

            if matches!(self.connections, Lce::Waiting) {
                self.reload_connections();
            }

            egui::ScrollArea::vertical().show(ui, |ui| {
                self.connections.ui(
                    ui,
                    |ui| {
                        ui.heading("Waiting");
                    },
                    |ui, _l| {
                        ui.horizontal(|ui| {
                            ui.heading("Loading");
                            ui.spinner();
                        });
                    },
                    |ui, c| {
                        for connection in c {
                            connection.process(frametime);

                            ui.separator();
                            ui.label(connection.connection.name());

                            connection.with_bar_info(frametime, |idx, info| {
                                ui.add(
                                    voicemeter::ui::HorizontalBarWidget::new(
                                        info.jagged,
                                        info.smooth,
                                        info.decaying,
                                    )
                                    .with_label(format!("{idx}")),
                                );
                            });
                        }
                    },
                    |ui, e| {
                        ui.heading("Error");
                        ui.code(e.to_string());
                    },
                );
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
        Box::new(|ctx| {
            let host = cpal::host_from_id(cpal::HostId::Wasapi)
                .wrap_err("Getting host")
                .unwrap();

            let notifier = {
                let ctx = ctx.egui_ctx.clone();
                let callback = move || {
                    ctx.request_repaint();
                };
                Arc::new(callback)
            };

            let manager =
                voicemeter::connection::managed::Manager::new(host, Some("Wasapi"), notifier);

            let app = App::new(manager);
            Box::new(app)
        }),
    )
    .unwrap();

    Ok(())
}
