use crate::sync::mpsc;

use crate::runtime::time::TimerHandle;
pub(crate) struct CrossThreadTimerSystem{
  cancel_senders: Vec<mpsc::UnboundedSender<TimerHandle>>,
}

impl CrossThreadTimerSystem {
    pub(crate) fn new(num_workers:usize) -> (Self,Vec<mpsc::UnboundedReceiver<TimerHandle>>){
        let mut senders = Vec::with_capacity(num_workers);
        let mut receivers = Vec::with_capacity(num_workers);

        for _ in 0..num_workers{
           let (tx,rx) = mpsc::unbounded_channel();
           senders.push(tx);
           receivers.push(rx);
        }

        (Self { cancel_senders:senders }, receivers)
    }


    pub(crate) fn cancel_timer(&self, timer:TimerHandle, target_worker: usize) -> Result<(),TimerHandle>{
        if let Some(sender) = self.cancel_senders.get(target_worker) {
            sender.send(timer).map_err(|e| e.0)
        } else {
            Err(timer)
        }
    }
}