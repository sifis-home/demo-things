use futures_concurrency::{future::Join, stream::Merge};
use futures_util::stream;
use http_api_problem::HttpApiProblem;

use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use sifis_td::Sifis;
use std::{collections::HashMap, future::ready, ops::Not, sync::Arc, time::Duration};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::{
    sync::{broadcast, mpsc, oneshot},
    time::sleep,
};
use tokio_stream::{
    wrappers::{BroadcastStream, IntervalStream, ReceiverStream},
    Stream, StreamExt,
};
use tower_http::cors::CorsLayer;
use tracing::warn;
use uuid::Uuid;
use wot_serve::{
    servient::{BuildServient, HttpRouter, ServientSettings},
    Servient,
};

use axum::{
    extract::Path,
    http::StatusCode,
    response::{sse, sse::KeepAlive, IntoResponse, Json, Sse},
    Extension,
};
use wot_td::builder::{
    BuildableDataSchema, BuildableHumanReadableInfo, BuildableInteractionAffordance,
    IntegerDataSchemaBuilderLike, ObjectDataSchemaBuilderLike, SpecializableDataSchema,
};

use clap::Parser;
use demo_things::{CliCommon, ThingBuilderExt};

struct Lamp {
    is_on: bool,
    brightness: u8,
    actions: Vec<StoredAction>,
    temperature: u16,
}

const MESSAGE_QUEUE_LENGTH: usize = 16;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = CliCommon::parse();

    cli.setup_tracing();

    let lamp = Lamp {
        is_on: true,
        brightness: 50,
        actions: Default::default(),
        temperature: 25,
    };

    let (message_sender, message_receiver) = mpsc::channel(MESSAGE_QUEUE_LENGTH);
    let (event_sender, _event_receiver) = broadcast::channel(MESSAGE_QUEUE_LENGTH);

    let app_state = AppState {
        message_sender: message_sender.clone(),
        event_sender: event_sender.clone(),
    };

    let addr = cli.socket_addr();
    let mut servient = Servient::builder("My Lamp")
        .ext(create_sifis())
        .finish_extend()
        .id("urn:dev:ops:my-lamp-1234")
        .attype("OnOffSwitch")
        .attype("Light")
        .base_from_cli(&cli)
        .description("A web connected lamp")
        .security(|b| b.no_sec().with_key("nosec_sc").required())
        .form(|b| {
            b.ext(())
                .href("/properties")
                .http_get(properties)
                .content_type("application/json")
                .op(wot_td::thing::FormOperation::ReadAllProperties)
        })
        .form(|b| {
            b.ext(())
                .href("/properties/observe")
                .http_get(observe_properties)
                .op(wot_td::thing::FormOperation::ObserveAllProperties)
                .op(wot_td::thing::FormOperation::UnobserveAllProperties)
                .subprotocol("sse")
        })
        .form(|b| {
            b.ext(())
                .href("/actions")
                .http_get(get_actions)
                .content_type("application/json")
                .op(wot_td::thing::FormOperation::QueryAllActions)
        })
        .form(|b| {
            b.ext(())
                .href("/events")
                .http_get(all_events)
                .op(wot_td::thing::FormOperation::SubscribeAllEvents)
                .op(wot_td::thing::FormOperation::UnsubscribeAllEvents)
                .subprotocol("sse")
        })
        .property("on", |b| {
            b.ext(())
                .ext_interaction(())
                .ext_data_schema(())
                .finish_extend_data_schema()
                .attype("OnOffProperty")
                .title("On/Off")
                .description("Whether the lamp is turned on")
                .form(|b| {
                    b.ext(())
                        .href("/properties/on")
                        .http_get(get_on_property)
                        .http_put(put_on_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                        .op(wot_td::thing::FormOperation::WriteProperty)
                })
                .bool()
        })
        .property("brightness", |b| {
            b.ext(())
                .ext_interaction(())
                .ext_data_schema(())
                .finish_extend_data_schema()
                .attype("BrightnessProperty")
                .title("Brightness")
                .description("The level of light from 0-100")
                .form(|b| {
                    b.ext(())
                        .href("/properties/brightness")
                        .http_get(get_brightness_property)
                        .http_put(put_brightness_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                        .op(wot_td::thing::FormOperation::WriteProperty)
                })
                .form(|b| {
                    b.ext(())
                        .href("/properties/brightness/observe")
                        .http_get(observe_brightness)
                        .op(wot_td::thing::FormOperation::ObserveProperty)
                        .op(wot_td::thing::FormOperation::UnobserveProperty)
                        .subprotocol("sse")
                })
                .observable(true)
                .integer()
                .minimum(0)
                .maximum(100)
                .unit("percent")
        })
        .action("fade", |b| {
            b.ext(())
                .ext_interaction(())
                .title("Fade")
                .description("Fade the lamp to a given level")
                .form(|b| {
                    b.ext(())
                        .href("/actions/fade")
                        .http_post(post_fade_action)
                        .op(wot_td::thing::FormOperation::InvokeAction)
                })
                .form(|b| {
                    b.ext(())
                        .href("/actions/fade/{action_id}")
                        .http_get(get_fade_action)
                        .op(wot_td::thing::FormOperation::QueryAction)
                        .http_delete(delete_fade_action)
                        .op(wot_td::thing::FormOperation::CancelAction)
                })
                .uri_variable("action_id", |b| {
                    b.ext(())
                        .finish_extend()
                        .description("Identifier of the ongoing action")
                        .string()
                })
                .input(|b| {
                    b.ext(())
                        .finish_extend()
                        .object()
                        .property("brightness", true, |b| {
                            b.ext(())
                                .finish_extend()
                                .integer()
                                .minimum(0)
                                .maximum(100)
                                .unit("percent")
                        })
                        .property("duration", true, |b| {
                            b.ext(())
                                .finish_extend()
                                .integer()
                                .minimum(1)
                                .unit("milliseconds")
                        })
                })
        })
        .event("overheated", |b| {
            b.ext(())
                .ext_interaction(())
                .description("The lamp has exceeded its safe operating temperature")
                .form(|b| {
                    b.ext(())
                        .href("/events/overheated")
                        .http_get(overheated_events)
                        .op(wot_td::thing::FormOperation::SubscribeEvent)
                        .op(wot_td::thing::FormOperation::UnsubscribeEvent)
                        .subprotocol("sse")
                })
                .data(|b| b.ext(()).finish_extend().number().unit("degree celsius"))
        })
        .http_bind(addr)
        .build_servient()
        .expect("cannot build Thing Descriptor for the lamp");

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
        handle_messages(lamp, message_sender, message_receiver, event_sender),
        axum_future,
    )
        .join()
        .await;
}

#[derive(Clone)]
struct AppState {
    message_sender: mpsc::Sender<Message>,
    event_sender: broadcast::Sender<Event>,
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
    async fn get_brightness(&self) -> u8 {
        self.use_oneshot(Message::GetBrightness).await
    }

    #[inline]
    async fn get_is_on(&self) -> bool {
        self.use_oneshot(Message::GetIsOn).await
    }

    #[inline]
    async fn set_brightness(&self, value: u8) {
        self.send_message(Message::SetBrightness(value)).await;
    }

    #[inline]
    async fn set_is_on(&self, value: bool) {
        self.send_message(Message::SetIsOn(value)).await;
    }

    #[inline]
    async fn get_actions(&self) -> Arc<Vec<StoredAction>> {
        self.use_oneshot(Message::GetActions).await
    }

    #[inline]
    async fn delete_action(&self, name: ActionName, id: Uuid) -> bool {
        self.use_oneshot(|sender| Message::DeleteAction { name, id, sender })
            .await
    }

    #[inline]
    async fn fade(&self, data: FadeActionInput) -> StoredAction {
        self.use_oneshot(|sender| {
            let FadeActionInput {
                brightness,
                duration,
            } = data;
            let time_requested = OffsetDateTime::now_utc();
            let duration = Duration::from_millis(duration);
            Message::Fade {
                brightness,
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
    GetProperties(oneshot::Sender<Properties>),
    GetBrightness(oneshot::Sender<u8>),
    GetIsOn(oneshot::Sender<bool>),
    SetBrightness(u8),
    SetIsOn(bool),
    GetActions(oneshot::Sender<Arc<Vec<StoredAction>>>),
    DeleteAction {
        sender: oneshot::Sender<bool>,
        name: ActionName,
        id: Uuid,
    },
    Fade {
        brightness: u8,
        time_requested: OffsetDateTime,
        duration: Duration,
        sender: oneshot::Sender<StoredAction>,
    },
    CompleteAction(Uuid),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
enum Event {
    Overheated {
        data: u16,
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },

    Brightness {
        data: u8,
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },
}

async fn handle_messages(
    lamp: Lamp,
    message_sender: mpsc::Sender<Message>,
    receiver: mpsc::Receiver<Message>,
    event_sender: broadcast::Sender<Event>,
) {
    enum Event {
        Message(Message),
        Tick,
        Stop,
    }

    let Lamp {
        mut is_on,
        mut brightness,
        actions,
        mut temperature,
    } = lamp;
    let mut actions = Arc::new(actions);

    let receiver_stream = ReceiverStream::new(receiver)
        .map(Event::Message)
        .chain(stream::once(ready(Event::Stop)));
    let interval_stream =
        IntervalStream::new(tokio::time::interval(Duration::from_millis(10))).map(|_| Event::Tick);
    let mut fader = Fader::default();

    let mut stream = (receiver_stream, interval_stream).merge();
    while let Some(event) = stream.next().await {
        match event {
            Event::Message(message) => {
                handle_message(
                    message,
                    &mut is_on,
                    &mut brightness,
                    &mut actions,
                    &mut fader,
                    &message_sender,
                    &event_sender,
                )
                .await
            }
            Event::Tick => handle_tick(
                is_on,
                &mut brightness,
                &mut temperature,
                &mut fader,
                &event_sender,
            ),
            Event::Stop => break,
        }
    }
}

fn handle_tick(
    is_on: bool,
    brightness: &mut u8,
    temperature: &mut u16,
    fader: &mut Fader,
    event_sender: &broadcast::Sender<Event>,
) {
    fader.tick(brightness, event_sender);

    if is_on && *brightness > 50 {
        if *temperature < 200 {
            *temperature = temperature.saturating_add(1);
        }
    } else if *temperature > 25 {
        *temperature -= 1;
    }

    let temperature = *temperature;
    if temperature > 100 {
        event_sender
            .send(Event::Overheated {
                data: temperature,
                timestamp: OffsetDateTime::now_utc(),
            })
            .expect("events channel should be open");
    }
}

async fn handle_message(
    message: Message,
    is_on: &mut bool,
    brightness: &mut u8,
    actions: &mut Arc<Vec<StoredAction>>,
    fader: &mut Fader,
    message_sender: &mpsc::Sender<Message>,
    event_sender: &broadcast::Sender<Event>,
) {
    use Message::*;

    match message {
        GetProperties(sender) => {
            let properties = Properties {
                brightness: *brightness,
                on: *is_on,
            };
            sender.send(properties).unwrap();
        }
        GetBrightness(sender) => sender.send(*brightness).unwrap(),
        GetIsOn(sender) => sender.send(*is_on).unwrap(),
        SetBrightness(value) => {
            *brightness = value;
            if let Err(err) = event_sender.send(Event::Brightness {
                data: value,
                timestamp: OffsetDateTime::now_utc(),
            }) {
                warn!("unable to send brightness event: {err}");
            }
        }
        SetIsOn(value) => *is_on = value,
        GetActions(sender) => sender.send(Arc::clone(actions)).unwrap(),
        DeleteAction { sender, name, id } => {
            let filter = match name {
                ActionName::Fade => StoredAction::to_fade,
            };

            let index = actions.iter().position(|action| {
                filter(action)
                    .map(|action| action.id == id)
                    .unwrap_or(false)
            });

            let removed = match index {
                Some(index) => {
                    Arc::make_mut(actions).swap_remove(index);
                    true
                }
                None => false,
            };
            sender.send(removed).unwrap()
        }
        Fade {
            brightness: fade_brightness,
            duration,
            time_requested,
            sender,
        } => {
            let fade_brightness = fade_brightness.min(100);
            let now = OffsetDateTime::now_utc();
            let real_duration = duration
                .saturating_sub((now - time_requested).try_into().unwrap_or(Duration::ZERO));
            let id = Uuid::new_v4();
            let href = format!("/actions/fade/{}", id);
            let fade_action = ActionStatus {
                output: Some(FadeActionInput {
                    brightness: fade_brightness,
                    duration: duration.as_millis().try_into().unwrap_or(u64::MAX),
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
                *brightness = 0;
                if event_sender
                    .send(Event::Brightness {
                        data: 0,
                        timestamp: now,
                    })
                    .is_err()
                {
                    warn!("unable to send brightness event when starting a fade action with the lump off");
                }
                *is_on = true;
            }
            fader.fade(*brightness, fade_brightness, duration);

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

async fn properties(Extension(state): Extension<AppState>) -> Json<Properties> {
    let properties = state.get_properties().await;

    Json(properties)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Property {
    Brightness(u8),
    On(bool),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PropertyName {
    Brightness,
    On,
}

fn handle_sse_stream<F>(
    Extension(state): Extension<AppState>,
    mut f: F,
) -> Sse<impl Stream<Item = Result<sse::Event, serde_json::Error>>>
where
    F: FnMut(Event) -> Result<Option<sse::Event>, serde_json::Error> + Send + 'static,
{
    let receiver = BroadcastStream::new(state.event_sender.subscribe())
        .filter_map(move |ev| ev.ok().and_then(|v| f(v).transpose()));

    Sse::new(receiver).keep_alive(KeepAlive::default())
}

fn handle_overheated_event(event: Event) -> Result<Option<sse::Event>, serde_json::Error> {
    match event {
        Event::Overheated { data, timestamp } => {
            let mut event = sse::Event::default().event("overheated").json_data(data)?;
            if let Ok(timestamp) = timestamp.format(&Rfc3339) {
                event = event.id(timestamp);
            }
            Ok(Some(event))
        }
        _ => Ok(None),
    }
}

#[inline]
async fn all_events(
    extension: Extension<AppState>,
) -> Sse<impl Stream<Item = Result<sse::Event, serde_json::Error>>> {
    handle_sse_stream(extension, handle_overheated_event)
}

#[inline]
async fn overheated_events(
    extension: Extension<AppState>,
) -> Sse<impl Stream<Item = Result<sse::Event, serde_json::Error>>> {
    handle_sse_stream(extension, handle_overheated_event)
}

async fn get_actions(Extension(state): Extension<AppState>) -> impl IntoResponse {
    let mut actions = HashMap::<_, Vec<_>>::new();
    for action in &*state.get_actions().await {
        actions
            .entry(action.ty.name())
            .or_default()
            .push(action.clone());
    }

    Json(actions)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FadeActionInput {
    brightness: u8,
    duration: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredAction {
    #[serde(skip)]
    id: Uuid,
    #[serde(flatten)]
    ty: StoredActionType,
}

impl StoredAction {
    fn to_fade(&self) -> Option<&Self> {
        match &self.ty {
            StoredActionType::Fade(_) => Some(self),
        }
    }
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

    fn name(&self) -> &'static str {
        match self {
            Self::Fade(_) => "fade",
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

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum ActionName {
    Fade,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum EventName {
    Overheated,
}

async fn get_on_property(Extension(app): Extension<AppState>) -> Json<bool> {
    let is_on = app.get_is_on().await;
    Json(is_on)
}

async fn put_on_property(
    Extension(app): Extension<AppState>,
    Json(value): Json<bool>,
) -> impl IntoResponse {
    app.set_is_on(value).await;
    StatusCode::NO_CONTENT
}

async fn get_brightness_property(Extension(app): Extension<AppState>) -> Json<u8> {
    let brightness = app.get_brightness().await;
    Json(brightness)
}

async fn put_brightness_property(
    Extension(app): Extension<AppState>,
    Json(value): Json<u8>,
) -> impl IntoResponse {
    app.set_brightness(value).await;
    StatusCode::NO_CONTENT
}

fn handle_brightness_stream(event: Event) -> Result<Option<sse::Event>, serde_json::Error> {
    match event {
        Event::Brightness { data, timestamp } => {
            let mut event = sse::Event::default().event("brightness").json_data(data)?;
            if let Ok(timestamp) = timestamp.format(&Rfc3339) {
                event = event.id(timestamp);
            }
            Ok(Some(event))
        }
        _ => Ok(None),
    }
}

#[inline]
async fn observe_brightness(
    extension: Extension<AppState>,
) -> Sse<impl Stream<Item = Result<sse::Event, serde_json::Error>>> {
    handle_sse_stream(extension, handle_brightness_stream)
}

#[inline]
async fn observe_properties(
    extension: Extension<AppState>,
) -> Sse<impl Stream<Item = Result<sse::Event, serde_json::Error>>> {
    handle_sse_stream(extension, handle_brightness_stream)
}

async fn get_fade_action(
    Path(id): Path<Uuid>,
    Extension(app): Extension<AppState>,
) -> impl IntoResponse {
    let actions = app.get_actions().await;
    match actions
        .iter()
        .filter_map(StoredAction::to_fade)
        .find(|action| action.id == id)
    {
        Some(action) => {
            let StoredActionType::Fade(fade_action) = &action.ty;
            let fade_out = match fade_action.status {
                ActionStatusStatus::Pending | ActionStatusStatus::Running => ActionStatus {
                    status: fade_action.status,
                    output: None,
                    error: None,
                    href: fade_action.href.clone(),
                    time_requested: fade_action.time_requested,
                    time_ended: None,
                },
                ActionStatusStatus::Completed => ActionStatus {
                    status: fade_action.status,
                    output: fade_action.output,
                    error: None,
                    href: fade_action.href.clone(),
                    time_requested: fade_action.time_requested,
                    time_ended: fade_action.time_ended,
                },
                ActionStatusStatus::Failed => ActionStatus {
                    status: fade_action.status,
                    output: None,
                    error: fade_action.error.clone(),
                    href: fade_action.href.clone(),
                    time_requested: fade_action.time_requested,
                    time_ended: fade_action.time_ended,
                },
            };
            (
                StatusCode::OK,
                Json(Some(StoredAction {
                    id,
                    ty: StoredActionType::Fade(fade_out),
                })),
            )
        }
        None => (StatusCode::NOT_FOUND, Json(None)),
    }
}

async fn post_fade_action(
    Extension(state): Extension<AppState>,
    Json(input): Json<FadeActionInput>,
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

async fn delete_fade_action(
    Path(id): Path<Uuid>,
    Extension(state): Extension<AppState>,
) -> impl IntoResponse {
    match state.delete_action(ActionName::Fade, id).await {
        true => StatusCode::NO_CONTENT,
        false => StatusCode::NOT_FOUND,
    }
}

// Not Darth
#[derive(Debug, Default)]
struct Fader {
    active: bool,
    tick: u32,
    ticks: u32,
    initial_brightness: u8,
    delta_brightness: i8,
}

impl Fader {
    fn tick(&mut self, brightness: &mut u8, event_sender: &broadcast::Sender<Event>) {
        if self.active.not() {
            return;
        }

        let current_delta = (f64::from(self.delta_brightness) / f64::from(self.ticks)
            * f64::from(self.tick + 1)) as i8;
        // `as` is fine because brightness MUST be between 0 and 100.
        // This will be better when `saturating_add_signed` is stable.
        let new_brightness = (self.initial_brightness as i8)
            .saturating_add(current_delta)
            .clamp(0, 100) as u8;

        if *brightness != new_brightness {
            *brightness = new_brightness;
            let timestamp = OffsetDateTime::now_utc();
            if event_sender
                .send(Event::Brightness {
                    data: new_brightness,
                    timestamp,
                })
                .is_err()
            {
                warn!("cannot send brightness event during fading tick");
            }
        }

        self.tick += 1;
        if self.tick >= self.ticks {
            self.active = false;
        }
    }

    fn fade(&mut self, current_brightness: u8, final_brightness: u8, duration: Duration) {
        self.active = true;
        self.tick = 0;
        // We are using 10ms ticks
        self.ticks = u32::try_from(duration.as_millis()).unwrap_or(u32::MAX) / 10;
        self.initial_brightness = current_brightness;
        self.delta_brightness = (final_brightness as i8) - (current_brightness as i8);
    }
}

fn create_sifis() -> Sifis {
    Sifis::builder()
        .electric_energy_consumption(1, |cond| cond.when("/properties/on").eq(true))
        .electric_energy_consumption(2, |cond| {
            cond.when("/properties/on")
                .eq(true)
                .and("/properties/brightness")
                .ge(30)
        })
        .electric_energy_consumption(3, |cond| {
            cond.when("/properties/on")
                .eq(true)
                .and("/properties/brightness")
                .ge(60)
        })
        .electric_energy_consumption(3, |cond| {
            cond.when("/properties/on")
                .eq(true)
                .and("/properties/brightness")
                .ge(90)
        })
        .scald(1, |cond| {
            cond.when("/properties/on")
                .eq(true)
                .and("/properties/brightness")
                .ge(30)
        })
        .scald(2, |cond| {
            cond.when("/properties/on")
                .eq(true)
                .and("/properties/brightness")
                .ge(60)
        })
        .build()
}
