use std::{borrow::Cow, sync::Arc, time::Duration};

use connection::*;
use cpal::traits::*;
use eyre::{bail, Context, Report, Result};

mod circular_buffer;
mod connection;

struct App<H, S, I, E> {
    host: H,
    repaint: Option<Arc<dyn Fn() + Send + Sync + 'static>>,
    devices: Vec<MeteredConnection<S, I, E>>,
    error_log: Vec<eyre::Report>,
    frame: std::time::Instant,
}

type HostInputStream<H> =
    <<H as cpal::traits::HostTrait>::Device as cpal::traits::DeviceTrait>::Stream;

impl<H: cpal::traits::HostTrait> App<H, HostInputStream<H>, i32, cpal::StreamError> {
    fn reload_connections(&mut self) {
        let Some(repaint) = self.repaint.as_ref() else {
            tracing::warn!("could not get repainter");
            return;
        };

        let devices = match self.host.devices() {
            Ok(devices) => devices,
            Err(e) => {
                tracing::warn!("could not get devices");
                self.error_log
                    .push(Report::new(e).wrap_err("Getting new device list"));
                return;
            }
        };

        self.devices.clear();

        for device in devices {
            let repaint = {
                let parent = repaint.clone();
                move || parent()
            };

            match connection::MeteredConnection::new(device, repaint) {
                Ok(conn) => {
                    self.devices.push(conn);
                }
                Err(e) => {
                    self.error_log.push(e.wrap_err("setting up connection"));
                }
            };
        }
    }
}

impl<H, S, I, E> App<H, S, I, E> {
    fn new(host: H) -> Self {
        App {
            host,
            repaint: None,
            devices: Default::default(),
            error_log: Default::default(),
            frame: std::time::Instant::now(),
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

impl<H> eframe::App for App<H, HostInputStream<H>, i32, cpal::StreamError>
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

            self.devices.retain_mut(|connection| {
                let mut keep = true;

                println!("Name: {}", connection.name());
                connection.process();

                ui.separator();

                ui.heading(connection.name());

                let max = connection
                    .take_duration_samples(frametime)
                    .fold(0, |acc, it| acc.max(it.saturating_abs()));
                let max_frac = max as f32 / i32::MAX as f32;

                ui.add(egui::ProgressBar::new(max_frac).text("current"));

                ui.label(format!(
                    "Buffered: {}ms",
                    connection.buffered_time().as_millis()
                ));

                if ui.button("remove").clicked() {
                    keep = false;
                }

                keep
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

// fn main() -> Result<()> {
//     {
//         use tracing_subscriber::prelude::*;
//         tracing_subscriber::registry()
//             .with(
//                 tracing_subscriber::fmt::layer()
//                     .with_timer(tracing_subscriber::fmt::time::uptime())
//                     .with_filter(tracing_subscriber::EnvFilter::from_default_env()),
//             )
//             .init();

//         color_eyre::install()?;
//     }

//     println!("Hello, world!");

//     let host = cpal::host_from_id(cpal::HostId::Wasapi).wrap_err("Getting Audio Host")?;

//     for input in host.input_devices().wrap_err("getting inputs")? {
//         let name = input
//             .name()
//             .map(Cow::Owned)
//             .unwrap_or(Cow::Borrowed("<unknown>"));
//         println!("- {}", name);

//         if name.contains("Scarlett") {
//             println!("Starting stream!");
//             for config in input
//                 .supported_input_configs()
//                 .wrap_err("couldn't get supported configurations")?
//             {
//                 println!("Supported config: {config:#?}");
//             }

//             let supported_config = input
//                 .default_input_config()
//                 .wrap_err("getting input config")?;

//             let config = {
//                 let buffer_size = match supported_config.buffer_size() {
//                     &cpal::SupportedBufferSize::Range { min, max: _ } => {
//                         cpal::BufferSize::Fixed(buffer_size_from_min(min))
//                     }
//                     cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Default,
//                 };

//                 cpal::StreamConfig {
//                     channels: supported_config.channels(),
//                     sample_rate: supported_config.sample_rate(),
//                     buffer_size,
//                 }
//             };

//             eprintln!("{config:#?}");

//             let stream = input
//                 .build_input_stream(&config, i8s, log_err, None)
//                 .wrap_err("building input stream")?;

//             stream.play().wrap_err("starting stream")?;

//             let mut buf = String::new();
//             std::io::stdin()
//                 .read_line(&mut buf)
//                 .wrap_err("getting user input")?;

//             stream.pause().wrap_err("pausing stream")?;
//             drop(stream);
//         }
//     }

//     Ok(())
// }
