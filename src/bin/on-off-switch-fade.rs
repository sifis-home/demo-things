use clap::Parser;
use demo_things::{CliCommon, ThingBuilderExt};
use futures_concurrency::{future::Join, stream::Merge};
use futures_util::stream;
use http_api_problem::HttpApiProblem;
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::{future::ready, ops::Not, sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::{
    sync::{mpsc, oneshot},
    time::sleep,
};
use tokio_stream::{
    wrappers::{IntervalStream, ReceiverStream},
    StreamExt,
};
use tower_http::cors::CorsLayer;
use uuid::Uuid;
use wot_serve::{
    servient::{BuildServient, HttpRouter, ServientSettings},
    Servient,
};

use axum::{
    http::StatusCode,
    response::{IntoResponse, Json},
    Extension,
};
use wot_td::builder::{
    BuildableDataSchema, BuildableHumanReadableInfo, BuildableInteractionAffordance,
    IntegerDataSchemaBuilderLike, ObjectDataSchemaBuilderLike, SpecializableDataSchema,
};

struct Thing {
    is_on: bool,
    level: u8,
    actions: Vec<StoredAction>,
}

const MESSAGE_QUEUE_LENGTH: usize = 16;

#[derive(Parser)]
struct Cli {
    #[clap(flatten)]
    common: CliCommon,

    /// Add the Light @type to the switch
    #[clap(long)]
    light: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    cli.common.setup_tracing();

    let thing = Thing {
        is_on: true,
        level: 50,
        actions: Default::default(),
    };

    let (message_sender, message_receiver) = mpsc::channel(MESSAGE_QUEUE_LENGTH);

    let app_state = AppState {
        message_sender: message_sender.clone(),
    };

    let addr = cli.common.socket_addr();
    let mut thing_builder = Servient::builder("On-Off Switch")
        .finish_extend()
        .id("urn:dev:ops:on-off-1234")
        .attype("OnOffSwitch");
    if cli.light {
        thing_builder = thing_builder.attype("Light");
    }
    let mut servient = thing_builder
        .base_from_cli(&cli.common)
        .security(|b| b.no_sec().with_key("nosec_sc").required())
        .property("on", |b| {
            b.finish_extend_data_schema()
                .attype("OnOffProperty")
                .title("On/Off")
                .description("Whether the switch is turned on")
                .form(|b| {
                    b.href("/properties/on")
                        .http_get(get_on_property)
                        .http_put(put_on_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                        .op(wot_td::thing::FormOperation::WriteProperty)
                })
                .bool()
        })
        .action("fade", |b| {
            b.title("Fade")
                .attype("FadeAction")
                .form(|b| {
                    b.href("/actions/fade")
                        .http_post(post_fade_action)
                        .op(wot_td::thing::FormOperation::InvokeAction)
                })
                .input(|b| {
                    b.finish_extend()
                        .object()
                        .property("level", true, |b| {
                            b.finish_extend()
                                .integer()
                                .minimum(0)
                                .maximum(100)
                                .unit("percent")
                        })
                        .property("duration", true, |b| {
                            b.finish_extend().integer().minimum(0).unit("seconds")
                        })
                })
        })
        .http_bind(addr)
        .build_servient()
        .expect("cannot build Thing Descriptor for the on-off switch");

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

    (
        handle_messages(thing, message_sender, message_receiver),
        axum_future,
    )
        .join()
        .await;
}

#[derive(Clone)]
struct AppState {
    message_sender: mpsc::Sender<Message>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredAction {
    #[serde(skip)]
    id: Uuid,
    #[serde(flatten)]
    ty: StoredActionType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", untagged)]
enum StoredActionType {
    Fade(ActionStatus<FadeActionInput>),
}

impl StoredActionType {
    fn to_fade_mut(&mut self) -> Option<&mut ActionStatus<FadeActionInput>> {
        match self {
            Self::Fade(fade) => Some(fade),
        }
    }

    fn into_output_href(self) -> Option<(Option<FadeActionInput>, Option<String>)> {
        match self {
            Self::Fade(ActionStatus { output, href, .. }) => Some((output, href)),
        }
    }
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActionStatus<Output> {
    #[serde(default)]
    status: ActionStatusStatus,
    output: Option<Output>,
    error: Option<HttpApiProblem>,
    href: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    time_requested: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    time_ended: Option<OffsetDateTime>,
}

#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
enum ActionStatusStatus {
    #[default]
    Pending,
    Running,
    Completed,
    Failed,
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
    async fn get_is_on(&self) -> bool {
        self.use_oneshot(Message::GetIsOn).await
    }

    #[inline]
    async fn set_is_on(&self, value: bool) {
        self.send_message(Message::SetIsOn(value)).await;
    }

    #[inline]
    async fn fade(&self, data: FadeActionInput) -> StoredAction {
        self.use_oneshot(|sender| {
            let FadeActionInput { level, duration } = data;
            let time_requested = OffsetDateTime::now_utc();
            let duration = Duration::from_secs(duration.into());
            Message::Fade {
                level,
                time_requested,
                duration,
                sender,
            }
        })
        .await
    }
}

#[derive(Debug)]
enum Message {
    GetIsOn(oneshot::Sender<bool>),
    SetIsOn(bool),
    Fade {
        level: u8,
        time_requested: OffsetDateTime,
        duration: Duration,
        sender: oneshot::Sender<StoredAction>,
    },
    CompleteAction(Uuid),
}

async fn handle_messages(
    thing: Thing,
    message_sender: mpsc::Sender<Message>,
    receiver: mpsc::Receiver<Message>,
) {
    enum Event {
        Message(Message),
        Tick,
        Stop,
    }

    let Thing {
        mut is_on,
        mut level,
        actions,
    } = thing;
    let mut actions = Arc::new(actions);

    let mut fader = Fader::default();
    let receiver_stream = ReceiverStream::new(receiver)
        .map(Event::Message)
        .chain(stream::once(ready(Event::Stop)));
    let interval_stream =
        IntervalStream::new(tokio::time::interval(Duration::from_millis(10))).map(|_| Event::Tick);

    let mut stream = (receiver_stream, interval_stream).merge();
    while let Some(event) = stream.next().await {
        match event {
            Event::Message(message) => {
                handle_message(
                    message,
                    &mut is_on,
                    &mut level,
                    &mut actions,
                    &mut fader,
                    &message_sender,
                )
                .await
            }
            Event::Tick => fader.tick(&mut level),
            Event::Stop => break,
        }
    }
}

async fn handle_message(
    message: Message,
    is_on: &mut bool,
    level: &mut u8,
    actions: &mut Arc<Vec<StoredAction>>,
    fader: &mut Fader,
    message_sender: &mpsc::Sender<Message>,
) {
    use Message::*;

    match message {
        GetIsOn(sender) => sender.send(*is_on).unwrap(),
        SetIsOn(value) => *is_on = value,
        Fade {
            level: fade_level,
            duration,
            time_requested,
            sender,
        } => {
            let fade_brightness = fade_level.min(100);
            let now = OffsetDateTime::now_utc();
            let real_duration = duration
                .saturating_sub((now - time_requested).try_into().unwrap_or(Duration::ZERO));
            let id = Uuid::new_v4();
            let href = format!("/actions/fade/{}", id);
            let fade_action = ActionStatus {
                output: Some(FadeActionInput {
                    level: fade_level,
                    duration: duration.as_secs().try_into().unwrap_or(u32::MAX),
                }),
                href: Some(href),
                time_requested: Some(time_requested),
                error: None,
                time_ended: None,
                status: ActionStatusStatus::Pending,
            };

            Arc::make_mut(actions).push(StoredAction {
                id,
                ty: StoredActionType::Fade(ActionStatus {
                    status: ActionStatusStatus::Pending,
                    ..fade_action.clone()
                }),
            });

            if is_on.not() {
                *level = 0;
                *is_on = true;
            }
            fader.fade(*level, fade_brightness, duration);

            let message_sender = message_sender.clone();
            tokio::spawn(async move {
                sleep(real_duration).await;
                if let Err(err) = message_sender.send(CompleteAction(id)).await {
                    tracing::warn!("unable to queue complete action: {err}");
                }
            });

            let action = StoredAction {
                id,
                ty: StoredActionType::Fade(fade_action),
            };
            sender.send(action).unwrap();
        }
        CompleteAction(id) => {
            let actions = Arc::make_mut(actions);
            let action = match actions.iter_mut().find(|action| action.id == id) {
                Some(action) => action,
                None => {
                    tracing::warn!("unable to complete action with id {id}");
                    return;
                }
            };

            let action = action.ty.to_fade_mut().unwrap();
            let now = OffsetDateTime::now_utc();
            action.time_ended = Some(now);
            action.status = ActionStatusStatus::Completed;
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Properties {
    brightness: u8,
    on: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Property {
    On(bool),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PropertyName {
    On,
}

async fn get_on_property(Extension(app): Extension<AppState>) -> Json<bool> {
    let is_on = app.get_is_on().await;
    Json(is_on)
}

async fn put_on_property(
    Json(value): Json<bool>,
    Extension(app): Extension<AppState>,
) -> impl IntoResponse {
    app.set_is_on(value).await;
    StatusCode::NO_CONTENT
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FadeActionInput {
    level: u8,
    duration: u32,
}

async fn post_fade_action(
    Json(input): Json<FadeActionInput>,
    Extension(state): Extension<AppState>,
) -> impl IntoResponse {
    let action = state.fade(input).await;
    let (output, href) = action.ty.into_output_href().unwrap();

    let response = ActionStatus {
        status: ActionStatusStatus::Pending,
        output: Some(output),
        error: None,
        href: href.clone(),
        time_requested: None,
        time_ended: None,
    };

    match href {
        Some(href) => (StatusCode::CREATED, [("location", href)], Json(response)).into_response(),
        None => (StatusCode::CREATED, Json(response)).into_response(),
    }
}

#[derive(Debug, Default)]
struct Fader {
    active: bool,
    tick: u32,
    ticks: u32,
    initial_level: u8,
    delta_level: i8,
}

impl Fader {
    fn tick(&mut self, level: &mut u8) {
        if self.active.not() {
            return;
        }

        let current_delta =
            (f64::from(self.delta_level) / f64::from(self.ticks) * f64::from(self.tick + 1)) as i8;
        // `as` is fine because brightness MUST be between 0 and 100.
        // This will be better when `saturating_add_signed` is stable.
        let new_level = (self.initial_level as i8)
            .saturating_add(current_delta)
            .clamp(0, 100) as u8;

        if *level != new_level {
            *level = new_level;
        }

        self.tick += 1;
        if self.tick >= self.ticks {
            self.active = false;
        }
    }

    fn fade(&mut self, current_level: u8, final_level: u8, duration: Duration) {
        self.active = true;
        self.tick = 0;
        // We are using 10ms ticks
        self.ticks = u32::try_from(duration.as_millis()).unwrap_or(u32::MAX) / 10;
        self.initial_level = current_level;
        self.delta_level = (final_level as i8) - (current_level as i8);
    }
}
