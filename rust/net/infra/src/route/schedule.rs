//
// Copyright 2024 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::stream::{FusedStream, FuturesUnordered};
use futures_util::{Stream, StreamExt};
use pin_project::pin_project;
use tokio::time::{Duration, Instant};

use crate::dns::dns_utils::log_safe_domain;
use crate::dns::DnsError;
use crate::route::{ResolveHostnames, ResolvedRoute, Resolver};
use crate::utils::binary_heap::{MinKeyValueQueue, Queue};

/// Resolves routes with domain names to equivalent routes with IP addresses.
///
/// [`RouteResolver::resolve`] is the main entry point; this type exists mostly
/// to provide some named state that is used as input to that function.
pub struct RouteResolver {
    pub allow_ipv6: bool,
}

/// A policy object that decides how much to delay a route.
pub trait RouteDelayPolicy<R> {
    /// Given a route, how much should it be delayed by?
    fn compute_delay(&self, route: &R, now: Instant) -> Duration;
}

/// Metadata about a resolved route.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResolveMeta {
    /// The position of the un-resolved route in the source input stream.
    original_group_index: usize,
}

/// Schedules resolved routes for connection attempts in priority order.
///
/// This is notionally a stream, though it doesn't (yet) implement [`Stream`].
/// [`Schedule::next`] behaves like [`StreamExt::next`]. The output of calling
/// [`next`] is a sequence of routes to attempt connecting over, in the order in
/// which they should be tried.
///
/// Internally, this is implemented with two min-heaps: one that orders
/// [`ResolvedRoutes`] (groups of routes from the same unresolved route) by
/// their original order in the input, and a second that orders individual
/// routes by the time at which they should be attempted (based on the
/// [`RouteDelayPolicy`] provided).
#[derive(Debug)]
#[pin_project(project=ScheduleProj)]
pub struct Schedule<S, R, M, SP> {
    #[pin]
    resolver_stream: QueuedStream<SwapPairStream<S>, MinKeyValueQueue<M, ResolvedRoutes<R>>>,
    scoring_policy: SP,

    delayed_individual_routes: MinKeyValueQueue<Instant, R>,
    #[pin]
    individual_routes_sleep: tokio::time::Sleep,
}

/// Record of recent connection outcomes.
///
/// Implements [`RouteDelayPolicy`].
pub struct ConnectionOutcomes<R> {
    params: ConnectionOutcomeParams,
    recent_failures: Arc<HashMap<R, (Instant, u8)>>,
}

pub struct ConnectionOutcomeParams {
    pub age_cutoff: Duration,
    pub cooldown_growth_factor: f32,
    pub count_growth_factor: f32,
    pub max_count: u8,
    pub max_delay: Duration,
}

impl RouteResolver {
    /// Resolve an ordered sequence of routes with hostnames as a stream of
    /// resolved routes.
    ///
    /// Produces a sequence of [`ResolvedRoutes`] in roughly priority order by
    /// resolving the hostnames in each of the input routes. Each input route
    /// corresponds to a single `ResolvedRoutes` in the output, though not
    /// necessarily in the same order as the input sequence. The order is
    /// maintained as much as possible subject to delays in name resolution.
    pub fn resolve<'r, R>(
        &'r self,
        ordered_routes: impl Iterator<Item = R> + 'r,
        resolver: &'r impl Resolver,
    ) -> impl FusedStream<Item = (ResolvedRoutes<R::Resolved>, ResolveMeta)> + 'r
    where
        R: ResolveHostnames + Clone + 'static,
        R::Resolved: ResolvedRoute,
    {
        let Self { allow_ipv6 } = self;

        let resolved = eagerly_resolve_each(ordered_routes, resolver).filter_map(
            |(resolution_result, meta)| {
                std::future::ready(match resolution_result {
                    Ok(route_group) => Some((route_group, meta)),
                    Err((name, err)) => {
                        log::warn!(
                            "DNS resolution for {name} failed: {err}",
                            name = log_safe_domain(&name)
                        );
                        None
                    }
                })
            },
        );

        // Prune routes that connect directly to IPv6 addresses if necessary.
        resolved.map(|(mut routes, meta)| {
            if !*allow_ipv6 {
                routes
                    .routes
                    .retain(|route| route.immediate_target().is_ipv4())
            }
            (routes, meta)
        })
    }
}

impl<S, R, M, SP> Schedule<S, R, M, SP>
where
    M: Ord,
    S: FusedStream<Item = (ResolvedRoutes<R>, M)>,
    SP: RouteDelayPolicy<R>,
{
    pub fn new(resolver_stream: S, previous_attempts: SP) -> Self {
        Self {
            resolver_stream: QueuedStream::new(SwapPairStream(resolver_stream)),
            delayed_individual_routes: MinKeyValueQueue::new(),
            scoring_policy: previous_attempts,
            individual_routes_sleep: tokio::time::sleep(Duration::ZERO),
        }
    }

    /// Returns the next route to try, or `None` if all routes are exhausted.
    ///
    /// This is functionally [`StreamExt::next`], but this type doesn't (yet)
    /// implement [`Stream`]. See the type-level documentation for the order in
    /// which this will produce routes.
    pub async fn next(self: Pin<&mut Self>) -> Option<R> {
        let ScheduleProj {
            resolver_stream,
            delayed_individual_routes,

            scoring_policy,

            mut individual_routes_sleep,
        } = self.project();

        #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
        enum Event<T> {
            PulledFromResolver(T),
            ReturnNextIndividualRoute,
        }

        let mut resolver_stream =
            resolver_stream.filter(|value| std::future::ready(!value.1.routes.is_empty()));

        loop {
            let next_from_individual_routes = delayed_individual_routes.peek().map(|(time, _)| {
                individual_routes_sleep.as_mut().reset(*time);
                individual_routes_sleep.as_mut()
            });

            let pull_from_resolver_if_not_terminated =
                (!resolver_stream.is_terminated()).then_some(resolver_stream.next());

            if next_from_individual_routes.is_none()
                && pull_from_resolver_if_not_terminated.is_none()
            {
                return None;
            }
            let event = tokio::select! {
                () = SomeOrPending(next_from_individual_routes) => Event::ReturnNextIndividualRoute,
                route = SomeOrPending(pull_from_resolver_if_not_terminated) => Event::PulledFromResolver(route),
            };

            match event {
                Event::PulledFromResolver(Some(value)) => {
                    let (_penalty, routes) = value;
                    let now = Instant::now();
                    delayed_individual_routes.extend(routes.into_iter().enumerate().map(
                        |(i, r)| {
                            let delay = HAPPY_EYEBALLS_DELAY * u32::try_from(i).unwrap_or(u32::MAX)
                                + scoring_policy.compute_delay(&r, now);
                            (now + delay, r)
                        },
                    ));

                    // The routes queue was updated. Restart the loop so we can
                    // recompute the sleep timeouts.
                    continue;
                }
                Event::PulledFromResolver(None) => {
                    // We know for sure the resolver stream is terminated. Start
                    // the top of the loop again so we can check if the two
                    // queues are empty and we need to exit.
                    continue;
                }
                Event::ReturnNextIndividualRoute => {
                    let next = delayed_individual_routes
                        .pop()
                        .expect("non-empty checked earlier");
                    return Some(next.1);
                }
            }
        }
    }
}

pub struct AttemptOutcome {
    pub started: Instant,
    pub result: Result<(), UnsuccessfulOutcome>,
}

/// Unit type that represents a failure to connect.
///
/// Right now the cause of the failure is unimportant, though if that changes in
/// the future this should be made an `enum`.
pub struct UnsuccessfulOutcome;

impl<R: Hash + Eq + Clone> ConnectionOutcomes<R> {
    pub fn new(params: ConnectionOutcomeParams) -> Self {
        Self {
            params,
            recent_failures: Default::default(),
        }
    }

    /// Update the internal state with the results of completed connection attempts.
    pub fn apply_outcome_updates(
        &mut self,
        updates: impl IntoIterator<Item = (R, AttemptOutcome)>,
        now: Instant,
    ) {
        use std::collections::hash_map::Entry;

        let Self {
            params,
            recent_failures,
        } = self;
        // Get mutable access to our failures data, cloning it into a new owned
        // copy if it's currently borrowed.
        let recent_failures = Arc::make_mut(recent_failures);

        // Age out any old entries.
        recent_failures.retain(|_route, (last_time, _failure_count)| {
            now.saturating_duration_since(*last_time) < params.age_cutoff
        });

        for (route, outcome) in updates {
            let AttemptOutcome { started, result } = outcome;

            match result {
                Ok(()) => {
                    let _ = recent_failures.remove(&route);
                }
                Err(UnsuccessfulOutcome) => match recent_failures.entry(route) {
                    Entry::Occupied(mut entry) => {
                        let (when, count) = entry.get_mut();
                        *count = (*count + 1).min(params.max_count);
                        *when = started;
                    }
                    Entry::Vacant(entry) => {
                        entry.insert((started, 1));
                    }
                },
            }
        }
    }
}

impl<P: RouteDelayPolicy<R>, R> RouteDelayPolicy<R> for &P {
    fn compute_delay(&self, route: &R, now: Instant) -> Duration {
        P::compute_delay(self, route, now)
    }
}

/// Delay routes according to previous history, with some caps.
///
/// Delay routes according to the following rules:
/// - older failures cause less delay
/// - more consecutive failures cause more delay
/// - delay should increase exponentially with failure count
/// - absent any information there should be no delay
impl<R: Hash + Eq> RouteDelayPolicy<R> for ConnectionOutcomes<R> {
    fn compute_delay(&self, route: &R, now: Instant) -> Duration {
        let Self {
            recent_failures,
            params,
        } = self;

        let Some((when, count)) = recent_failures.get(route) else {
            return Duration::ZERO;
        };

        params.compute_delay(now.saturating_duration_since(*when), *count)
    }
}

impl ConnectionOutcomeParams {
    /// Compute the delay given the time since the last failure and count of
    /// repeated failures.
    ///
    /// The implementation is based on exponential backoff with a scaling factor
    /// based on the amount of time since the last known failure.
    fn compute_delay(
        &self,
        since_last_failure: Duration,
        consecutive_failure_count: u8,
    ) -> Duration {
        let Self {
            age_cutoff,
            cooldown_growth_factor,
            count_growth_factor,
            max_count,
            max_delay,
        } = *self;

        // Exponential backoff: as the count grows, the delay should be longer.
        //
        // This is equivalent to "normal exponential backoff" with a change of
        // constants. The usual formula is
        //
        //    t = min(T * (C**x - 1), D)
        //
        // where `x` is the number of failures, `C` is the exponential growth
        // constant, `T` is a time constant, and `D` is the maximum delay.
        //
        // We let `M` be the value of `x` past which the clamp applies, so
        // `D = T * (C**M - 1)`, and apply associativity of `min` to get
        //
        //    t = T * C**min(x, M) - 1
        //
        // We introduce a new constant `k` and substitute `C = k**(1/M)`:
        //
        //    t = T * k**( min(x, M) / M) - 1
        //
        // Then divide both sides by our original `D` to get the delay as a
        // fraction of the maximum value:
        //
        //    t/D = [ k ** ( min(x, M) / M) - 1 ] / [ k - 1]
        //
        // The right side is exactly the formula below, where `M` is the maximum
        // count and `k` is the growth factor. The upside of this formulation is
        // that it lets us specify the count value `M` at which the maximum
        // duration is achieved as an input instead of as a function of `T`,
        // `C`, and `D`.
        let count_factor = {
            let normalized_count =
                consecutive_failure_count.min(max_count) as f32 / max_count as f32;

            let numerator = count_growth_factor.powf(normalized_count) - 1.0;
            let denominator = count_growth_factor - 1.0;
            numerator / denominator
        };

        // Exponential decrease: as the age of the last failure increases, it
        // becomes less relevant and the delay is shorter.
        let age_factor = {
            // TODO use Duration::div_duration_f32 once MSRV >= 1.80
            let normalized_age =
                (since_last_failure.as_nanos() as f32 / age_cutoff.as_nanos() as f32).min(1.0);

            let numerator = cooldown_growth_factor - cooldown_growth_factor.powf(normalized_age);
            let denominator = cooldown_growth_factor - 1.0;
            numerator / denominator
        };

        // Combine the two factors so that if either one is zero, the whole
        // thing is zero.
        let factor = age_factor * count_factor;

        // Clamp the product as insurance since `Duration::mul_f32` panics if
        // the input is negative, and in case of rounding errors that would make
        // it > 1.
        max_delay.mul_f32(factor.clamp(0.0, 1.0))
    }
}

/// [`Stream`] that maps elements `(a, b)` in the wrapped stream to `(b, a)`.
#[pin_project]
#[derive(Clone, Debug)]
struct SwapPairStream<S>(#[pin] S);

impl<S: Stream<Item = (A, B)>, A, B> Stream for SwapPairStream<S> {
    type Item = (B, A);

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.project()
            .0
            .poll_next(cx)
            .map(|v| v.map(|(a, b)| (b, a)))
    }
}

impl<S: FusedStream<Item = (A, B)>, A, B> FusedStream for SwapPairStream<S> {
    fn is_terminated(&self) -> bool {
        self.0.is_terminated()
    }
}

/// [`Future`] that delegates to the inner `F: Future` or never resolves.
#[pin_project]
struct SomeOrPending<F>(#[pin] Option<F>);

impl<F: Future> Future for SomeOrPending<F> {
    type Output = F::Output;

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match self.project().0.as_pin_mut() {
            Some(f) => f.poll(cx),
            None => std::task::Poll::Pending,
        }
    }
}

const HAPPY_EYEBALLS_DELAY: Duration = Duration::from_millis(300);

/// A group of resolved routes that came from the same unresolved route.
#[derive(Clone, Debug)]
pub struct ResolvedRoutes<R> {
    routes: Vec<R>,
}

impl<R> IntoIterator for ResolvedRoutes<R> {
    type IntoIter = <Vec<R> as IntoIterator>::IntoIter;
    type Item = R;

    fn into_iter(self) -> Self::IntoIter {
        self.routes.into_iter()
    }
}

/// Produces a stream of resolved routes.
///
/// Resolves all the input routes in parallel.
fn eagerly_resolve_each<'r, R: ResolveHostnames + Clone + 'static>(
    routes: impl Iterator<Item = R> + 'r,
    resolver: &'r impl Resolver,
) -> impl FusedStream<
    Item = (
        Result<ResolvedRoutes<R::Resolved>, (Arc<str>, DnsError)>,
        ResolveMeta,
    ),
> + 'r {
    FuturesUnordered::from_iter(routes.enumerate().map(|(index, route)| async move {
        let resolution = super::resolve_route(resolver, route)
            .await
            .map(|routes| ResolvedRoutes {
                routes: routes.collect(),
            });

        (
            resolution,
            ResolveMeta {
                original_group_index: index,
            },
        )
    }))
}

/// A [`Stream`] that sorts the input as much as possible.
///
/// Wraps the input `Stream` in a stream that eagerly pulls from the input when
/// polled and reorders items by the provided sorting function. If cached items
/// are available, they are emitted first. When sorting, the minimal item is
/// emitted first.
#[derive(Debug)]
#[pin_project(project=TryPickMinProj)]
struct QueuedStream<S, Q> {
    #[pin]
    input: S,
    heap: Q,
}

impl<S: FusedStream, Q: Default> QueuedStream<S, Q> {
    fn new(input: S) -> Self {
        Self {
            input,
            heap: Default::default(),
        }
    }
}

impl<S: FusedStream, Q: Queue<Item = S::Item>> Stream for QueuedStream<S, Q> {
    type Item = S::Item;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let TryPickMinProj { mut input, heap } = self.as_mut().project();
        if input.is_terminated() {
            return std::task::Poll::Ready(heap.pop());
        }

        while let std::task::Poll::Ready(item) = input.as_mut().poll_next(cx) {
            match item {
                Some(item) => {
                    heap.push(item);
                }
                None => {
                    // There are no more pending items, so just return from
                    // the queue.
                    return std::task::Poll::Ready(heap.pop());
                }
            }
        }

        // All the results of already-finished pending futures are in the
        // queue. Return one of those if there is one.
        if let Some(item) = heap.pop() {
            return std::task::Poll::Ready(Some(item));
        }

        std::task::Poll::Pending
    }
}

impl<S: FusedStream<Item = T>, T, Q: Queue<Item = T>> FusedStream for QueuedStream<S, Q> {
    fn is_terminated(&self) -> bool {
        let Self { input, heap } = self;
        input.is_terminated() && heap.is_empty()
    }
}

#[cfg(test)]
mod test {
    use std::collections::{HashMap, HashSet};
    use std::net::IpAddr;

    use const_str::ip_addr;
    use itertools::Itertools as _;
    use proptest::proptest;

    use super::*;
    use crate::dns::lookup_result::LookupResult;
    use crate::route::UnresolvedHost;
    use crate::DnsSource;

    struct NoDelay;

    impl<R> RouteDelayPolicy<R> for NoDelay {
        fn compute_delay(&self, _route: &R, _now: Instant) -> Duration {
            Duration::ZERO
        }
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
    struct FakeRoute<A>(A);

    impl<A: ResolveHostnames> ResolveHostnames for FakeRoute<A> {
        type Resolved = FakeRoute<A::Resolved>;

        fn hostnames(&self) -> impl Iterator<Item = &UnresolvedHost> {
            self.0.hostnames()
        }

        fn resolve(self, lookup: impl FnMut(&str) -> IpAddr) -> Self::Resolved {
            FakeRoute(self.0.resolve(lookup))
        }
    }

    impl<A: ResolvedRoute> ResolvedRoute for FakeRoute<A> {
        fn immediate_target(&self) -> &IpAddr {
            self.0.immediate_target()
        }
    }

    impl<S, R, M, SP> Schedule<S, R, M, SP>
    where
        M: Ord,
        S: FusedStream<Item = (ResolvedRoutes<R>, M)>,
        SP: RouteDelayPolicy<R>,
    {
        pub fn as_stream<'a>(self: Pin<&'a mut Self>) -> impl Stream<Item = R> + 'a
        where
            M: 'a,
        {
            let schedule = self;
            futures_util::stream::unfold(schedule, |mut schedule| async {
                schedule.as_mut().next().await.map(|r| (r, schedule))
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn single_resolved_route_e2e() {
        let resolver = RouteResolver { allow_ipv6: true };
        let name_resolver = HashMap::from([(
            "domain-name",
            LookupResult {
                ipv4: vec![ip_addr!(v4, "1.2.3.4")],
                ipv6: vec![ip_addr!(v6, "::1234")],
                source: DnsSource::Static,
            },
        )]);

        let unresolved_routes = [FakeRoute(UnresolvedHost("domain-name".into()))];

        let resolve = resolver.resolve(unresolved_routes.into_iter(), &name_resolver);
        let schedule = Schedule::new(resolve.fuse(), NoDelay);

        let start_at = Instant::now();
        let schedule = std::pin::pin!(schedule);
        let schedule: Vec<_> = schedule
            .as_stream()
            .map(|r| (r, Instant::now().duration_since(start_at)))
            .collect()
            .await;

        assert_eq!(
            schedule,
            vec![
                (FakeRoute(ip_addr!("::1234")), Duration::ZERO),
                (FakeRoute(ip_addr!("1.2.3.4")), HAPPY_EYEBALLS_DELAY),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn multiple_resolved_routes_e2e() {
        let resolver = RouteResolver { allow_ipv6: true };

        let name_resolver = HashMap::from([
            (
                "name-1",
                LookupResult {
                    ipv4: vec![ip_addr!(v4, "1.2.3.4")],
                    ipv6: vec![ip_addr!(v6, "::1234")],
                    source: DnsSource::Static,
                },
            ),
            (
                "name-2",
                LookupResult {
                    ipv4: vec![ip_addr!(v4, "5.6.7.8")],
                    ipv6: vec![ip_addr!(v6, "::5678")],
                    source: DnsSource::Static,
                },
            ),
        ]);

        let unresolved_routes = [
            FakeRoute(UnresolvedHost("name-1".into())),
            FakeRoute(UnresolvedHost("name-2".into())),
        ];

        let resolve = resolver.resolve(unresolved_routes.into_iter(), &name_resolver);
        let schedule = Schedule::new(futures_util::StreamExt::fuse(resolve), NoDelay);

        let start_at = Instant::now();
        let schedule = std::pin::pin!(schedule);
        let schedule: Vec<_> = schedule
            .as_stream()
            .map(|r| (r, Instant::now().duration_since(start_at)))
            .collect()
            .await;

        // Compare with HashSet since the ordering isn't deterministic because
        // the DNS resolution is instantaneous.
        assert_eq!(
            HashSet::from_iter(schedule),
            HashSet::from([
                (FakeRoute(ip_addr!("::1234")), Duration::ZERO),
                (FakeRoute(ip_addr!("1.2.3.4")), HAPPY_EYEBALLS_DELAY),
                (FakeRoute(ip_addr!("::5678")), Duration::ZERO),
                (FakeRoute(ip_addr!("5.6.7.8")), HAPPY_EYEBALLS_DELAY),
            ])
        );
    }

    macro_rules! assert_in_range {
        ($v:expr, $range:expr) => {
            let v = $v;
            let range = $range;
            assert!(range.contains(&v), "{v:?} not in {range:?}");
        };
    }

    proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig { cases: 99, .. Default::default() })]

        #[test]
        fn connection_outcome_delay_bounds(cooldown_growth_factor in 1.1f32..100.0, count_growth_factor in 1.1f32..100.0) {
            const MAX_DELAY: Duration = Duration::from_secs(10);
            const AGE_CUTOFF: Duration = Duration::from_secs(100);
            const COUNT_CUTOFF: u8 = 5;

            let params = ConnectionOutcomeParams {
                age_cutoff: AGE_CUTOFF,
                cooldown_growth_factor,
                count_growth_factor,
                max_count: COUNT_CUTOFF,
                max_delay: MAX_DELAY,
            };

            // Lots of failures, the last one recent.
            assert_eq!(
                params.compute_delay(Duration::ZERO, COUNT_CUTOFF),
                MAX_DELAY
            );

            proptest!(|(count in 0..COUNT_CUTOFF)|{
                // Regardless of the count, the delay is zero if the information is
                // too old.
                assert_eq!(
                    params.compute_delay(AGE_CUTOFF, count),
                    Duration::ZERO
                );
            });

            proptest!(|(count in 0..COUNT_CUTOFF, age_seconds in 0..AGE_CUTOFF.as_secs())| {
                let delay = params.compute_delay(Duration::from_secs(age_seconds), count);
                // The delay should always be less than the configured max.
                assert_in_range!(delay, Duration::ZERO..MAX_DELAY);
            });
        }
    }

    impl<R: Hash + Eq + Clone> ConnectionOutcomes<R> {
        fn record_outcome(
            &mut self,
            route: R,
            started: Instant,
            connect_duration: Duration,
            result: Result<(), UnsuccessfulOutcome>,
        ) {
            self.apply_outcome_updates(
                [(route, AttemptOutcome { started, result })],
                started + connect_duration,
            )
        }
    }

    #[test]
    fn connection_outcomes_delays_failing_route() {
        const MAX_DELAY: Duration = Duration::from_secs(100);
        const AGE_CUTOFF: Duration = Duration::from_secs(1000);

        const MAX_COUNT: u8 = 5;
        let mut outcomes = ConnectionOutcomes::new(ConnectionOutcomeParams {
            age_cutoff: AGE_CUTOFF,
            cooldown_growth_factor: 2.0,
            count_growth_factor: 10.0,
            max_count: MAX_COUNT,
            max_delay: MAX_DELAY,
        });

        const ROUTE: &str = "route";
        let start = Instant::now();

        // Without any information, the delay should be zero.
        assert_eq!(outcomes.compute_delay(&ROUTE, start), Duration::ZERO);

        let mut delays = vec![];
        let mut now = start;
        for _ in 0..=MAX_COUNT {
            const CONNECT_DELAY: Duration = Duration::from_secs(10);
            // Record that the previous connection attempt failed after CONNECT_DELAY.
            outcomes.record_outcome(ROUTE, now, CONNECT_DELAY, Err(UnsuccessfulOutcome));
            now += CONNECT_DELAY;

            // Compute the new delay and "wait" for it to elapse before the next
            // connection attempt.
            let delay = outcomes.compute_delay(&ROUTE, now);
            delays.push(delay);
            now += delay;
        }

        assert_eq!(
            delays.iter().map(Duration::as_secs).collect_vec(),
            [6, 16, 32, 58, 99, 99]
        );
    }

    #[test]
    fn connection_outcomes_delays_decrease_over_time() {
        const MAX_DELAY: Duration = Duration::from_secs(100);
        const AGE_CUTOFF: Duration = Duration::from_secs(1000);
        const MAX_COUNT: u8 = 5;

        let mut outcomes = ConnectionOutcomes::new(ConnectionOutcomeParams {
            age_cutoff: AGE_CUTOFF,
            cooldown_growth_factor: 2.0,
            count_growth_factor: 10.0,
            max_count: MAX_COUNT,
            max_delay: MAX_DELAY,
        });

        const ROUTE: &str = "route";
        let start = Instant::now();
        outcomes.record_outcome(ROUTE, start, Duration::ZERO, Err(UnsuccessfulOutcome));

        let delays = (0..=5)
            .map(|i| {
                let when = start + Duration::from_secs(i * 200);
                outcomes.compute_delay(&ROUTE, when)
            })
            .collect_vec();

        assert_eq!(
            delays.iter().map(Duration::as_secs).collect_vec(),
            [6, 5, 4, 3, 1, 0]
        );
    }
}