use clap::Parser;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use signal_hook::consts::SIGHUP;
use signal_hook_tokio::Signals;
use std::{net::SocketAddr, ops::Not, path::PathBuf, time::Duration, vec};
use tokio::{
    join, select,
    sync::{mpsc, oneshot},
};
use tracing::{error, info, instrument, trace, warn};
use wot_serve::{
    servient::{BuildServient, HttpRouter, ServientSettings},
    Servient,
};

use axum::{response::Json, Extension};
use wot_td::builder::{
    BuildableDataSchema, BuildableHumanReadableInfo, BuildableInteractionAffordance,
    NumberDataSchemaBuilderLike, ReadableWriteableDataSchema, SpecializableDataSchema,
};

struct Thing {
    temperature: SensorStatus,
    humidity: SensorStatus,
}

const MESSAGE_QUEUE_LENGTH: usize = 16;
const TICK_DURATION: Duration = Duration::from_millis(10);

#[derive(Parser)]
struct Cli {
    /// The config TOML file for the ticking sensor.
    config: PathBuf,

    /// Dump a default configuration to the specified file and exit.
    #[clap(short, long)]
    dump: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct Config {
    humidity: SensorConfig,
    temperature: SensorConfig,
}

#[derive(Debug, Deserialize, Serialize)]
struct SensorConfig {
    initial: f32,
    variations: Vec<SensorVariation>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SensorVariation {
    #[serde(with = "humantime_serde")]
    duration: Duration,
    variation: f32,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    if cli.dump {
        let config = Config {
            humidity: SensorConfig {
                initial: 60.,
                variations: vec![
                    SensorVariation {
                        duration: Duration::from_secs(10),
                        variation: 0.1,
                    },
                    SensorVariation {
                        duration: Duration::from_secs(7),
                        variation: -0.2,
                    },
                    SensorVariation {
                        duration: Duration::from_secs(5),
                        variation: 0.3,
                    },
                ],
            },
            temperature: SensorConfig {
                initial: 20.,
                variations: vec![
                    SensorVariation {
                        duration: Duration::from_secs(10),
                        variation: -0.3,
                    },
                    SensorVariation {
                        duration: Duration::from_secs(4),
                        variation: 0.8,
                    },
                    SensorVariation {
                        duration: Duration::from_secs(5),
                        variation: -0.2,
                    },
                ],
            },
        };

        let config = toml::to_vec(&config).unwrap();
        std::fs::write(&cli.config, config).expect("unable to dump config to file");
        println!(
            "Configuration successfully written to {}",
            cli.config.display()
        );
        return;
    };

    let config: Config = {
        let config = std::fs::read(&cli.config).expect("unable to read config file");
        toml::from_slice(&config).expect("unable to parse config file")
    };

    let thing = Thing {
        temperature: config.temperature.into(),
        humidity: config.humidity.into(),
    };

    let (message_sender, message_receiver) = mpsc::channel(MESSAGE_QUEUE_LENGTH);

    let app_state = AppState {
        message_sender: message_sender.clone(),
    };

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    let thing_builder = Servient::builder("Ticking Sensor")
        .finish_extend()
        .id("urn:dev:ops:ticking-sensor-1234")
        .attype("TemperatureSensor")
        .attype("HumiditySensor");
    let mut servient = thing_builder
        .security(|b| b.no_sec().with_key("nosec_sc").required())
        .property("temperature", |b| {
            b.finish_extend_data_schema()
                .attype("TemperatureProperty")
                .title("Temperature")
                .description("The measured temperature")
                .form(|b| {
                    b.href("/properties/temperature")
                        .http_get(get_temperature_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                })
                .number()
                .read_only()
                .unit("degree celsius")
        })
        .property("humidity", |b| {
            b.finish_extend_data_schema()
                .attype("HumidityProperty")
                .title("Humidity")
                .description("The measured humidity")
                .form(|b| {
                    b.href("/properties/humidity")
                        .http_get(get_humidity_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                })
                .number()
                .read_only()
                .minimum(0.)
                .maximum(100.)
                .unit("percent")
        })
        .http_bind(addr)
        .build_servient()
        .expect("cannot build Thing Descriptor for the ticking sensor");

    servient.router = servient.router.layer(Extension(app_state));

    let axum_future = async {
        tracing::debug!("listening on {}", addr);
        servient
            .serve()
            .await
            .unwrap_or_else(|err| panic!("unable to create web server on address {addr}: {err}"));
    };

    join!(
        handle_messages(thing, message_receiver, message_sender, &cli),
        axum_future
    );
}

#[derive(Clone)]
struct AppState {
    message_sender: mpsc::Sender<Message>,
}

impl AppState {
    #[inline]
    async fn use_oneshot<F, T>(&self, f: F) -> T
    where
        F: FnOnce(oneshot::Sender<T>) -> Message,
    {
        let (sender, receiver) = oneshot::channel();
        self.send_message(f(sender)).await;

        receiver.await.unwrap()
    }

    async fn send_message(&self, message: Message) {
        self.message_sender
            .send(message)
            .await
            .expect("message channel should be open");
    }

    #[inline]
    async fn get_temperature(&self) -> f32 {
        self.use_oneshot(Message::GetTemperature).await
    }

    #[inline]
    async fn get_humidity(&self) -> f32 {
        self.use_oneshot(Message::GetHumidity).await
    }
}

#[derive(Debug)]
enum Message {
    GetTemperature(oneshot::Sender<f32>),
    GetHumidity(oneshot::Sender<f32>),
    SetConfig(Config),
}

async fn handle_messages(
    thing: Thing,
    mut receiver: mpsc::Receiver<Message>,
    sender: mpsc::Sender<Message>,
    cli: &Cli,
) {
    let Thing {
        mut temperature,
        mut humidity,
    } = thing;

    let signals = Signals::new([SIGHUP]).expect("unable to create signal handlers");
    let signals_handle = signals.handle();
    let signals_task = tokio::spawn(handle_signals(
        signals,
        cli.config.to_owned(),
        sender.clone(),
    ));

    let mut interval = tokio::time::interval(TICK_DURATION);
    // Skip immediate tick
    interval.tick().await;
    let mut maybe_interval = Some(interval);

    macro_rules! handle_message {
        ($message:expr) => {{
            let Some(message) = $message else {
                                error!("Messages channel has been closed unexpectedly. Stopping.");
                                signals_handle.close();
                                if let Err(err) = signals_task.await {
                                    error!("Error received signals task on joining: {err}");
                                }
                                break;
                            };

            if let Some(config) =
                handle_message(message, temperature.current, humidity.current).await
            {
                info!("New config obtained. Resetting sensor statuses to new config.");

                humidity = SensorStatus {
                    current: humidity.current,
                    ..config.humidity.into()
                };
                temperature = SensorStatus {
                    current: temperature.current,
                    ..config.temperature.into()
                };
                maybe_interval = {
                    let mut interval = tokio::time::interval(TICK_DURATION);
                    // Skip immediate tick
                    interval.tick().await;
                    Some(interval)
                };
            }
        }};
    }

    loop {
        match &mut maybe_interval {
            Some(interval) => {
                select! {
                    message = receiver.recv() => handle_message!(message),

                    _ = interval.tick() => {
                        let temperature_ticked = temperature.tick();
                        let humidity_ticked = humidity.tick();

                        if temperature_ticked.not() & humidity_ticked.not() {
                            maybe_interval = None;
                        }
                    }
                }
            }
            None => handle_message!(receiver.recv().await),
        }
    }
}

async fn handle_signals(mut signals: Signals, config_file: PathBuf, sender: mpsc::Sender<Message>) {
    while let Some(signal) = signals.next().await {
        assert_eq!(signal, SIGHUP);
        let raw_config = match std::fs::read(&config_file) {
            Ok(config) => config,
            Err(err) => {
                error!("unable to read config file: {err}");
                continue;
            }
        };

        match toml::from_slice(&raw_config) {
            Ok(new_config) => {
                if sender.send(Message::SetConfig(new_config)).await.is_err() {
                    error!("unable to send new config through channel: channel is closed.");
                }
            }
            Err(err) => error!("unable to parse config file: {err}"),
        }
    }
}

#[derive(Debug)]
struct SensorStatus {
    current: f32,
    interpolation: Option<Interpolation>,
    variations: vec::IntoIter<SensorVariation>,
}

impl SensorStatus {
    #[instrument]
    fn tick(&mut self) -> bool {
        loop {
            match self.interpolation.as_mut() {
                Some(interpolation) => match interpolation.step() {
                    Some(new_value) => {
                        trace!("ticked a new value: {new_value}");
                        self.current = new_value;
                        break true;
                    }
                    None => {
                        self.interpolation = self
                            .variations
                            .next()
                            .map(|variation| interpolation_from_variation(self.current, variation))
                    }
                },
                None => break false,
            }
        }
    }
}

fn interpolation_from_variation(current: f32, variation: SensorVariation) -> Interpolation {
    let duration = variation.duration.as_millis();
    let len = usize::try_from(duration / TICK_DURATION.as_millis())
        .map(|mut len| {
            if duration % TICK_DURATION.as_millis() != 0 {
                len += 1;
            }
            len
        })
        .unwrap_or(usize::MAX);
    let start = current;
    let end = current + variation.variation;

    trace!("new interpolation between {start} and {end} to run in {len} ticks");
    Interpolation {
        start,
        end,
        index: 0,
        len,
    }
}

impl From<SensorConfig> for SensorStatus {
    #[inline]
    fn from(value: SensorConfig) -> Self {
        let SensorConfig {
            initial,
            variations,
        } = value;
        let mut variations = variations.into_iter();
        let interpolation = variations
            .next()
            .map(|variation| interpolation_from_variation(initial, variation));

        Self {
            current: initial,
            interpolation,
            variations,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Interpolation {
    start: f32,
    end: f32,
    index: usize,
    len: usize,
}

impl Interpolation {
    fn step(&mut self) -> Option<f32> {
        (self.index < self.len).then(|| {
            self.index += 1;

            let range_len = self.end - self.start;
            range_len * self.index as f32 / self.len as f32 + self.start
        })
    }
}

async fn handle_message(message: Message, temperature: f32, humidity: f32) -> Option<Config> {
    use Message::*;

    match message {
        GetTemperature(sender) => {
            sender.send(temperature).unwrap();
            None
        }
        GetHumidity(sender) => {
            sender.send(humidity).unwrap();
            None
        }
        SetConfig(new_config) => Some(new_config),
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Properties {
    temperature: f32,
    humidity: f32,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Property {
    Temperature(f32),
    Humidity(f32),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PropertyName {
    Temperature,
    Humidity,
}

async fn get_temperature_property(Extension(app): Extension<AppState>) -> Json<f32> {
    let temperature = app.get_temperature().await;
    Json(temperature)
}

async fn get_humidity_property(Extension(app): Extension<AppState>) -> Json<f32> {
    let temperature = app.get_humidity().await;
    Json(temperature)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolation() {
        const INIT_LERP: Interpolation = Interpolation {
            start: 2.,
            end: 6.,
            index: 0,
            len: 4,
        };

        let mut lerp = INIT_LERP;

        assert_eq!(lerp.step(), Some(3.));
        assert_eq!(
            lerp,
            Interpolation {
                index: 1,
                ..INIT_LERP
            }
        );

        assert_eq!(lerp.step(), Some(4.));
        assert_eq!(
            lerp,
            Interpolation {
                index: 2,
                ..INIT_LERP
            }
        );

        assert_eq!(lerp.step(), Some(5.));
        assert_eq!(
            lerp,
            Interpolation {
                index: 3,
                ..INIT_LERP
            }
        );

        assert_eq!(lerp.step(), Some(6.));
        assert_eq!(
            lerp,
            Interpolation {
                index: 4,
                ..INIT_LERP
            }
        );

        assert_eq!(lerp.step(), None);
        assert_eq!(
            lerp,
            Interpolation {
                index: 4,
                ..INIT_LERP
            }
        );
    }
}
