use std::{future, ops::Not, path::PathBuf, pin::pin, time::Duration, vec};

use axum::{
    http::StatusCode,
    response::{IntoResponse, Json},
    Extension,
};
use clap::Parser;
use demo_things::{config_signal_loader, CliCommon, Simulation, SimulationStream};
use futures_concurrency::prelude::*;
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use signal_hook::consts::SIGHUP;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::{IntervalStream, ReceiverStream};
use tracing::{debug, info, trace};
use wot_serve::{
    servient::{BuildServient, HttpRouter, ServientSettings},
    Servient,
};
use wot_td::builder::{
    BuildableDataSchema, BuildableHumanReadableInfo, BuildableInteractionAffordance,
    ReadableWriteableDataSchema, SpecializableDataSchema,
};

const MESSAGE_QUEUE_LENGTH: usize = 16;

#[derive(Debug, Deserialize, Serialize)]
struct FridgeStatus {
    open: bool,
    temperature: f32,
    target_temperature: i8,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct FridgeSimulation {
    #[serde(with = "humantime_serde")]
    wait: Duration,
    #[serde(skip_serializing_if = "Option::is_none")]
    open: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize)]
struct FridgeConfig {
    initial: FridgeStatus,
    simulation: Vec<FridgeSimulation>,
    deltas: Deltas,
}

#[derive(Debug, Deserialize, Serialize)]
struct Deltas {
    decrease: f32,
    door_open: f32,
    door_close: f32,
    target_range: f32,
}

#[derive(Debug)]
struct Fridge {
    status: FridgeStatus,
    simulation: vec::IntoIter<FridgeSimulation>,
    deltas: Deltas,
    cooling: bool,
}

#[derive(Debug, Parser)]
pub struct Cli {
    #[clap(flatten)]
    common: CliCommon,

    /// Dump a default configuration to the specified file and exit.
    #[clap(short, long)]
    dump: bool,

    /// The config TOML file for the fridge.
    config: PathBuf,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    cli.common.setup_tracing();

    if cli.dump {
        let config = FridgeConfig {
            initial: FridgeStatus {
                open: false,
                temperature: 8.0,
                target_temperature: 4,
            },
            deltas: Deltas {
                decrease: 0.35,
                door_open: 0.3,
                door_close: 0.01,
                target_range: 3.5,
            },
            simulation: vec![
                FridgeSimulation {
                    wait: Duration::from_secs(3),
                    open: Some(true),
                },
                FridgeSimulation {
                    wait: Duration::from_secs(8),
                    open: Some(false),
                },
                FridgeSimulation {
                    wait: Duration::from_secs(15),
                    open: Some(true),
                },
                FridgeSimulation {
                    wait: Duration::from_secs(3),
                    open: Some(false),
                },
            ],
        };

        let config = toml::to_vec(&config).unwrap();
        std::fs::write(&cli.config, config).expect("unable to dump config to file");
        println!(
            "Configuration successfully written to {}",
            cli.config.display()
        );
        return;
    };

    let fridge: FridgeConfig = {
        let config = std::fs::read(&cli.config).expect("unable to read config file");
        toml::from_slice(&config).expect("unable to parse config file")
    };

    let fridge = Fridge {
        status: fridge.initial,
        simulation: fridge.simulation.into_iter(),
        deltas: fridge.deltas,
        cooling: false,
    };

    let (message_sender, message_receiver) = mpsc::channel(MESSAGE_QUEUE_LENGTH);

    let app_state = AppState {
        message_sender: message_sender.clone(),
    };

    let addr = cli.common.socket_addr();
    let mut servient = Servient::builder("My Fridge")
        .finish_extend()
        .id("urn:dev:ops:my-fridge-1234")
        .attype("DoorSensor")
        .attype("Thermostat")
        .description("A web connected fridge")
        .security(|b| b.no_sec().with_key("nosec_sc").required())
        .form(|b| {
            b.href("/properties")
                .http_get(properties)
                .content_type("application/json")
                .op(wot_td::thing::FormOperation::ReadAllProperties)
        })
        .property("door", |b| {
            b.finish_extend_data_schema()
                .attype("OpenProperty")
                .title("Open door")
                .description("Whether the door of the fridge is open or closed")
                .form(|b| {
                    b.href("/properties/door")
                        .http_get(get_door_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                })
                .bool()
                .read_only()
        })
        .property("temperature", |b| {
            b.finish_extend_data_schema()
                .attype("TemperatureProperty")
                .title("Temperature")
                .description("The temperature")
                .form(|b| {
                    b.href("/properties/temperature")
                        .http_get(get_temperature_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                })
                .integer()
                .unit("degree celsius")
                .read_only()
        })
        .property("target_temperature", |b| {
            b.finish_extend_data_schema()
                .attype("TargetTemperatureProperty")
                .title("Target temperature")
                .description("The target temperature")
                .form(|b| {
                    b.href("/properties/target-temperature")
                        .http_get(get_target_temperature_property)
                        .http_put(put_target_temperature_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                        .op(wot_td::thing::FormOperation::WriteProperty)
                })
                .integer()
                .unit("degree celsius")
        })
        .http_bind(addr)
        .build_servient()
        .expect("cannot build Thing Descriptor for the fridge");

    servient.router = servient.router.layer(Extension(app_state));

    let axum_future = async {
        tracing::debug!("listening on {}", addr);
        servient
            .serve()
            .await
            .unwrap_or_else(|err| panic!("unable to create web server on address {addr}: {err}"));
    };

    (handle_messages(fridge, message_receiver, &cli), axum_future)
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
    async fn get_properties(&self) -> Properties {
        self.use_oneshot(Message::GetProperties).await
    }

    #[inline]
    async fn get_target_temperature(&self) -> i8 {
        self.use_oneshot(Message::GetTargetTemperature).await
    }

    #[inline]
    async fn get_door_is_open(&self) -> bool {
        self.use_oneshot(Message::GetOpen).await
    }

    #[inline]
    async fn get_temperature(&self) -> i8 {
        self.use_oneshot(Message::GetTemperature).await
    }

    #[inline]
    async fn set_target_temperature(&self, value: i8) {
        self.send_message(Message::SetTargetTemperature(value))
            .await;
    }
}

#[derive(Debug)]
enum Message {
    GetProperties(oneshot::Sender<Properties>),
    GetTargetTemperature(oneshot::Sender<i8>),
    GetTemperature(oneshot::Sender<i8>),
    GetOpen(oneshot::Sender<bool>),
    SetTargetTemperature(i8),
}

async fn handle_messages(fridge: Fridge, receiver: mpsc::Receiver<Message>, cli: &Cli) {
    #[derive(Debug)]
    enum Event {
        Message(Message),
        Config(FridgeConfig),
        NextStatus(FridgeSimulationOutput),
        Tick,
        Stop,
    }

    let create_interval_stream = || {
        IntervalStream::new(tokio::time::interval(Duration::from_millis(10))).map(|_| Event::Tick)
    };
    let create_simulation_stream =
        |simulation| SimulationStream::new(simulation).map(Event::NextStatus);

    let Fridge {
        mut status,
        simulation,
        mut deltas,
        mut cooling,
    } = fridge;

    let mut csl = config_signal_loader([SIGHUP], &cli.config);
    let mut simulation_stream = create_simulation_stream(simulation);

    let mut interval_stream = create_interval_stream();
    let mut receiver_stream = ReceiverStream::new(receiver)
        .map(Event::Message)
        .chain(stream::once(future::ready(Event::Stop)));
    let mut csl_stream = csl.stream.by_ref().map(Event::Config);

    'outer: loop {
        let mut events_stream = pin!((
            &mut receiver_stream,
            &mut csl_stream,
            &mut interval_stream,
            simulation_stream,
        )
            .merge());

        loop {
            let Some(event) = events_stream.next().await else {
                break 'outer;
            };

            match event {
                Event::Message(message) => {
                    handle_message(
                        message,
                        &mut status.open,
                        status.temperature,
                        &mut status.target_temperature,
                    )
                    .await
                }
                Event::Tick => handle_tick(
                    status.open,
                    status.target_temperature,
                    &mut status.temperature,
                    &mut cooling,
                    &deltas,
                ),
                Event::Config(config) => {
                    info!("New config obtained. Resetting sensor statuses to new config.");

                    status = config.initial;
                    deltas = config.deltas;
                    simulation_stream = create_simulation_stream(config.simulation.into_iter());

                    interval_stream = create_interval_stream();
                    // Skip immediate tick
                    interval_stream.next().await;
                    break;
                }
                Event::NextStatus(next_status) => {
                    debug!("Got next status from simulation: {next_status:#?}");

                    if let Some(open) = next_status.open {
                        status.open = open;
                        if open {
                            info!("Door is now open");
                        } else {
                            info!("Door is now closed");
                        }
                    }
                }
                Event::Stop => break 'outer,
            }
        }
    }
}

impl Simulation for FridgeSimulation {
    type Output = FridgeSimulationOutput;

    fn output_and_wait(self) -> (Self::Output, Duration) {
        let Self { wait, open } = self;
        let output = FridgeSimulationOutput { open };
        (output, wait)
    }
}

#[derive(Debug)]
struct FridgeSimulationOutput {
    open: Option<bool>,
}

fn handle_tick(
    door_is_open: bool,
    target_temperature: i8,
    temperature: &mut f32,
    cooling: &mut bool,
    deltas: &Deltas,
) {
    #[derive(Debug, Clone, Copy)]
    enum TargetRange {
        Below,
        InRange,
        Above,
    }

    let mut temp_delta = if door_is_open {
        deltas.door_open
    } else {
        deltas.door_close
    };

    let target_temperature = f32::from(target_temperature);
    let target_range = if *temperature < target_temperature - deltas.target_range {
        TargetRange::Below
    } else if *temperature <= target_temperature + deltas.target_range {
        TargetRange::InRange
    } else {
        TargetRange::Above
    };

    if matches!(
        (*cooling, target_range),
        (true, TargetRange::Below) | (false, TargetRange::Above)
    ) {
        *cooling = cooling.not();
    }

    if *cooling {
        temp_delta -= deltas.decrease;
    }

    trace!(
        "Temperature: {temperature:.2} -> {:.2}",
        *temperature + temp_delta
    );
    *temperature += temp_delta;
}

async fn handle_message(
    message: Message,
    door_is_open: &mut bool,
    temperature: f32,
    target_temperature: &mut i8,
) {
    use Message::*;

    match message {
        GetProperties(sender) => {
            let properties = Properties {
                open: *door_is_open,
                temperature: temperature as i8,
                target_temperature: *target_temperature,
            };
            sender.send(properties).unwrap();
        }
        GetTargetTemperature(sender) => sender.send(*target_temperature).unwrap(),
        GetOpen(sender) => sender.send(*door_is_open).unwrap(),
        SetTargetTemperature(value) => {
            *target_temperature = value;
        }
        GetTemperature(sender) => sender.send(temperature as i8).unwrap(),
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Properties {
    open: bool,
    temperature: i8,
    target_temperature: i8,
}

async fn properties(Extension(state): Extension<AppState>) -> Json<Properties> {
    let properties = state.get_properties().await;

    Json(properties)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Property {
    Temperature(i8),
    TargetTemperature(i8),
    Open(bool),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PropertyName {
    Temperature,
    TargetTemperature,
    Open,
}

async fn get_door_property(Extension(app): Extension<AppState>) -> Json<bool> {
    let door_is_open = app.get_door_is_open().await;
    Json(door_is_open)
}

async fn get_temperature_property(Extension(app): Extension<AppState>) -> Json<i8> {
    let temperature = app.get_temperature().await;
    Json(temperature)
}

async fn get_target_temperature_property(Extension(app): Extension<AppState>) -> Json<i8> {
    let target_temperature = app.get_target_temperature().await;
    Json(target_temperature)
}

async fn put_target_temperature_property(
    Json(value): Json<i8>,
    Extension(app): Extension<AppState>,
) -> impl IntoResponse {
    app.set_target_temperature(value).await;
    StatusCode::NO_CONTENT
}
