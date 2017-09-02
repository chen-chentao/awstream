use super::{Adapt, AdaptSignal, Experiment};
use super::AsDatum;
use futures::Stream;
use futures::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, AtomicBool, Ordering};
use std::time::Duration;
use tokio_core::reactor::Handle;
use tokio_timer;

type AdaptControl = UnboundedSender<AdaptSignal>;
type DataChannel = UnboundedReceiver<AsDatum>;

pub type SourceCtrl = (AdaptControl, DataChannel, Arc<AtomicUsize>, Arc<AtomicBool>);

pub struct TimerSource;

/// `ProbeTracker` controls the probing behavior. The core function is `next`
/// that returns an `Option<AsDatum>`, it is either a probe datum, or indicates
/// the probing has done.
///
/// Probing is evenly spaced in each tick within a second. So complication of
/// this data type is due to the calculation of a proper rate. See `start_probe`
/// for details.
struct ProbeTracker {
    /// We need to know the tick_period to calculate how large each probe packet
    /// is for a even distribution.
    pub tick_period: u64,

    /// The target probe bandwidth.
    pub target_in_kbps: f64,

    /// The target pace, i.e. packet size for each tick. This is derived from
    /// `target_in_kbps`.
    pub target_pace: usize,

    /// The pace, i.e. the current packet size for each tick.
    pub pace: usize,

    /// Step in each `inc_pace`.
    pub delta: usize,
}

const NUM_PROBE_REQUIRED: usize = 5;

impl ProbeTracker {
    fn new(tick_period: u64) -> ProbeTracker {
        ProbeTracker {
            tick_period: tick_period,
            target_in_kbps: 0.0,
            target_pace: 0,
            delta: 0,
            pace: 0,
        }
    }

    pub fn start_probe(&mut self, additional_kbps: f64) {
        self.target_in_kbps = additional_kbps;

        let bytes_per_sec = self.target_in_kbps * 1000.0 / 8.0;
        let ticks_per_sec = 1000.0 / self.tick_period as f64;
        let size_per_tick = bytes_per_sec / ticks_per_sec;
        self.target_pace = size_per_tick as usize;

        self.delta = self.target_pace / NUM_PROBE_REQUIRED;
        self.pace = self.delta;
    }

    /// Probing is the additive increase phase (as AIMD in TCP).
    pub fn inc_pace(&mut self) -> bool {
        if self.pace < self.target_pace {
            self.pace = self.pace + self.delta;
            true
        } else {
            false
        }
    }

    pub fn stop_probe(&mut self) {
        self.target_in_kbps = 0.0;
        self.target_pace = 0;
        self.pace = 0;
        self.delta = 0;
    }

    fn next(&self) -> Option<AsDatum> {
        if self.target_pace > 0 {
            Some(AsDatum::probe(self.pace))
        } else {
            None
        }
    }
}

enum Incoming {
    Timer,
    Adapt(AdaptSignal),
}

impl TimerSource {
    pub fn spawn<As: Adapt + Experiment + 'static>(mut source: As, handle: Handle) -> SourceCtrl {
        let timer_tick = source.period_in_ms();
        let timer = tokio_timer::wheel()
            .tick_duration(Duration::from_millis(1))
            .build()
            .interval(Duration::from_millis(timer_tick))
            .map_err(|_e| ())
            .map(|_e| Incoming::Timer);

        let (adapt_tx, adapt_rx) = unbounded();
        let adapter = adapt_rx.map(|level| Incoming::Adapt(level));

        let (data_tx, data_rx) = unbounded();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let mut prober = ProbeTracker::new(timer_tick);
        let probe_done = Arc::new(AtomicBool::new(false));
        let probe_done_clone = probe_done.clone();

        let work = timer.select(adapter).for_each(
            move |incoming| match incoming {
                Incoming::Timer => {
                    let size = source.next_datum();
                    if size == 0 {
                        return Ok(());
                    }

                    if let Some(p) = prober.next() {
                        counter_clone.clone().fetch_add(p.len(), Ordering::SeqCst);
                        data_tx
                            .unbounded_send(p)
                            .map(|_| ())
                            .map_err(|_| ())
                            .expect("failed to send probing packet");
                    }

                    let level = source.current_level();
                    let data_to_send = AsDatum::new(level, vec![0; size]);
                    info!("add new data {}", data_to_send.len());
                    counter_clone.clone().fetch_add(
                        data_to_send.len(),
                        Ordering::SeqCst,
                    );
                    data_tx.unbounded_send(data_to_send).map(|_| ()).map_err(
                        |_| (),
                    )
                }
                Incoming::Adapt(AdaptSignal::ToRate(rate)) => {
                    source.adapt(rate);
                    Ok(())
                }
                Incoming::Adapt(AdaptSignal::DecreaseDegradation) => {
                    source.dec_degradation();
                    Ok(())
                }
                Incoming::Adapt(AdaptSignal::StartProbe(target_in_kbps)) => {
                    prober.start_probe(target_in_kbps);
                    Ok(())
                }
                Incoming::Adapt(AdaptSignal::IncreaseProbePace) => {
                    if !prober.inc_pace() {
                        probe_done_clone.clone().store(true, Ordering::SeqCst);
                    }
                    Ok(())
                }
                Incoming::Adapt(AdaptSignal::StopProbe) => {
                    prober.stop_probe();
                    Ok(())
                }
            },
        );
        handle.spawn(work);

        (adapt_tx, data_rx, counter.clone(), probe_done)
    }
}
