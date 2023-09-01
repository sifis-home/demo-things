use std::{
    future::Future,
    pin::Pin,
    task::{self, ready, Poll},
    vec,
};

use futures_util::{stream::FusedStream, Stream};
use pin_project_lite::pin_project;
use tokio::time::{Duration, Sleep};

pub trait Simulation {
    type Output;

    fn output_and_wait(self) -> (Self::Output, Duration);
}

pin_project! {
    #[project_replace = SimulationStreamProjReplace]
    pub struct SimulationStream<T: Simulation> {
        simulations: vec::IntoIter<T>,
        #[pin]
        next_status: Option<SimulationStreamNext<T::Output>>,
    }
}

pin_project! {
    #[project_replace = SimulationStreamNextProjReplace]
    struct SimulationStreamNext<T> {
        #[pin]
        sleep: Sleep,
        output: T,
    }
}

impl<T: Simulation> SimulationStream<T> {
    pub fn new(simulations: impl Into<vec::IntoIter<T>>) -> Self {
        let mut simulations = simulations.into();
        let next_status = simulations.next().map(SimulationStreamNext::from);

        Self {
            simulations,
            next_status,
        }
    }
}

impl<T> Stream for SimulationStream<T>
where
    T: Simulation,
{
    type Item = T::Output;

    fn poll_next(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        let Some(mut next_status) = this.next_status.as_mut().as_pin_mut() else {
            return Poll::Ready(None);
        };

        ready!(next_status.as_mut().project().sleep.poll(cx));
        let current_status = match this.simulations.next() {
            Some(next_simulation) => next_status.project_replace(next_simulation.into()).output,
            None => {
                // SAFETY:
                // We are manually project-replacing the pinned option. This is sound because the
                // `sleep` field is _pinned_ and it's not touched anymore (other than being
                // dropped), and the `output` is not pinned.
                //
                // The `unwrap` could theoretically be `unwrap_unchecked`, because the state of
                // `next_status` is `Some`. However, it should be optimized away and the code is
                // less dangerous.
                unsafe { this.next_status.get_unchecked_mut().take().unwrap().output }
            }
        };
        Poll::Ready(Some(current_status))
    }
}

impl<T> FusedStream for SimulationStream<T>
where
    T: Simulation,
{
    #[inline]
    fn is_terminated(&self) -> bool {
        self.next_status.is_none()
    }
}

impl<T> From<T> for SimulationStreamNext<T::Output>
where
    T: Simulation,
{
    fn from(value: T) -> Self {
        let (output, wait) = value.output_and_wait();
        let sleep = tokio::time::sleep(wait);
        Self { sleep, output }
    }
}
