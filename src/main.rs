use std::{sync::Arc, time::Duration};

use connection::*;
use eyre::{Context, Report, Result};

mod connection;

struct App<H, S, I> {
    host: H,
    repaint: Option<Arc<dyn Fn() + Send + Sync + 'static>>,
    devices: Vec<ChannelConnection<S, I>>,
}

type HostInputStream<H> =
    <<H as cpal::traits::HostTrait>::Device as cpal::traits::DeviceTrait>::Stream;

impl<H: cpal::traits::HostTrait> App<H, HostInputStream<H>, i32> {
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
                    self.devices.push(conn);
                }
                Err(e) => {
                    tracing::error!("Could not build device! Got {e}");
                }
            }
        }
    }
}

impl<H, S, I> App<H, S, I> {
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

impl<H> eframe::App for App<H, HostInputStream<H>, i32>
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
                connection.process();

                ui.separator();
                ui.heading(connection.name());

                for (idx, channel) in connection.channels().iter_mut().enumerate() {
                    let max = channel
                        .take_duration(frametime)
                        .fold(0, |acc, it| acc.max(it.abs()));
                    channel.trim_backbuffer_duration(Duration::from_millis(500));
                    let buffer_max = channel.backbuffer().fold(0, |acc, it| acc.max(it.abs()));

                    ui.add(
                        egui::ProgressBar::new(max as f32 / i32::MAX as f32)
                            .text(format!("Channel {}", idx + 1)),
                    );
                    ui.add(egui::ProgressBar::new(buffer_max as f32 / i32::MAX as f32));

                    ui.label(format!(
                        "Buffered: back {}ms {}samples / forward {}ms {}samples",
                        channel.backbuffer_duration().as_millis(),
                        channel.backbuffer_len(),
                        channel.buffer_duration().as_millis(),
                        channel.buffer_len()
                    ));

                    ui.add_space(24.0);
                }
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
