use clap::Parser;
use demo_things::{config_signal_loader, CliCommon, OptionStream};
use futures_concurrency::{future::Join, stream::Merge};
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use signal_hook::consts::SIGHUP;
use std::{future, ops::Not, path::PathBuf, time::Duration, vec};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::{IntervalStream, ReceiverStream};
use tower_http::cors::CorsLayer;
use tracing::{info, instrument, trace, warn};
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
    #[clap(flatten)]
    common: CliCommon,

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
    let cli = Cli::parse();
    cli.common.setup_tracing();

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

    let addr = cli.common.socket_addr();
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

    let cors = CorsLayer::new()
        .allow_methods(tower_http::cors::Any)
        .allow_origin(tower_http::cors::Any);

    servient.router = servient.router.layer(Extension(app_state)).layer(cors);

    let axum_future = async {
        tracing::debug!("listening on {}", addr);
        servient
            .serve()
            .await
            .unwrap_or_else(|err| panic!("unable to create web server on address {addr}: {err}"));
    };

    (handle_messages(thing, message_receiver, &cli), axum_future)
        .join()
        .await;
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
}

async fn handle_messages(thing: Thing, receiver: mpsc::Receiver<Message>, cli: &Cli) {
    #[derive(Debug)]
    enum Event {
        Message(Message),
        Config(Config),
        Tick,
        Stop,
    }

    let create_interval_stream = || {
        OptionStream::from(Some(
            IntervalStream::new(tokio::time::interval(Duration::from_millis(10)))
                .map(|_| Event::Tick),
        ))
    };

    let Thing {
        mut temperature,
        mut humidity,
    } = thing;

    let mut csl = config_signal_loader([SIGHUP], &cli.config);

    let mut interval_stream = create_interval_stream();
    let mut receiver_stream = ReceiverStream::new(receiver)
        .map(Event::Message)
        .chain(stream::once(future::ready(Event::Stop)));
    let mut csl_stream = csl.stream.by_ref().map(Event::Config);

    'outer: loop {
        let mut events_stream =
            (&mut receiver_stream, &mut csl_stream, &mut interval_stream).merge();

        loop {
            let Some(event) = events_stream.next().await else {
                break 'outer;
            };

            match event {
                Event::Message(message) => {
                    handle_message(message, temperature.current, humidity.current).await
                }
                Event::Tick => {
                    let temperature_ticked = temperature.tick();
                    let humidity_ticked = humidity.tick();

                    if temperature_ticked.not() & humidity_ticked.not() {
                        interval_stream = None.into();
                        break;
                    }
                }
                Event::Config(config) => {
                    info!("New config obtained. Resetting sensor statuses to new config.");

                    humidity = SensorStatus {
                        current: humidity.current,
                        ..config.humidity.into()
                    };
                    temperature = SensorStatus {
                        current: temperature.current,
                        ..config.temperature.into()
                    };

                    interval_stream = create_interval_stream();
                    // Skip immediate tick
                    interval_stream.next().await;
                    break;
                }
                Event::Stop => break 'outer,
            }
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

async fn handle_message(message: Message, temperature: f32, humidity: f32) {
    use Message::*;

    match message {
        GetTemperature(sender) => {
            sender.send(temperature).unwrap();
        }
        GetHumidity(sender) => {
            sender.send(humidity).unwrap();
        }
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
