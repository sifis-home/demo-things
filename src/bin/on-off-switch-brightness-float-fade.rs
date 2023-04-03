use clap::Parser;
use demo_things::CliCommon;
use http_api_problem::HttpApiProblem;
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::{ops::Not, sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::{
    join, select,
    sync::{mpsc, oneshot},
    time::sleep,
};
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
    IntegerDataSchemaBuilderLike, NumberDataSchemaBuilderLike, ObjectDataSchemaBuilderLike,
    SpecializableDataSchema,
};

struct Thing {
    is_on: bool,
    brightness: f64,
    actions: Vec<StoredAction>,
}

const MESSAGE_QUEUE_LENGTH: usize = 16;

#[derive(Parser)]
struct Cli {
    #[clap(flatten)]
    common: CliCommon,

    /// Add the Light @type to the switch
    #[clap(short, long)]
    light: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    cli.common.setup_tracing();

    let thing = Thing {
        is_on: true,
        brightness: 50.,
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
        .property("brightness", |b| {
            b.finish_extend_data_schema()
                .attype("BrightnessProperty")
                .title("Brightness")
                .form(|b| {
                    b.href("/properties/brightness")
                        .http_get(get_brightness_property)
                        .http_put(put_brightness_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                        .op(wot_td::thing::FormOperation::WriteProperty)
                })
                .number()
                .minimum(0.)
                .maximum(100.)
                .unit("percent")
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

    servient.router = servient.router.layer(Extension(app_state));

    let axum_future = async {
        tracing::debug!("listening on {}", addr);
        servient
            .serve()
            .await
            .unwrap_or_else(|err| panic!("unable to create web server on address {addr}: {err}"));
    };

    join!(
        handle_messages(thing, message_sender, message_receiver),
        axum_future
    );
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
    async fn get_brightness(&self) -> f64 {
        self.use_oneshot(Message::GetBrightness).await
    }

    #[inline]
    async fn set_brightness(&self, value: f64) {
        self.send_message(Message::SetBrightness(value)).await;
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
    GetBrightness(oneshot::Sender<f64>),
    SetBrightness(f64),
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
    mut receiver: mpsc::Receiver<Message>,
) {
    let Thing {
        mut is_on,
        mut brightness,
        actions,
    } = thing;
    let mut actions = Arc::new(actions);

    let mut fader = Fader::default();
    let mut interval = tokio::time::interval(Duration::from_millis(10));

    loop {
        select! {
            message = receiver.recv() => {
                match message {
                    Some(message) => handle_message(
                        message,
                        &mut is_on,
                        &mut brightness,
                        &mut actions,
                        &mut fader,
                        &message_sender,
                    ).await,

                    None => break,
                }
            }

            _ = interval.tick() => {
                fader.tick(&mut brightness);
            }
        }
    }
}

async fn handle_message(
    message: Message,
    is_on: &mut bool,
    brightness: &mut f64,
    actions: &mut Arc<Vec<StoredAction>>,
    fader: &mut Fader,
    message_sender: &mpsc::Sender<Message>,
) {
    use Message::*;

    match message {
        GetIsOn(sender) => sender.send(*is_on).unwrap(),
        SetIsOn(value) => *is_on = value,
        GetBrightness(sender) => sender.send(*brightness).unwrap(),
        SetBrightness(value) => *brightness = value,
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
                *brightness = 0.;
                *is_on = true;
            }
            fader.fade(*brightness, fade_brightness.into(), duration);

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
    brightness: f64,
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

async fn get_brightness_property(Extension(app): Extension<AppState>) -> Json<f64> {
    let brightness = app.get_brightness().await;
    Json(brightness)
}

async fn put_brightness_property(
    Json(value): Json<f64>,
    Extension(app): Extension<AppState>,
) -> impl IntoResponse {
    app.set_brightness(value).await;
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
    initial_brightness: f64,
    delta_brightness: f64,
}

impl Fader {
    fn tick(&mut self, brightness: &mut f64) {
        if self.active.not() {
            return;
        }

        let current_delta =
            self.delta_brightness / f64::from(self.ticks) * f64::from(self.tick + 1);
        let new_brightness = (self.initial_brightness + current_delta).clamp(0., 100.);

        if *brightness != new_brightness {
            *brightness = new_brightness;
        }

        self.tick += 1;
        if self.tick >= self.ticks {
            self.active = false;
        }
    }

    fn fade(&mut self, current_brightness: f64, final_brightness: f64, duration: Duration) {
        self.active = true;
        self.tick = 0;
        // We are using 10ms ticks
        self.ticks = u32::try_from(duration.as_millis()).unwrap_or(u32::MAX) / 10;
        self.initial_brightness = current_brightness;
        self.delta_brightness = final_brightness - current_brightness;
    }
}
