use super::{GAIN, LEDBAT_MIN_CWND, LEDBAT_TARGET_DELAY, LedbatController};
use std::collections::VecDeque;

const MSS: u32 = 1400;
const RTT_US: u64 = 100_000;
const BASE_DELAY_US: u64 = 40_000;
const CAPACITY_PER_RTT: u64 = 100_000;
const QUEUE_LIMIT_BYTES: u64 = 2 * CAPACITY_PER_RTT;
const BASE_HISTORY_MINUTES: usize = 10;

#[derive(Debug)]
struct ResultSummary {
    final_cwnd: u32,
    average_cwnd: u64,
    average_delivered_per_rtt: u64,
    max_queue_delay_us: u64,
    loss_rounds: u64,
}

#[derive(Debug)]
struct RfcReference {
    cwnd: u64,
    bytes_in_flight: u64,
    current_delays: VecDeque<u64>,
    base_delays: Vec<u64>,
    last_minute: Option<u64>,
    target_us: u64,
}

impl RfcReference {
    fn new() -> Self {
        Self {
            cwnd: u64::from(LEDBAT_MIN_CWND * MSS),
            bytes_in_flight: 0,
            current_delays: VecDeque::from([0]),
            base_delays: vec![u64::MAX; BASE_HISTORY_MINUTES],
            last_minute: None,
            target_us: LEDBAT_TARGET_DELAY.as_micros() as u64,
        }
    }

    fn on_ack(&mut self, now_ms: u64, delay_us: u64, bytes_acked: u64) {
        let minute = now_ms / 60_000;
        let slot = (minute as usize) % BASE_HISTORY_MINUTES;
        if self.last_minute != Some(minute) {
            self.base_delays[slot] = delay_us;
            self.last_minute = Some(minute);
        } else {
            self.base_delays[slot] = self.base_delays[slot].min(delay_us);
        }

        self.current_delays.pop_front();
        self.current_delays.push_back(delay_us);
        let filtered_delay = self.current_delays.back().copied().unwrap_or(delay_us);
        let base_delay = self.base_delays.iter().copied().min().unwrap_or(delay_us);
        let queue_delay = filtered_delay.saturating_sub(base_delay);
        let off_target = (self.target_us as f64 - queue_delay as f64) / self.target_us as f64;
        let increment = GAIN * off_target * bytes_acked as f64 * MSS as f64 / self.cwnd as f64;
        self.cwnd = (self.cwnd as f64 + increment).max((2 * MSS) as f64) as u64;
        let max_allowed = self.bytes_in_flight.saturating_add(u64::from(MSS));
        self.cwnd = self.cwnd.min(max_allowed).max(u64::from(2 * MSS));
        // Model an ACK-clocked sender that replenishes its flight to cwnd
        // immediately after each acknowledgement.
        self.bytes_in_flight = self.cwnd;
    }

    fn begin_round(&mut self) {
        self.bytes_in_flight = self.cwnd;
    }

    fn on_loss(&mut self) {
        self.cwnd = (self.cwnd / 2).max(u64::from(LEDBAT_MIN_CWND * MSS));
    }
}

fn run_trace(
    rounds: u64,
    cross_traffic: impl Fn(u64) -> u64,
    base_shift: impl Fn(u64) -> u64,
) -> (ResultSummary, ResultSummary) {
    let mut current = LedbatController::with_mss(MSS);
    let mut reference = RfcReference::new();
    let mut current_queue_bytes = 0u64;
    let mut reference_queue_bytes = 0u64;
    let mut current_cwnd_sum = 0u64;
    let mut reference_cwnd_sum = 0u64;
    let mut delivered_current = 0u64;
    let mut delivered_reference = 0u64;
    let mut current_max_queue_delay_us = 0u64;
    let mut reference_max_queue_delay_us = 0u64;
    let mut current_loss_rounds = 0u64;
    let mut reference_loss_rounds = 0u64;

    for round in 0..rounds {
        let cross_bytes = cross_traffic(round).min(CAPACITY_PER_RTT);
        let current_sent = u64::from(current.get_window_size());
        let reference_sent = reference.cwnd;
        let available_for_current = CAPACITY_PER_RTT.saturating_sub(cross_bytes);
        let current_offered = current_queue_bytes.saturating_add(current_sent);
        let reference_offered = reference_queue_bytes.saturating_add(reference_sent);
        let current_delivered = current_offered.min(available_for_current);
        let reference_delivered = reference_offered.min(available_for_current);
        let current_backlog = current_offered.saturating_sub(current_delivered);
        let reference_backlog = reference_offered.saturating_sub(reference_delivered);
        let current_dropped = current_backlog.saturating_sub(QUEUE_LIMIT_BYTES);
        let reference_dropped = reference_backlog.saturating_sub(QUEUE_LIMIT_BYTES);
        current_queue_bytes = current_backlog.min(QUEUE_LIMIT_BYTES);
        reference_queue_bytes = reference_backlog.min(QUEUE_LIMIT_BYTES);
        let current_queue_delay_us = ((current_queue_bytes as u128 * u128::from(RTT_US))
            / u128::from(CAPACITY_PER_RTT))
        .min(u128::from(u64::MAX)) as u64;
        let reference_queue_delay_us = ((reference_queue_bytes as u128 * u128::from(RTT_US))
            / u128::from(CAPACITY_PER_RTT))
        .min(u128::from(u64::MAX)) as u64;
        current_max_queue_delay_us = current_max_queue_delay_us.max(current_queue_delay_us);
        reference_max_queue_delay_us = reference_max_queue_delay_us.max(reference_queue_delay_us);
        let base_delay = BASE_DELAY_US.saturating_add(base_shift(round));
        let current_delay_sample = base_delay.saturating_add(current_queue_delay_us);
        let reference_delay_sample = base_delay.saturating_add(reference_queue_delay_us);
        let now_ms = round.saturating_mul(RTT_US / 1000);

        current.bytes_in_flight = current.get_window_size();
        reference.begin_round();
        let mut current_remaining = current_sent;
        while current_remaining > 0 {
            let acknowledged = current_remaining.min(u64::from(MSS)) as u32;
            current.on_ack_received(current_delay_sample, acknowledged);
            current.bytes_in_flight = current.get_window_size();
            current_remaining -= u64::from(acknowledged);
        }
        let mut reference_remaining = reference_sent;
        while reference_remaining > 0 {
            let acknowledged = reference_remaining.min(u64::from(MSS));
            reference.on_ack(now_ms, reference_delay_sample, acknowledged);
            reference_remaining -= acknowledged;
        }
        if current_dropped > 0 {
            current.on_loss();
            current_loss_rounds += 1;
        }
        if reference_dropped > 0 {
            reference.on_loss();
            reference_loss_rounds += 1;
        }

        delivered_current = delivered_current.saturating_add(current_delivered);
        delivered_reference = delivered_reference.saturating_add(reference_delivered);
        current_cwnd_sum = current_cwnd_sum.saturating_add(u64::from(current.get_window_size()));
        reference_cwnd_sum = reference_cwnd_sum.saturating_add(reference.cwnd);
    }

    let divisor = rounds.max(1);
    (
        ResultSummary {
            final_cwnd: current.get_window_size(),
            average_cwnd: current_cwnd_sum / divisor,
            average_delivered_per_rtt: delivered_current / divisor,
            max_queue_delay_us: current_max_queue_delay_us,
            loss_rounds: current_loss_rounds,
        },
        ResultSummary {
            final_cwnd: reference.cwnd.min(u64::from(u32::MAX)) as u32,
            average_cwnd: reference_cwnd_sum / divisor,
            average_delivered_per_rtt: delivered_reference / divisor,
            max_queue_delay_us: reference_max_queue_delay_us,
            loss_rounds: reference_loss_rounds,
        },
    )
}

#[test]
fn compare_current_controller_with_rfc_reference_under_deterministic_traces() {
    type Scenario = (&'static str, u64, fn(u64) -> u64, fn(u64) -> u64);
    let scenarios: [Scenario; 5] = [
        ("uncontended", 600, |_| 0, |_| 0),
        (
            "persistent-70-percent-cross-traffic",
            600,
            |_| CAPACITY_PER_RTT * 70 / 100,
            |_| 0,
        ),
        (
            "cross-traffic-burst",
            600,
            |round| {
                if (150..350).contains(&round) {
                    CAPACITY_PER_RTT * 70 / 100
                } else {
                    0
                }
            },
            |_| 0,
        ),
        (
            "capacity-halves-mid-run",
            600,
            |round| {
                if round >= 300 {
                    CAPACITY_PER_RTT / 2
                } else {
                    0
                }
            },
            |_| 0,
        ),
        (
            "path-base-delay-rises",
            1800,
            |_| 0,
            |round| if round >= 300 { 20_000 } else { 0 },
        ),
    ];

    let mut results = Vec::new();
    for (name, rounds, cross, base_shift) in scenarios {
        let (current, reference) = run_trace(rounds, cross, base_shift);
        println!("{name}: current={current:?}; rfc-reference={reference:?}");
        assert!(current.final_cwnd > 0);
        assert!(reference.final_cwnd > 0);
        assert!(current.average_cwnd > 0);
        assert!(reference.average_cwnd > 0);
        assert!(current.average_delivered_per_rtt <= CAPACITY_PER_RTT);
        assert!(reference.average_delivered_per_rtt <= CAPACITY_PER_RTT);
        assert!(current.max_queue_delay_us <= RTT_US * 2);
        assert!(reference.max_queue_delay_us <= RTT_US * 2);
        results.push((current, reference));
    }

    for (current, reference) in &results {
        assert_eq!(current.final_cwnd, reference.final_cwnd);
        assert_eq!(current.average_cwnd, reference.average_cwnd);
        assert_eq!(
            current.average_delivered_per_rtt,
            reference.average_delivered_per_rtt
        );
        assert_eq!(current.max_queue_delay_us, reference.max_queue_delay_us);
        assert_eq!(current.loss_rounds, reference.loss_rounds);
    }
}
