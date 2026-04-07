use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{sleep_until, Instant as TokioInstant};
use tracing::{debug, info};

use crate::error::Result;

pub type TimerId = u64;

#[derive(Clone, Debug)]
pub enum TimerEvent {
    OneShot(TimerId),
    Periodic(TimerId),
}

pub struct Timer {
    id: TimerId,
    next_fire: TokioInstant,
    interval: Option<Duration>,
}

impl Timer {
    fn new(id: TimerId, delay: Duration, interval: Option<Duration>) -> Self {
        let next_fire = TokioInstant::now() + delay;
        Timer { id, next_fire, interval }
    }
}

pub struct TimerA2 {
    timers: HashMap<TimerId, Timer>,
    event_tx: mpsc::UnboundedSender<TimerEvent>,
    next_id: TimerId,
}

impl TimerA2 {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<TimerEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        
        let timer_a2 = TimerA2 {
            timers: HashMap::new(),
            event_tx,
            next_id: 0,
        };

        info!("定时器系统初始化完成");
        (timer_a2, event_rx)
    }

    pub fn add_timer(&mut self, delay: Duration) -> Result<TimerId> {
        self.add_periodic_timer(delay, None)
    }

    pub fn add_periodic_timer(&mut self, delay: Duration, interval: Option<Duration>) -> Result<TimerId> {
        let id = self.next_id;
        self.next_id += 1;

        let timer = Timer::new(id, delay, interval);
        self.timers.insert(id, timer);

        debug!("添加定时器 #{} (延迟: {:?}, 间隔: {:?})", id, delay, interval);
        Ok(id)
    }

    pub fn remove_timer(&mut self, id: TimerId) {
        if self.timers.remove(&id).is_some() {
            debug!("移除定时器 #{}", id);
        }
    }

    pub async fn run(mut self) -> Result<()> {
        info!("定时器系统启动");

        loop {
            if self.timers.is_empty() {
                debug!("无活跃定时器,等待新定时器");
                tokio::task::yield_now().await;
                continue;
            }

            let now = TokioInstant::now();
            
            let mut fired_timers: Vec<(TimerId, bool)> = Vec::new();
            
            for (&id, timer) in &self.timers {
                if now >= timer.next_fire {
                    let is_periodic = timer.interval.is_some();
                    fired_timers.push((id, is_periodic));
                }
            }

            for (id, is_periodic) in fired_timers {
                if is_periodic {
                    let _ = self.event_tx.send(TimerEvent::Periodic(id));
                    
                    if let Some(timer) = self.timers.get_mut(&id) {
                        if let Some(interval) = timer.interval {
                            timer.next_fire = TokioInstant::now() + interval;
                        }
                    }
                } else {
                    let _ = self.event_tx.send(TimerEvent::OneShot(id));
                    self.timers.remove(&id);
                }
            }

            if !self.timers.is_empty() {
                let next_fire = self.timers
                    .values()
                    .map(|t| t.next_fire)
                    .min()
                    .expect("至少有一个定时器");

                sleep_until(next_fire).await;
            }
        }
    }
}
