use clap::Parser;
use door::*;
use futures_util::{pin_mut, StreamExt};
use http_api_problem::HttpApiProblem;
use serde::{Deserialize, Serialize};
use signal_hook::consts::SIGHUP;
use signal_hook_tokio::Signals;
use std::{net::SocketAddr, path::PathBuf, time::Duration, vec};
use tokio::{
    join, select,
    sync::{mpsc, oneshot},
    time::Sleep,
};
use tracing::{error, info};
use wot_serve::{
    servient::{BuildServient, HttpRouter, ServientSettings},
    Servient,
};

use axum::{
    http::status::StatusCode,
    response::{IntoResponse, Json},
    Extension,
};
use wot_td::builder::{
    BuildableHumanReadableInfo, BuildableInteractionAffordance, ReadableWriteableDataSchema,
    SpecializableDataSchema,
};

struct Thing {
    status: Door,
    simulation: vec::IntoIter<DoorSimulation>,
}

const MESSAGE_QUEUE_LENGTH: usize = 16;

#[derive(Parser)]
struct Cli {
    /// The config TOML file for the ticking door.
    config: PathBuf,

    /// Dump a default configuration to the specified file and exit.
    #[clap(short, long)]
    dump: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct Config {
    door: DoorConfig,
}

#[derive(Debug, Deserialize, Serialize)]
struct DoorConfig {
    initial: Door,
    simulation: Vec<DoorSimulation>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct DoorSimulation {
    #[serde(with = "humantime_serde")]
    wait: Duration,
    #[serde(skip_serializing_if = "Option::is_none")]
    open: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lock: Option<LockStatus>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    if cli.dump {
        let config = Config {
            door: DoorConfig {
                initial: Door::new(false, LockStatus::Unlocked),
                simulation: vec![
                    DoorSimulation {
                        wait: Duration::from_secs(5),
                        open: Some(true),
                        ..Default::default()
                    },
                    DoorSimulation {
                        wait: Duration::from_secs(5),
                        open: Some(false),
                        lock: Some(LockStatus::Locked),
                    },
                    DoorSimulation {
                        wait: Duration::from_secs(5),
                        lock: Some(LockStatus::Jammed),
                        ..Default::default()
                    },
                    DoorSimulation {
                        wait: Duration::from_secs(5),
                        open: Some(true),
                        ..Default::default()
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
        status: config.door.initial,
        simulation: config.door.simulation.into_iter(),
    };

    let (message_sender, message_receiver) = mpsc::channel(MESSAGE_QUEUE_LENGTH);

    let app_state = AppState {
        message_sender: message_sender.clone(),
    };

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    let thing_builder = Servient::builder("Ticking Door")
        .finish_extend()
        .id("urn:dev:ops:ticking-door-1234")
        .attype("DoorSensor")
        .attype("Lock");
    let mut servient = thing_builder
        .security(|b| b.no_sec().with_key("nosec_sc").required())
        .property("open", |b| {
            b.finish_extend_data_schema()
                .attype("OpenProperty")
                .title("Open")
                .description("Whether the door is open")
                .form(|b| {
                    b.href("/properties/open")
                        .http_get(get_open_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                })
                .bool()
                .read_only()
        })
        .property("locked", |b| {
            b.finish_extend_data_schema()
                .attype("LockedProperty")
                .title("Locked")
                .description("Whether the door is locked")
                .form(|b| {
                    b.href("/properties/locked")
                        .http_get(get_locked_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                })
                .bool()
                .read_only()
        })
        .action("lock", |b| {
            b.attype("LockAction")
                .title("Lock")
                .description("Lock the door, if possible")
                .form(|b| {
                    b.href("/action/lock")
                        .http_post(action_lock)
                        .op(wot_td::thing::FormOperation::InvokeAction)
                })
        })
        .action("unlock", |b| {
            b.attype("UnlockAction")
                .title("Unlock")
                .description("Unlock the door, if possible")
                .form(|b| {
                    b.href("/action/unlock")
                        .http_post(action_unlock)
                        .op(wot_td::thing::FormOperation::InvokeAction)
                })
        })
        .http_bind(addr)
        .build_servient()
        .expect("cannot build Thing Descriptor for the ticking door");

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
    async fn get_open(&self) -> bool {
        self.use_oneshot(Message::GetOpen).await
    }

    #[inline]
    async fn get_lock(&self) -> LockStatus {
        self.use_oneshot(Message::GetLock).await
    }

    #[inline]
    async fn lock(&self) -> Result<(), DoorError> {
        self.use_oneshot(Message::Lock).await
    }

    #[inline]
    async fn unlock(&self) -> Result<(), DoorError> {
        self.use_oneshot(Message::Unlock).await
    }
}

#[derive(Debug)]
enum Message {
    GetOpen(oneshot::Sender<bool>),
    GetLock(oneshot::Sender<LockStatus>),
    Lock(oneshot::Sender<Result<(), DoorError>>),
    Unlock(oneshot::Sender<Result<(), DoorError>>),
    SetConfig(Config),
}

async fn handle_messages(
    thing: Thing,
    mut receiver: mpsc::Receiver<Message>,
    sender: mpsc::Sender<Message>,
    cli: &Cli,
) {
    let Thing {
        mut status,
        simulation,
    } = thing;

    let signals = Signals::new([SIGHUP]).expect("unable to create signal handlers");
    let signals_handle = signals.handle();
    let signals_task = tokio::spawn(handle_signals(
        signals,
        cli.config.to_owned(),
        sender.clone(),
    ));

    let simulation_stream = SimulationStream::new(simulation);
    pin_mut!(simulation_stream);

    let mut next_status_future = simulation_stream.next();

    'outer: loop {
        macro_rules! handle_message {
            ($message:expr) => {{
                let Some(message) = $message else {
                                    error!("Messages channel has been closed unexpectedly. Stopping.");
                                    signals_handle.close();
                                    if let Err(err) = signals_task.await {
                                        error!("Error received signals task on joining: {err}");
                                    }
                                    break 'outer;
                                };

                if let Some(config) =
                    handle_message(message, &mut status).await
                {
                    info!("New config obtained. Resetting door statuses to new config.");

                    status = config.door.initial;
                    simulation_stream.set(SimulationStream::new(config.door.simulation.into_iter()));
                    next_status_future = simulation_stream.next();

                    continue 'outer;
                }
            }};
        }

        loop {
            select! {
                message = receiver.recv() => handle_message!(message),

                next_status = &mut next_status_future => {
                    match next_status {
                        Some(next_status) => {
                            if let Some(open) = next_status.open {
                                status.simulate_open(open);
                                if open {
                                    info!("Door is now open");
                                } else {
                                    info!("Door is now closed");
                                }
                            }

                            if let Some(lock) = next_status.lock {
                                status.simulate_lock(lock);
                                info!("Door lock status is {lock}");
                            }
                        }
                        None => break,
                    }
                }
            }
        }

        loop {
            handle_message!(receiver.recv().await);
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

mod door {
    use std::{
        fmt,
        future::Future,
        pin::Pin,
        task::{self, ready, Poll},
    };

    use futures_util::{stream::FusedStream, Stream};
    use pin_project_lite::pin_project;

    use super::*;

    #[derive(Debug, Default, Serialize, Deserialize)]
    pub struct Door {
        is_open: bool,
        lock: LockStatus,
    }

    impl Door {
        pub fn new(is_open: bool, lock: LockStatus) -> Self {
            Self { is_open, lock }
        }

        pub fn simulate_open(&mut self, is_open: bool) -> &mut Self {
            self.is_open = is_open;
            self
        }

        pub fn simulate_lock(&mut self, lock: LockStatus) -> &mut Self {
            self.lock = lock;
            self
        }

        pub fn do_lock(&mut self) -> Result<&mut Self, DoorError> {
            if self.is_open {
                Err(DoorError::OpenDoor)
            } else {
                self.lock.lock().map_err(DoorError::Lock)?;
                Ok(self)
            }
        }

        pub fn do_unlock(&mut self) -> Result<&mut Self, DoorError> {
            if self.is_open {
                Err(DoorError::OpenDoor)
            } else {
                self.lock.unlock().map_err(DoorError::Lock)?;
                Ok(self)
            }
        }

        pub fn is_open(&self) -> bool {
            self.is_open
        }

        pub fn lock(&self) -> LockStatus {
            self.lock
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum DoorError {
        OpenDoor,
        Lock(LockError),
    }

    impl fmt::Display for DoorError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::OpenDoor => f.write_str("cannot lock or unlock an open door"),
                Self::Lock(err) => write!(f, "{err}"),
            }
        }
    }

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum LockStatus {
        #[default]
        Unlocked,
        Locked,
        Jammed,
    }

    impl fmt::Display for LockStatus {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Unlocked => f.write_str("unlocked"),
                Self::Locked => f.write_str("locked"),
                Self::Jammed => f.write_str("jammed"),
            }
        }
    }

    impl From<LockNormalStatus> for LockStatus {
        fn from(value: LockNormalStatus) -> Self {
            match value {
                LockNormalStatus::Locked => Self::Locked,
                LockNormalStatus::Unlocked => Self::Unlocked,
            }
        }
    }

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum LockNormalStatus {
        #[default]
        Unlocked,
        Locked,
    }

    impl fmt::Display for LockNormalStatus {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Unlocked => f.write_str("unlocked"),
                Self::Locked => f.write_str("locked"),
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum LockError {
        AlreadyInStatus(LockNormalStatus),
        Jammed,
    }

    impl fmt::Display for LockError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::AlreadyInStatus(status) => {
                    write!(
                        f,
                        "Lock status is already {status} and it is not being changed"
                    )
                }
                Self::Jammed => f.write_str("lock is jammed"),
            }
        }
    }

    impl LockStatus {
        pub fn lock(&mut self) -> Result<&mut Self, LockError> {
            match self {
                Self::Unlocked => {
                    *self = Self::Locked;
                    Ok(self)
                }
                Self::Locked => Err(LockError::AlreadyInStatus(LockNormalStatus::Locked)),
                Self::Jammed => Err(LockError::Jammed),
            }
        }

        pub fn unlock(&mut self) -> Result<&mut Self, LockError> {
            match self {
                Self::Locked => {
                    *self = Self::Unlocked;
                    Ok(self)
                }
                Self::Unlocked => Err(LockError::AlreadyInStatus(LockNormalStatus::Unlocked)),
                Self::Jammed => Err(LockError::Jammed),
            }
        }
    }

    pin_project! {
        #[project_replace = SimulationStreamProjReplace]
        pub struct SimulationStream {
            simulations: vec::IntoIter<DoorSimulation>,
            #[pin]
            next_status: Option<SimulationStreamNext>,
        }
    }

    pin_project! {
        #[project_replace = SimulationStreamNextProjReplace]
        struct SimulationStreamNext {
            #[pin]
            sleep: Sleep,
            output: SimulationStreamItem,
        }
    }

    impl SimulationStream {
        pub(crate) fn new(simulations: impl Into<vec::IntoIter<DoorSimulation>>) -> Self {
            let mut simulations = simulations.into();
            let next_status = simulations.next().map(SimulationStreamNext::from);

            Self {
                simulations,
                next_status,
            }
        }
    }

    impl Stream for SimulationStream {
        type Item = SimulationStreamItem;

        fn poll_next(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
            let mut this = self.project();
            let Some(mut next_status) = this.next_status.as_mut().as_pin_mut() else {
                return Poll::Ready(None);
            };

            ready!(next_status.as_mut().project().sleep.poll(cx));
            let current_status = match this.simulations.next() {
                Some(next_simulation) => next_status.project_replace(next_simulation.into()).output,
                None => {
                    let current_status = this
                        .next_status
                        .as_ref()
                        .as_pin_ref()
                        .unwrap()
                        .as_ref()
                        .output;
                    this.next_status.as_mut().set(None);
                    current_status
                }
            };
            Poll::Ready(Some(current_status))
        }
    }

    impl FusedStream for SimulationStream {
        #[inline]
        fn is_terminated(&self) -> bool {
            self.next_status.is_none()
        }
    }

    impl From<DoorSimulation> for SimulationStreamNext {
        fn from(value: DoorSimulation) -> Self {
            let DoorSimulation { wait, open, lock } = value;
            let sleep = tokio::time::sleep(wait);
            let output = SimulationStreamItem { open, lock };
            Self { sleep, output }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SimulationStreamItem {
        pub open: Option<bool>,
        pub lock: Option<LockStatus>,
    }
}

async fn handle_message(message: Message, status: &mut Door) -> Option<Config> {
    use Message::*;

    match message {
        GetOpen(sender) => {
            sender.send(status.is_open()).unwrap();
            None
        }
        GetLock(sender) => {
            sender.send(status.lock()).unwrap();
            None
        }
        Lock(sender) => {
            sender.send(status.do_lock().map(|_| ())).unwrap();
            None
        }
        Unlock(sender) => {
            sender.send(status.do_unlock().map(|_| ())).unwrap();
            None
        }
        SetConfig(new_config) => Some(new_config),
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Properties {
    open: bool,
    locked: LockStatus,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Property {
    Open(bool),
    Locked(LockStatus),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PropertyName {
    Open,
    Locked,
}

async fn get_open_property(Extension(app): Extension<AppState>) -> Json<bool> {
    let value = app.get_open().await;
    Json(value)
}

async fn get_locked_property(Extension(app): Extension<AppState>) -> Json<LockStatus> {
    let value = app.get_lock().await;
    Json(value)
}

async fn action_lock(Extension(app): Extension<AppState>) -> impl IntoResponse {
    match app.lock().await {
        Ok(()) => (StatusCode::OK, Json(None)),
        Err(err) => {
            let status = StatusCode::BAD_REQUEST;
            let title = format!("cannot lock door: {err}");
            (status, Json(Some(HttpApiProblem::new(status).title(title))))
        }
    }
}

async fn action_unlock(Extension(app): Extension<AppState>) -> impl IntoResponse {
    match app.unlock().await {
        Ok(()) => (StatusCode::OK, Json(None)),
        Err(err) => {
            let status = StatusCode::BAD_REQUEST;
            let title = format!("cannot unlock door: {err}");
            (status, Json(Some(HttpApiProblem::new(status).title(title))))
        }
    }
}
