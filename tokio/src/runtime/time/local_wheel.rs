// tokio/src/runtime/time/local_wheel.rs
use crate::sync::mpsc;
use crate::runtime::time::{TimerHandle, wheel};
use crate::time::error::InsertError;
use std::collections::VecDeque;
use std::ptr::NonNull;
use crate::runtime::time::entry::TimerShared;
use crate::util::linked_list::Link; // 需要导入Link trait

pub(crate) struct LocalTimerWheel {
    /// 本地时间轮实现
    wheel: wheel::Wheel,
    
    /// 等待注册的定时器队列
    pending_registrations: VecDeque<TimerHandle>,
    
    /// 跨线程取消接收器
    cancel_receiver: mpsc::UnboundedReceiver<TimerHandle>,
}

impl LocalTimerWheel {
    pub(crate) fn new(cancel_receiver: mpsc::UnboundedReceiver<TimerHandle>) -> Self {
        Self {
            wheel: wheel::Wheel::new(),
            pending_registrations: VecDeque::new(),
            cancel_receiver,
        }
    }
    
    /// 注册定时器到本地轮
    pub(crate) fn register(&mut self, timer: TimerHandle) -> Result<(), TimerHandle> {
        match unsafe { self.wheel.insert(timer) } {
            Ok(_when) => Ok(()),
            Err((timer, InsertError::Elapsed)) => {
                // 定时器已经过期，应该立即触发
                Err(timer)
            }
        }
    }
    
    /// 添加定时器到等待注册队列（用于跨线程场景）
    pub(crate) fn queue_for_registration(&mut self, timer: TimerHandle) {
        self.pending_registrations.push_back(timer);
    }
    
    /// 处理到期的定时器和跨线程取消
    pub(crate) fn process_expired_and_cancelled(&mut self, now: u64) -> Vec<TimerHandle> {
        // 1. 处理跨线程取消的定时器
        while let Ok(timer_handle) = self.cancel_receiver.try_recv() {
            // 使用Link trait的as_raw方法获取NonNull<TimerShared>
            let timer_ptr = TimerShared::as_raw(&timer_handle);
            unsafe {
                self.wheel.remove(timer_ptr);
            }
        }
        
        // 2. 处理等待注册的定时器
        while let Some(timer) = self.pending_registrations.pop_front() {
            match self.register(timer) {
                Ok(()) => {
                    // 注册成功
                }
                Err(expired_timer) => {
                    // 定时器已过期，立即返回给调用者处理
                    return vec![expired_timer];
                }
            }
        }
        
        // 3. 收集到期的定时器
        let mut expired_timers = Vec::new();
        while let Some(timer) = self.wheel.poll(now) {
            expired_timers.push(timer);
        }
        
        expired_timers
    }
    
    /// 获取下次到期时间
    pub(crate) fn next_expiration(&self) -> Option<u64> {
        self.wheel.next_expiration_time()
    }
    
    /// 移除指定的定时器（用于本地取消）
    pub(crate) fn remove(&mut self, timer: NonNull<TimerShared>) {
        unsafe {
            self.wheel.remove(timer);
        }
    }
    
    /// 检查是否有待处理的定时器
    pub(crate) fn has_pending(&self) -> bool {
        !self.pending_registrations.is_empty() || self.next_expiration().is_some()
    }
}