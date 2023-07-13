mod simulation;

pub use simulation::{Simulation, SimulationStream};

use futures_util::{FutureExt, Stream};
use pin_project_lite::pin_project;
use serde::{Deserialize, Serialize};
use signal_hook_tokio::{Signals, SignalsInfo};
use tracing::error;
use wot_td::{builder::ThingBuilder, extend::ExtendableThing};

use std::{
    borrow::Borrow,
    ffi::c_int,
    future::Future,
    marker::PhantomData,
    mem,
    net::SocketAddr,
    ops::Not,
    path::PathBuf,
    pin::Pin,
    task::{self, ready, Poll},
};

use clap::Parser;
use tracing_subscriber::filter::EnvFilter;

/// SIFIS-Home wot-rust demo thing
///
/// It sets up a servient listening the requested port and address and
/// advertises itself via mDNS/DNS-SD.
#[derive(Debug, Parser)]
pub struct CliCommon {
    /// Listening port
    #[arg(short, long, default_value = "3000")]
    pub listen_port: u16,

    /// Binding address
    #[arg(short = 'a', long, default_value = "0.0.0.0")]
    pub bind_addr: std::net::Ipv4Addr,

    /// Verbosity, more output per occurrence
    #[arg(
        long,
        short = 'v',
        action = clap::ArgAction::Count,
        global = true,
    )]
    pub verbose: u8,

    /// The name of the host.
    ///
    /// When provided, it is used to create the `base` field inside the WoT description.
    #[arg(long)]
    pub host: Option<String>,

    /// Set the Thing id
    #[arg(long)]
    pub id: Option<String>,

    /// Set the Thing title
    #[arg(long)]
    pub title: Option<String>,
}

impl CliCommon {
    pub fn setup_tracing(&self) {
        let filter = match self.verbose {
            0 => "error",
            1 => "warn",
            2 => "info",
            3 => "debug",
            _ => "trace",
        };
        let filter = EnvFilter::try_from_default_env()
            .or_else(|_| EnvFilter::try_new(filter))
            .unwrap();

        tracing_subscriber::fmt().with_env_filter(filter).init()
    }

    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::from((self.bind_addr, self.listen_port))
    }

    pub fn set_thing_base<O: ExtendableThing, S>(
        &self,
        thing_builder: ThingBuilder<O, S>,
    ) -> ThingBuilder<O, S> {
        use std::fmt::Write;

        match &self.host {
            Some(host) => {
                let mut base = format!("http://{host}");
                if self.listen_port != 80 {
                    write!(base, ":{}", self.listen_port).unwrap();
                }
                base.push('/');
                thing_builder.base(base)
            }
            None => thing_builder,
        }
    }

    pub fn set_thing_id<O: ExtendableThing, S>(
        &self,
        thing_builder: ThingBuilder<O, S>,
    ) -> ThingBuilder<O, S> {
        match &self.id {
            Some(id) => thing_builder.id(id),
            None => thing_builder.id(uuid::Uuid::new_v4().urn().to_string()),
        }
    }

    pub fn title_or<'a>(&'a self, default_title: &'a str) -> &'a str {
        self.title.as_deref().unwrap_or(default_title)
    }
}

pub trait ThingBuilderExt {
    fn base_from_cli(self, cli: &CliCommon) -> Self;
    fn id_from_cli(self, cli: &CliCommon) -> Self;
}

impl<Other, Status> ThingBuilderExt for ThingBuilder<Other, Status>
where
    Other: ExtendableThing,
{
    #[inline]
    fn base_from_cli(self, cli: &CliCommon) -> Self {
        cli.set_thing_base(self)
    }
    #[inline]
    fn id_from_cli(self, cli: &CliCommon) -> Self {
        cli.set_thing_id(self)
    }
}

pub fn config_signal_loader<I, S, T>(
    signals: I,
    config_path: impl Into<PathBuf>,
) -> ConfigSignalLoader<T>
where
    I: IntoIterator<Item = S>,
    S: Borrow<c_int>,
{
    let signals = Signals::new(signals).expect("unable to create signal handlers");
    let handle = signals.handle();
    let config_path = config_path.into();
    let stream = ConfigSignalLoaderStream {
        signals,
        inner: ConfigSignalLoaderStreamInner::None(ConfigSignalLoaderStreamInnerStatus {
            config_path,
            handle: handle.clone(),
            _marker: PhantomData,
        }),
    };

    ConfigSignalLoader { stream, handle }
}

pub struct ConfigSignalLoader<T> {
    pub stream: ConfigSignalLoaderStream<T>,
    pub handle: signal_hook_tokio::Handle,
}

impl<T> Drop for ConfigSignalLoader<T> {
    fn drop(&mut self) {
        if self.handle.is_closed().not() {
            self.handle.close();
        }
    }
}

pin_project! {
    pub struct ConfigSignalLoaderStream<T> {
        #[pin]
        signals: SignalsInfo,
        inner: ConfigSignalLoaderStreamInner<T>,
    }
}

enum ConfigSignalLoaderStreamInner<T> {
    None(ConfigSignalLoaderStreamInnerStatus<T>),
    Future(Pin<Box<ConfigSignalLoaderStreamInnerFuture<T>>>),
    Invalid,
}

type ConfigSignalLoaderStreamInnerFuture<T> =
    dyn Future<Output = (Option<T>, ConfigSignalLoaderStreamInnerStatus<T>)>;

struct ConfigSignalLoaderStreamInnerStatus<T> {
    config_path: PathBuf,
    handle: signal_hook_tokio::Handle,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Stream for ConfigSignalLoaderStream<T>
where
    T: for<'de> Deserialize<'de> + 'static,
{
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            let this = self.as_mut().project();
            match this.inner.future_or_inner() {
                Err(inner) => {
                    if ready!(this.signals.poll_next(cx)).is_none() {
                        if inner.handle.is_closed().not() {
                            inner.handle.close();
                        }
                        return Poll::Ready(None);
                    };

                    let status = this.inner.take_none().unwrap();

                    let mut future = async move {
                        let raw_config = match tokio::fs::read(&status.config_path).await {
                            Ok(config) => config,
                            Err(err) => {
                                error!("unable to read config file: {err}");
                                status.handle.close();
                                return (None, status);
                            }
                        };

                        let new_config = match toml::from_slice::<T>(&raw_config) {
                            Ok(new_config) => Some(new_config),
                            Err(err) => {
                                error!("unable to parse config file: {err}");
                                status.handle.close();
                                None
                            }
                        };
                        (new_config, status)
                    }
                    .boxed();

                    match future.poll_unpin(cx) {
                        Poll::Ready((new_config, status)) => {
                            *this.inner = ConfigSignalLoaderStreamInner::None(status);
                            if let Some(new_config) = new_config {
                                return Poll::Ready(Some(new_config));
                            }
                        }
                        Poll::Pending => {
                            *this.inner = ConfigSignalLoaderStreamInner::Future(future);
                            return Poll::Pending;
                        }
                    }
                }
                Ok(mut future) => {
                    let (new_config, status) = ready!(future.poll_unpin(cx));
                    *this.inner = ConfigSignalLoaderStreamInner::None(status);
                    if let Some(new_config) = new_config {
                        return Poll::Ready(Some(new_config));
                    }
                }
            }
        }
    }
}

impl<T> ConfigSignalLoaderStreamInner<T> {
    fn future_or_inner(
        &mut self,
    ) -> Result<
        Pin<&mut ConfigSignalLoaderStreamInnerFuture<T>>,
        &ConfigSignalLoaderStreamInnerStatus<T>,
    > {
        match self {
            Self::None(ref inner) => Err(inner),
            Self::Future(future) => Ok(future.as_mut()),
            Self::Invalid => panic!("invalid ConfigSignalLoaderStream state"),
        }
    }

    fn take_none(&mut self) -> Option<ConfigSignalLoaderStreamInnerStatus<T>> {
        match self {
            Self::None(_) => match mem::replace(self, Self::Invalid) {
                Self::None(sender) => Some(sender),
                _ => unreachable!(),
            },
            _ => None,
        }
    }
}

pin_project! {
    #[derive(
        Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
    )]
    pub struct OptionStream<T> {
        #[pin]
        inner: OptionStreamInner<T>
    }
}

pin_project! {
    #[derive(
        Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
    )]
    #[project = OptionStreamProj]
    enum OptionStreamInner<T> {
        Some {
            #[pin]
            inner: T
        },
        #[default]
        None
    }
}

impl<T> Stream for OptionStream<T>
where
    T: Stream,
{
    type Item = T::Item;

    #[inline]
    fn poll_next(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().inner.poll_next(cx)
    }
}

impl<T> Stream for OptionStreamInner<T>
where
    T: Stream,
{
    type Item = T::Item;

    #[inline]
    fn poll_next(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();

        match this {
            OptionStreamProj::Some { inner } => inner.poll_next(cx),
            OptionStreamProj::None => Poll::Pending,
        }
    }
}

impl<T> From<Option<T>> for OptionStream<T> {
    #[inline]
    fn from(value: Option<T>) -> Self {
        OptionStream {
            inner: value.map_or(OptionStreamInner::None, |inner| OptionStreamInner::Some {
                inner,
            }),
        }
    }
}
