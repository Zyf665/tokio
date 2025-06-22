use crate::runtime::{scheduler};

use super::local_wheel::LocalTimerWheel;
pub(crate) trait LocalTimerWheelProvider {
    fn local_timer_wheel(&mut self) -> &mut LocalTimerWheel;
    fn process_local_timers(&mut self,handle:&scheduler::Handle);
    fn next_local_timer_expiration(&self) -> Option<u64>;
}
