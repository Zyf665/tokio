# Tokio 异步运行时源码深度解析

基于你在学习 Tokio 源码过程中的提问和理解，我来为你整理一份系统性的技术详解。

## 📚 目录

1. [Tokio 整体架构概览](#1-tokio-整体架构概览)
2. [任务生命周期管理](#2-任务生命周期管理)
3. [调度系统深入解析](#3-调度系统深入解析)
4. [超时机制与任务取消](#4-超时机制与任务取消)
5. [运行时上下文系统](#5-运行时上下文系统)
6. [层层抽象的设计模式](#6-层层抽象的设计模式)
7. [核心调用链路梳理](#7-核心调用链路梳理)

---

## 1. Tokio 整体架构概览

### 🏗️ 架构图

```
┌─────────────────────────────────────────────────────────────┐
│                    用户 API 层                                │
├─────────────────────────────────────────────────────────────┤
│  tokio::spawn()  │  tokio::timeout()  │  tokio::sleep()    │
└─────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────────────────────────────────────┐
│                   Runtime 层                                │
├─────────────────────────────────────────────────────────────┤
│  Handle::spawn() │  Context 管理  │  调度器接口             │
└─────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────────────────────────────────────┐
│                   调度器层                                   │
├─────────────────────────────────────────────────────────────┤
│  多线程调度器   │  本地队列    │  全局注入队列              │
│  工作窃取       │  LIFO 槽位   │  协作式调度                │
└─────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────────────────────────────────────┐
│                   任务管理层                                 │
├─────────────────────────────────────────────────────────────┤
│  Task<S>        │  Notified<S>  │  JoinHandle<T>           │
│  状态管理       │  引用计数     │  生命周期控制              │
└─────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────────────────────────────────────┐
│                   底层执行层                                 │
├─────────────────────────────────────────────────────────────┤
│  Future::poll() │  Waker 机制   │  协作式取消               │
│  状态转换       │  内存管理     │  资源清理                  │
└─────────────────────────────────────────────────────────────┘
```

### 🔑 核心组件

| 组件 | 职责 | 关键类型 |
|------|------|----------|
| **Runtime** | 运行时管理 | `Runtime`, `Handle` |
| **Scheduler** | 任务调度 | `Scheduler`, `Worker` |
| **Task** | 任务抽象 | `Task<S>`, `Notified<S>`, `JoinHandle<T>` |
| **Context** | 上下文管理 | `Context`, 线程本地存储 |
| **Future** | 异步抽象 | `Future`, `Poll`, `Waker` |

---

## 2. 任务生命周期管理

### 🔄 任务状态转换图

```
          ┌─────────────┐
          │   创建      │
          │ (Created)   │
          └──────┬──────┘
                 │ new_task()
                 ▼
          ┌─────────────┐
          │   绑定      │
          │ (Bound)     │
          └──────┬──────┘
                 │ bind()
                 ▼
          ┌─────────────┐
          │   调度      │
          │(Scheduled)  │
          └──────┬──────┘
                 │ schedule()
                 ▼
    ┌─────────────────────────┐
    │         通知             │
    │      (Notified)         │
    └────────┬────────────────┘
             │ poll()
             ▼
    ┌─────────────────────────┐
    │        运行             │
    │      (Running)          │
    └────┬──────────────┬─────┘
         │              │
    Poll::Pending   Poll::Ready
         │              │
         ▼              ▼
    ┌─────────────┐ ┌─────────────┐
    │    空闲     │ │    完成     │
    │   (Idle)    │ │(Complete)   │
    └─────────────┘ └─────────────┘
         │              │
         │ 可被取消        │ 资源清理
         ▼              ▼
    ┌─────────────┐ ┌─────────────┐
    │   取消      │ │   释放      │
    │(Cancelled)  │ │(Released)   │
    └─────────────┘ └─────────────┘
```

### 📊 任务状态位字段

```rust
// 任务状态原子 usize 的位字段布局
struct TaskState {
    // 位 0: 任务是否正在运行
    RUNNING: bool,
    
    // 位 1: Future 是否已完成
    COMPLETE: bool,
    
    // 位 2: 是否存在 Notified 对象
    NOTIFIED: bool,
    
    // 位 3: 任务是否被取消
    CANCELLED: bool,
    
    // 位 4: 是否存在 JoinHandle
    JOIN_INTEREST: bool,
    
    // 位 5: JoinHandle waker 访问控制
    JOIN_WAKER: bool,
    
    // 位 16-31: 引用计数
    REF_COUNT: u16,
}
```

### 🔧 核心任务类型

```rust
/// 基础任务类型 - 引用计数管理
pub(crate) struct Task<S: 'static> {
    raw: RawTask,
    _p: PhantomData<S>,
}

/// 已通知的任务 - 准备执行
pub(crate) struct Notified<S: 'static>(Task<S>);

/// 本地通知任务 - 非 Send
pub(crate) struct LocalNotified<S: 'static> {
    task: Task<S>,
    _not_send: PhantomData<*const ()>,
}

/// 用户句柄 - 获取结果
pub struct JoinHandle<T> {
    raw: RawTask,
    _p: PhantomData<T>,
}
```

---

## 3. 调度系统深入解析

### 🏭 多线程调度器架构

```
                    ┌─────────────────────────────────────┐
                    │         全局注入队列                │
                    │      (Global Inject Queue)         │
                    └─────────────┬───────────────────────┘
                                  │
                                  │ 工作窃取
                                  ▼
┌─────────────────┬─────────────────┬─────────────────┬─────────────────┐
│   Worker 0      │   Worker 1      │   Worker 2      │   Worker N      │
├─────────────────┼─────────────────┼─────────────────┼─────────────────┤
│ ┌─────────────┐ │ ┌─────────────┐ │ ┌─────────────┐ │ ┌─────────────┐ │
│ │ LIFO 槽位   │ │ │ LIFO 槽位   │ │ │ LIFO 槽位   │ │ │ LIFO 槽位   │ │
│ └─────────────┘ │ └─────────────┘ │ └─────────────┘ │ └─────────────┘ │
│ ┌─────────────┐ │ ┌─────────────┐ │ ┌─────────────┐ │ ┌─────────────┐ │
│ │  本地队列   │ │ │  本地队列   │ │ │  本地队列   │ │ │  本地队列   │ │
│ │(Local Queue)│ │ │(Local Queue)│ │ │(Local Queue)│ │ │(Local Queue)│ │
│ └─────────────┘ │ └─────────────┘ │ └─────────────┘ │ └─────────────┘ │
└─────────────────┴─────────────────┴─────────────────┴─────────────────┘
         ▲                   ▲                   ▲                   ▲
         │                   │                   │                   │
         └───────────────────┼───────────────────┼───────────────────┘
                             │                   │
                        工作窃取算法          工作窃取算法
```

### ⚡ 调度策略详解

#### 1. **任务放置策略**

```rust
// 根据当前上下文决定任务放置位置
fn schedule_task(task: Notified<Self>, is_yield: bool) {
    if let Some(core) = self.core.borrow_mut().as_mut() {
        // 在工作线程中
        if is_yield {
            // yield 操作：放入本地队列末尾
            core.run_queue.push_back(task);
        } else {
            // 新任务：优先放入 LIFO 槽位（提高局部性）
            if let Some(prev_task) = core.lifo_slot.take() {
                core.run_queue.push_back(prev_task);
            }
            core.lifo_slot = Some(task);
        }
    } else {
        // 不在工作线程中：放入全局注入队列
        self.inject.push(task);
        self.notify_parked_worker();
    }
}
```

#### 2. **工作窃取算法**

```rust
// Worker 查找任务的优先级顺序
fn next_task() -> Option<Notified<Self>> {
    // 1. 检查 LIFO 槽位（最高优先级，局部性最好）
    if let Some(task) = self.lifo_slot.take() {
        return Some(task);
    }
    
    // 2. 检查本地队列
    if let Some(task) = self.run_queue.pop_front() {
        return Some(task);
    }
    
    // 3. 从全局注入队列窃取
    if let Some(task) = self.inject.steal_batch(&mut self.run_queue) {
        return Some(task);
    }
    
    // 4. 从其他 Worker 窃取
    for other_worker in &self.workers {
        if let Some(task) = other_worker.steal() {
            return Some(task);
        }
    }
    
    None
}
```

### 🎯 协作式调度

```rust
// 协作式调度预算系统
pub(crate) struct Budget {
    remaining: u8,
}

impl Budget {
    const INITIAL: u8 = 128;
    
    pub(crate) fn has_remaining(&self) -> bool {
        self.remaining > 0
    }
    
    pub(crate) fn consume(&mut self) -> bool {
        if self.remaining > 0 {
            self.remaining -= 1;
            true
        } else {
            false
        }
    }
}

// 在任务轮询时检查预算
fn poll_with_budget<F: Future>(mut future: Pin<&mut F>, cx: &mut Context) -> Poll<F::Output> {
    if !coop::has_budget_remaining() {
        // 预算耗尽，让出执行权
        cx.waker().wake_by_ref();
        return Poll::Pending;
    }
    
    // 消耗一个预算单位
    coop::consume_budget();
    
    // 轮询 Future
    future.as_mut().poll(cx)
}
```

---

## 4. 超时机制与任务取消

### ⏰ 超时处理流程

```
┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐
│   用户调用      │    │   Timeout       │    │   延迟计时器    │
│ timeout(dur,f)  │───▶│   Future        │───▶│   (Delay)       │
└─────────────────┘    └─────────────────┘    └─────────────────┘
                                │
                                ▼
                       ┌─────────────────┐
                       │   轮询逻辑      │
                       └─────────────────┘
                                │
                    ┌───────────┴───────────┐
                    ▼                       ▼
            ┌─────────────────┐    ┌─────────────────┐
            │  先轮询内部     │    │  再检查超时     │
            │   Future        │    │   计时器        │
            └─────────────────┘    └─────────────────┘
                    │                       │
                    ▼                       ▼
            ┌─────────────────┐    ┌─────────────────┐
            │ Poll::Ready(v)  │    │ Poll::Ready(()) │
            │ → Ok(v)         │    │ → Err(Elapsed)  │
            └─────────────────┘    └─────────────────┘
```

### 🔍 超时实现源码解析

```rust
impl<T> Future for Timeout<T>
where
    T: Future,
{
    type Output = Result<T::Output, Elapsed>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.project();
        
        let had_budget_before = coop::has_budget_remaining();

        // 🎯 关键设计：先轮询内部 Future
        if let Poll::Ready(v) = me.value.poll(cx) {
            return Poll::Ready(Ok(v));  // 即使超时也返回成功
        }

        let has_budget_now = coop::has_budget_remaining();

        // 检查超时
        let poll_delay = || -> Poll<Self::Output> {
            match me.delay.poll(cx) {
                Poll::Ready(()) => Poll::Ready(Err(Elapsed::new())),
                Poll::Pending => Poll::Pending,
            }
        };

        // 处理协作式调度边界情况
        if let (true, false) = (had_budget_before, has_budget_now) {
            // 如果内部 Future 耗尽了预算，用无约束预算检查超时
            coop::with_unconstrained(poll_delay)
        } else {
            poll_delay()
        }
    }
}
```

**🤔 为什么先轮询 Future 再检查超时？**

1. **性能优化**：如果 Future 已经 Ready，避免不必要的时间检查
2. **语义一致性**：符合 Rust Future 的原子性要求
3. **避免竞态**：防止检查-轮询之间的时间窗口问题

### 🚫 任务取消机制

#### 1. **取消调用链**

```
JoinHandle::abort()
       │
       ▼
Task::abort()
       │
       ▼
RawTask::remote_abort()
       │
       ▼
State::transition_to_notified_and_cancel()
       │
       ▼
Task::schedule() (如果需要)
       │
       ▼
Worker 轮询时检查 CANCELLED 位
       │
       ▼
cancel_task() → 清理资源
```

#### 2. **状态转换详解**

```rust
pub(super) fn transition_to_notified_and_cancel(&self) -> bool {
    self.fetch_update_action(|mut snapshot| {
        if snapshot.is_cancelled() || snapshot.is_complete() {
            // 已取消或已完成的任务：无操作
            (false, None)
        } else if snapshot.is_running() {
            // 正在运行的任务：标记取消，运行线程会检查
            snapshot.set_notified();
            snapshot.set_cancelled();
            (false, Some(snapshot))
        } else {
            // 空闲任务：标记取消并通知调度
            snapshot.set_cancelled();
            if !snapshot.is_notified() {
                snapshot.set_notified();
                snapshot.ref_inc();  // 增加引用计数
                (true, Some(snapshot))  // 需要调度
            } else {
                (false, Some(snapshot))
            }
        }
    })
}
```

#### 3. **协作式取消**

```rust
// 在任务轮询过程中检查取消状态
fn poll_inner(&self) -> PollFuture {
    match self.state().transition_to_running() {
        TransitionToRunning::Success => {
            // 正常轮询 Future
            let res = poll_future(self.core(), cx);
            
            if res == Poll::Ready(()) {
                return PollFuture::Complete;
            }

            // 轮询完成后检查是否被取消
            let transition_res = self.state().transition_to_idle();
            if let TransitionToIdle::Cancelled = transition_res {
                cancel_task(self.core());  // 执行取消清理
            }
            
            transition_result_to_poll_future(transition_res)
        }
        TransitionToRunning::Cancelled => {
            // 启动时就发现被取消
            cancel_task(self.core());
            PollFuture::Complete
        }
        // ... 其他状态
    }
}
```

---

## 5. 运行时上下文系统

### 🗂️ 上下文结构图

```
┌─────────────────────────────────────────────────────────────┐
│                 CONTEXT (线程本地存储)                        │
├─────────────────────────────────────────────────────────────┤
│ ┌─────────────┐ ┌─────────────┐ ┌─────────────┐ ┌─────────┐ │
│ │  thread_id  │ │   current   │ │  scheduler  │ │   rng   │ │
│ │   线程ID    │ │  运行时句柄  │ │  调度器上下文│ │ 随机数  │ │
│ └─────────────┘ └─────────────┘ └─────────────┘ └─────────┘ │
│ ┌─────────────┐ ┌─────────────┐ ┌─────────────┐ ┌─────────┐ │
│ │current_task │ │   runtime   │ │   budget    │ │  trace  │ │
│ │  当前任务ID │ │  运行时状态  │ │  协作预算   │ │ 追踪信息│ │
│ └─────────────┘ └─────────────┘ └─────────────┘ └─────────┘ │
└─────────────────────────────────────────────────────────────┘
```

### 🔧 上下文访问模式

```rust
// 典型的上下文访问模式
pub fn spawn<T>(future: T) -> JoinHandle<T::Output>
where
    T: Future + Send + 'static,
    T::Output: Send + 'static,
{
    // 通过上下文获取当前运行时句柄
    match context::with_current(|handle| handle.spawn(future, id)) {
        Ok(join_handle) => join_handle,
        Err(e) => panic!("no current runtime: {}", e),
    }
}

// 上下文访问的底层实现
pub(crate) fn with_current<F, R>(f: F) -> Result<R, TryCurrentError>
where
    F: FnOnce(&scheduler::Handle) -> R,
{
    CONTEXT.try_with(|ctx| {
        // 获取当前运行时句柄的引用
        ctx.current.handle.borrow().as_ref().map(f)
    })
    .unwrap_or(Err(TryCurrentError::new_no_context()))
    .unwrap_or(Err(TryCurrentError::new_thread_local_destroyed()))
}
```

### 🎛️ 上下文生命周期管理

```rust
// 进入运行时时设置上下文
pub(crate) fn enter_runtime<F, R>(
    handle: &scheduler::Handle,
    allow_block_in_place: bool,
    f: F
) -> R
where
    F: FnOnce(&mut BlockingRegionGuard) -> R,
{
    CONTEXT.with(|c| {
        if c.runtime.get().is_entered() {
            panic!("Cannot start a runtime from within a runtime");
        }
        
        // 设置运行时状态
        c.runtime.set(EnterRuntime::Entered { allow_block_in_place });
        
        // 设置当前运行时句柄
        let handle_guard = c.set_current(handle);
        
        // 创建进入守护
        let guard = EnterRuntimeGuard {
            blocking: BlockingRegionGuard::new(),
            handle: handle_guard,
        };
        
        // 在上下文中执行用户代码
        f(&mut guard.blocking)
    })
}
```

---

## 6. 层层抽象的设计模式

### 🏗️ 抽象层次图

```
┌─────────────────────────────────────────────────────────────┐
│                      用户 API 层                             │
│  tokio::spawn(), tokio::timeout(), tokio::sleep()          │
└─────────────────┬───────────────────────────────────────────┘
                  │ 简化接口，隐藏复杂性
                  ▼
┌─────────────────────────────────────────────────────────────┐
│                     Runtime 层                              │
│  Handle::spawn(), Context 管理, 生命周期控制                │
└─────────────────┬───────────────────────────────────────────┘
                  │ 运行时抽象，资源管理
                  ▼
┌─────────────────────────────────────────────────────────────┐
│                    调度器抽象层                              │
│  Schedule trait, 多种调度器实现                             │
└─────────────────┬───────────────────────────────────────────┘
                  │ 调度策略抽象
                  ▼
┌─────────────────────────────────────────────────────────────┐
│                     任务抽象层                               │
│  Task<S>, Notified<S>, 状态管理                            │
└─────────────────┬───────────────────────────────────────────┘
                  │ 任务生命周期抽象
                  ▼
┌─────────────────────────────────────────────────────────────┐
│                    底层执行层                                │
│  RawTask, 内存管理, Future 轮询                            │
└─────────────────────────────────────────────────────────────┘
```

### 🎨 设计模式应用

#### 1. **策略模式 (Strategy Pattern)**

```rust
// 调度器策略抽象
pub(crate) trait Schedule: Sync + Sized + 'static {
    fn release(&self, task: &Task<Self>) -> Option<Task<Self>>;
    fn schedule(&self, task: Notified<Self>);
    fn yield_now(&self, task: Notified<Self>);
    fn unhandled_panic(&self);
}

// 多线程调度器实现
impl Schedule for MultiThread {
    fn schedule(&self, task: Notified<Self>) {
        self.shared.schedule_task(task, false);
    }
}

// 当前线程调度器实现
impl Schedule for CurrentThread {
    fn schedule(&self, task: Notified<Self>) {
        self.context.run_queue.push_back(task);
    }
}
```

#### 2. **适配器模式 (Adapter Pattern)**

```rust
// 将不同大小的 Future 适配为统一接口
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
{
    let fut_size = std::mem::size_of::<F>();
    if fut_size > BOX_FUTURE_THRESHOLD {
        // 大 Future：装箱到堆上
        spawn_inner(Box::pin(future), SpawnMeta::new_unnamed(fut_size))
    } else {
        // 小 Future：直接使用
        spawn_inner(future, SpawnMeta::new_unnamed(fut_size))
    }
}
```

#### 3. **代理模式 (Proxy Pattern)**

```rust
// JoinHandle 作为任务的代理
pub struct JoinHandle<T> {
    raw: RawTask,
    _p: PhantomData<T>,
}

impl<T> JoinHandle<T> {
    // 代理任务取消操作
    pub fn abort(&self) {
        self.raw.remote_abort();
    }
    
    // 代理获取任务结果
    pub async fn await(self) -> Result<T, JoinError> {
        // 实际等待任务完成的逻辑
    }
}
```

#### 4. **观察者模式 (Observer Pattern)**

```rust
// Waker 作为观察者，监听任务状态变化
impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.header().state.transition_to_notified_by_val();
        self.header().schedule();
    }
    
    fn wake_by_ref(self: &Arc<Self>) {
        self.header().state.transition_to_notified_by_ref();
        self.header().schedule();
    }
}
```

---

## 7. 核心调用链路梳理

### 🚀 任务生成完整调用链

```
tokio::spawn(future)
       │
       ▼
spawn_inner(future, meta)
       │
       ▼
context::with_current(|handle| handle.spawn(future, id))
       │
       ▼
Handle::spawn(future, id)
       │
       ▼
Spawner::spawn(future, id)
       │
       ▼
new_task(future, scheduler, id)
       │
       ├─ 创建 Task<S>
       ├─ 创建 Notified<S>  
       └─ 创建 JoinHandle<T>
       │
       ▼
OwnedTasks::bind(task, scheduler, id)
       │
       ├─ 将任务添加到任务列表
       └─ 返回 (JoinHandle, Option<Notified>)
       │
       ▼
task_hooks.spawn(TaskMeta { id })  // 生成钩子
       │
       ▼
schedule_option_task_without_yield(notified)  // 调度任务
       │
       ▼
schedule_task(notified, false)
       │
       ├─ 如果在工作线程 → 本地队列/LIFO槽位
       └─ 如果不在工作线程 → 全局注入队列
```

### ⚡ 任务执行调用链

```
Worker::run()
       │
       ▼
Worker::next_task()
       │
       ├─ 检查 LIFO 槽位
       ├─ 检查本地队列  
       ├─ 从全局队列窃取
       └─ 从其他 Worker 窃取
       │
       ▼
LocalNotified::run()
       │
       ▼
Harness::poll()
       │
       ▼
Harness::poll_inner()
       │
       ├─ state.transition_to_running()
       ├─ poll_future(core, cx)
       └─ state.transition_to_idle()
       │
       ▼
poll_future(core, cx)
       │
       ▼
Future::poll(cx)  // 用户的 Future
       │
       ├─ Poll::Ready(output) → complete_task()
       └─ Poll::Pending → 任务重新调度
```

### 🕐 超时处理调用链

```
tokio::time::timeout(duration, future)
       │
       ▼
Timeout::new(Delay::new(duration), future)
       │
       ▼
Timeout::poll(cx)
       │
       ├─ coop::has_budget_remaining()  // 检查预算
       ├─ inner_future.poll(cx)         // 先轮询内部 Future
       │  └─ Poll::Ready(v) → Ok(v)     // 即使超时也返回成功
       └─ delay.poll(cx)                // 再检查超时
          └─ Poll::Ready(()) → Err(Elapsed)
```

### 🚫 任务取消调用链

```
JoinHandle::abort()
       │
       ▼
Task::abort()
       │
       ▼
RawTask::remote_abort()
       │
       ▼
State::transition_to_notified_and_cancel()
       │
       ├─ 如果任务正在运行 → 设置 CANCELLED 位
       ├─ 如果任务空闲 → 设置 CANCELLED + NOTIFIED 位
       └─ 返回是否需要调度
       │
       ▼ (如果需要调度)
Harness::schedule()
       │
       ▼
Worker 下次轮询时检查 CANCELLED 位
       │
       ▼
cancel_task(core)
       │
       ├─ panic::catch_unwind(|| core.drop_future_or_output())
       └─ core.store_output(Err(JoinError::Cancelled))
```

---

## 🎯 关键设计思想总结

### 1. **零成本抽象 (Zero-Cost Abstractions)**
- 编译时优化，运行时无额外开销
- 泛型特化，避免动态分发
- 内联优化，消除函数调用开销

### 2. **内存安全 (Memory Safety)**
- 引用计数管理任务生命周期
- 原子操作确保并发安全
- RAII 模式自动资源清理

### 3. **协作式调度 (Cooperative Scheduling)**
- 任务主动让出 CPU
- 预算系统防止任务饿死
- 非抢占式，避免上下文切换开销

### 4. **工作窃取 (Work Stealing)**
- 负载均衡，提高 CPU 利用率
- 局部性优化，减少缓存未命中
- 动态调整，适应负载变化

### 5. **层次化设计 (Layered Architecture)**
- 职责分离，降低耦合度
- 接口抽象，提高可扩展性
- 组合模式，灵活配置

这份详解涵盖了你在学习 Tokio 源码过程中遇到的核心问题和概念。每个部分都结合了具体的代码示例和图表说明，希望能帮助你更好地理解 Tokio 的设计思想和实现原理。

```plaintext
┌─────────────────────────────────────────────────────────────┐
│                    用户 API 层                                │
├─────────────────────────────────────────────────────────────┤
│  tokio::spawn()  │  tokio::timeout()  │  tokio::sleep()    │
└─────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────────────────────────────────────┐
│                   Runtime 层                                │
├─────────────────────────────────────────────────────────────┤
│  Handle::spawn() │  Context 管理  │  调度器接口             │
└─────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────────────────────────────────────┐
│                   调度器层                                   │
├─────────────────────────────────────────────────────────────┤
│  多线程调度器   │  本地队列    │  全局注入队列              │
│  工作窃取       │  LIFO 槽位   │  协作式调度                │
└─────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────────────────────────────────────┐
│                   任务管理层                                 │
├─────────────────────────────────────────────────────────────┤
│  Task<S>        │  Notified<S>  │  JoinHandle<T>           │
│  状态管理       │  引用计数     │  生命周期控制              │
└─────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────────────────────────────────────┐
│                   底层执行层                                 │
├─────────────────────────────────────────────────────────────┤
│  Future::poll() │  Waker 机制   │  协作式取消               │
│  状态转换       │  内存管理     │  资源清理                  │
└─────────────────────────────────────────────────────────────┘
```

```plaintext
          ┌─────────────┐
          │   创建      │
          │ (Created)   │
          └──────┬──────┘
                 │ new_task()
                 ▼
          ┌─────────────┐
          │   绑定      │
          │ (Bound)     │
          └──────┬──────┘
                 │ bind()
                 ▼
          ┌─────────────┐
          │   调度      │
          │(Scheduled)  │
          └──────┬──────┘
                 │ schedule()
                 ▼
    ┌─────────────────────────┐
    │         通知             │
    │      (Notified)         │
    └────────┬────────────────┘
             │ poll()
             ▼
    ┌─────────────────────────┐
    │        运行             │
    │      (Running)          │
    └────┬──────────────┬─────┘
         │              │
    Poll::Pending   Poll::Ready
         │              │
         ▼              ▼
    ┌─────────────┐ ┌─────────────┐
    │    空闲     │ │    完成     │
    │   (Idle)    │ │(Complete)   │
    └─────────────┘ └─────────────┘
         │              │
         │ 可被取消        │ 资源清理
         ▼              ▼
    ┌─────────────┐ ┌─────────────┐
    │   取消      │ │   释放      │
    │(Cancelled)  │ │(Released)   │
    └─────────────┘ └─────────────┘
```

```rust
// 任务状态原子 usize 的位字段布局
struct TaskState {
    // 位 0: 任务是否正在运行
    RUNNING: bool,
    
    // 位 1: Future 是否已完成
    COMPLETE: bool,
    
    // 位 2: 是否存在 Notified 对象
    NOTIFIED: bool,
    
    // 位 3: 任务是否被取消
    CANCELLED: bool,
    
    // 位 4: 是否存在 JoinHandle
    JOIN_INTEREST: bool,
    
    // 位 5: JoinHandle waker 访问控制
    JOIN_WAKER: bool,
    
    // 位 16-31: 引用计数
    REF_COUNT: u16,
}
```

```rust
/// 基础任务类型 - 引用计数管理
pub(crate) struct Task<S: 'static> {
    raw: RawTask,
    _p: PhantomData<S>,
}

/// 已通知的任务 - 准备执行
pub(crate) struct Notified<S: 'static>(Task<S>);

/// 本地通知任务 - 非 Send
pub(crate) struct LocalNotified<S: 'static> {
    task: Task<S>,
    _not_send: PhantomData<*const ()>,
}

/// 用户句柄 - 获取结果
pub struct JoinHandle<T> {
    raw: RawTask,
    _p: PhantomData<T>,
}
```

```plaintext
                    ┌─────────────────────────────────────┐
                    │         全局注入队列                │
                    │      (Global Inject Queue)         │
                    └─────────────┬───────────────────────┘
                                  │
                                  │ 工作窃取
                                  ▼
┌─────────────────┬─────────────────┬─────────────────┬─────────────────┐
│   Worker 0      │   Worker 1      │   Worker 2      │   Worker N      │
├─────────────────┼─────────────────┼─────────────────┼─────────────────┤
│ ┌─────────────┐ │ ┌─────────────┐ │ ┌─────────────┐ │ ┌─────────────┐ │
│ │ LIFO 槽位   │ │ │ LIFO 槽位   │ │ │ LIFO 槽位   │ │ │ LIFO 槽位   │ │
│ └─────────────┘ │ └─────────────┘ │ └─────────────┘ │ └─────────────┘ │
│ ┌─────────────┐ │ ┌─────────────┐ │ ┌─────────────┐ │ ┌─────────────┐ │
│ │  本地队列   │ │ │  本地队列   │ │ │  本地队列   │ │ │  本地队列   │ │
│ │(Local Queue)│ │ │(Local Queue)│ │ │(Local Queue)│ │ │(Local Queue)│ │
│ └─────────────┘ │ └─────────────┘ │ └─────────────┘ │ └─────────────┘ │
└─────────────────┴─────────────────┴─────────────────┴─────────────────┘
         ▲                   ▲                   ▲                   ▲
         │                   │                   │                   │
         └───────────────────┼───────────────────┼───────────────────┘
                             │                   │
                        工作窃取算法          工作窃取算法
```

```rust
// 根据当前上下文决定任务放置位置
fn schedule_task(task: Notified<Self>, is_yield: bool) {
    if let Some(core) = self.core.borrow_mut().as_mut() {
        // 在工作线程中
        if is_yield {
            // yield 操作：放入本地队列末尾
            core.run_queue.push_back(task);
        } else {
            // 新任务：优先放入 LIFO 槽位（提高局部性）
            if let Some(prev_task) = core.lifo_slot.take() {
                core.run_queue.push_back(prev_task);
            }
            core.lifo_slot = Some(task);
        }
    } else {
        // 不在工作线程中：放入全局注入队列
        self.inject.push(task);
        self.notify_parked_worker();
    }
}
```

```rust
// Worker 查找任务的优先级顺序
fn next_task() -> Option<Notified<Self>> {
    // 1. 检查 LIFO 槽位（最高优先级，局部性最好）
    if let Some(task) = self.lifo_slot.take() {
        return Some(task);
    }
    
    // 2. 检查本地队列
    if let Some(task) = self.run_queue.pop_front() {
        return Some(task);
    }
    
    // 3. 从全局注入队列窃取
    if let Some(task) = self.inject.steal_batch(&mut self.run_queue) {
        return Some(task);
    }
    
    // 4. 从其他 Worker 窃取
    for other_worker in &self.workers {
        if let Some(task) = other_worker.steal() {
            return Some(task);
        }
    }
    
    None
}
```

```rust
// 协作式调度预算系统
pub(crate) struct Budget {
    remaining: u8,
}

impl Budget {
    const INITIAL: u8 = 128;
    
    pub(crate) fn has_remaining(&self) -> bool {
        self.remaining > 0
    }
    
    pub(crate) fn consume(&mut self) -> bool {
        if self.remaining > 0 {
            self.remaining -= 1;
            true
        } else {
            false
        }
    }
}

// 在任务轮询时检查预算
fn poll_with_budget<F: Future>(mut future: Pin<&mut F>, cx: &mut Context) -> Poll<F::Output> {
    if !coop::has_budget_remaining() {
        // 预算耗尽，让出执行权
        cx.waker().wake_by_ref();
        return Poll::Pending;
    }
    
    // 消耗一个预算单位
    coop::consume_budget();
    
    // 轮询 Future
    future.as_mut().poll(cx)
}
```

```plaintext
┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐
│   用户调用      │    │   Timeout       │    │   延迟计时器    │
│ timeout(dur,f)  │───▶│   Future        │───▶│   (Delay)       │
└─────────────────┘    └─────────────────┘    └─────────────────┘
                                │
                                ▼
                       ┌─────────────────┐
                       │   轮询逻辑      │
                       └─────────────────┘
                                │
                    ┌───────────┴───────────┐
                    ▼                       ▼
            ┌─────────────────┐    ┌─────────────────┐
            │  先轮询内部     │    │  再检查超时     │
            │   Future        │    │   计时器        │
            └─────────────────┘    └─────────────────┘
                    │                       │
                    ▼                       ▼
            ┌─────────────────┐    ┌─────────────────┐
            │ Poll::Ready(v)  │    │ Poll::Ready(()) │
            │ → Ok(v)         │    │ → Err(Elapsed)  │
            └─────────────────┘    └─────────────────┘
```

```rust
impl<T> Future for Timeout<T>
where
    T: Future,
{
    type Output = Result<T::Output, Elapsed>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.project();
        
        let had_budget_before = coop::has_budget_remaining();

        // 🎯 关键设计：先轮询内部 Future
        if let Poll::Ready(v) = me.value.poll(cx) {
            return Poll::Ready(Ok(v));  // 即使超时也返回成功
        }

        let has_budget_now = coop::has_budget_remaining();

        // 检查超时
        let poll_delay = || -> Poll<Self::Output> {
            match me.delay.poll(cx) {
                Poll::Ready(()) => Poll::Ready(Err(Elapsed::new())),
                Poll::Pending => Poll::Pending,
            }
        };

        // 处理协作式调度边界情况
        if let (true, false) = (had_budget_before, has_budget_now) {
            // 如果内部 Future 耗尽了预算，用无约束预算检查超时
            coop::with_unconstrained(poll_delay)
        } else {
            poll_delay()
        }
    }
}
```

```plaintext
JoinHandle::abort()
       │
       ▼
Task::abort()
       │
       ▼
RawTask::remote_abort()
       │
       ▼
State::transition_to_notified_and_cancel()
       │
       ▼
Task::schedule() (如果需要)
       │
       ▼
Worker 轮询时检查 CANCELLED 位
       │
       ▼
cancel_task() → 清理资源
```

```rust
pub(super) fn transition_to_notified_and_cancel(&self) -> bool {
    self.fetch_update_action(|mut snapshot| {
        if snapshot.is_cancelled() || snapshot.is_complete() {
            // 已取消或已完成的任务：无操作
            (false, None)
        } else if snapshot.is_running() {
            // 正在运行的任务：标记取消，运行线程会检查
            snapshot.set_notified();
            snapshot.set_cancelled();
            (false, Some(snapshot))
        } else {
            // 空闲任务：标记取消并通知调度
            snapshot.set_cancelled();
            if !snapshot.is_notified() {
                snapshot.set_notified();
                snapshot.ref_inc();  // 增加引用计数
                (true, Some(snapshot))  // 需要调度
            } else {
                (false, Some(snapshot))
            }
        }
    })
}
```

```rust
// 在任务轮询过程中检查取消状态
fn poll_inner(&self) -> PollFuture {
    match self.state().transition_to_running() {
        TransitionToRunning::Success => {
            // 正常轮询 Future
            let res = poll_future(self.core(), cx);
            
            if res == Poll::Ready(()) {
                return PollFuture::Complete;
            }

            // 轮询完成后检查是否被取消
            let transition_res = self.state().transition_to_idle();
            if let TransitionToIdle::Cancelled = transition_res {
                cancel_task(self.core());  // 执行取消清理
            }
            
            transition_result_to_poll_future(transition_res)
        }
        TransitionToRunning::Cancelled => {
            // 启动时就发现被取消
            cancel_task(self.core());
            PollFuture::Complete
        }
        // ... 其他状态
    }
}
```

```plaintext
┌─────────────────────────────────────────────────────────────┐
│                 CONTEXT (线程本地存储)                        │
├─────────────────────────────────────────────────────────────┤
│ ┌─────────────┐ ┌─────────────┐ ┌─────────────┐ ┌─────────┐ │
│ │  thread_id  │ │   current   │ │  scheduler  │ │   rng   │ │
│ │   线程ID    │ │  运行时句柄  │ │  调度器上下文│ │ 随机数  │ │
│ └─────────────┘ └─────────────┘ └─────────────┘ └─────────┘ │
│ ┌─────────────┐ ┌─────────────┐ ┌─────────────┐ ┌─────────┐ │
│ │current_task │ │   runtime   │ │   budget    │ │  trace  │ │
│ │  当前任务ID │ │  运行时状态  │ │  协作预算   │ │ 追踪信息│ │
│ └─────────────┘ └─────────────┘ └─────────────┘ └─────────┘ │
└─────────────────────────────────────────────────────────────┘
```

```rust
// 典型的上下文访问模式
pub fn spawn<T>(future: T) -> JoinHandle<T::Output>
where
    T: Future + Send + 'static,
    T::Output: Send + 'static,
{
    // 通过上下文获取当前运行时句柄
    match context::with_current(|handle| handle.spawn(future, id)) {
        Ok(join_handle) => join_handle,
        Err(e) => panic!("no current runtime: {}", e),
    }
}

// 上下文访问的底层实现
pub(crate) fn with_current<F, R>(f: F) -> Result<R, TryCurrentError>
where
    F: FnOnce(&scheduler::Handle) -> R,
{
    CONTEXT.try_with(|ctx| {
        // 获取当前运行时句柄的引用
        ctx.current.handle.borrow().as_ref().map(f)
    })
    .unwrap_or(Err(TryCurrentError::new_no_context()))
    .unwrap_or(Err(TryCurrentError::new_thread_local_destroyed()))
}
```

```rust
// 进入运行时时设置上下文
pub(crate) fn enter_runtime<F, R>(
    handle: &scheduler::Handle,
    allow_block_in_place: bool,
    f: F
) -> R
where
    F: FnOnce(&mut BlockingRegionGuard) -> R,
{
    CONTEXT.with(|c| {
        if c.runtime.get().is_entered() {
            panic!("Cannot start a runtime from within a runtime");
        }
        
        // 设置运行时状态
        c.runtime.set(EnterRuntime::Entered { allow_block_in_place });
        
        // 设置当前运行时句柄
        let handle_guard = c.set_current(handle);
        
        // 创建进入守护
        let guard = EnterRuntimeGuard {
            blocking: BlockingRegionGuard::new(),
            handle: handle_guard,
        };
        
        // 在上下文中执行用户代码
        f(&mut guard.blocking)
    })
}
```

```plaintext
┌─────────────────────────────────────────────────────────────┐
│                      用户 API 层                             │
│  tokio::spawn(), tokio::timeout(), tokio::sleep()          │
└─────────────────┬───────────────────────────────────────────┘
                  │ 简化接口，隐藏复杂性
                  ▼
┌─────────────────────────────────────────────────────────────┐
│                     Runtime 层                              │
│  Handle::spawn(), Context 管理, 生命周期控制                │
└─────────────────┬───────────────────────────────────────────┘
                  │ 运行时抽象，资源管理
                  ▼
┌─────────────────────────────────────────────────────────────┐
│                    调度器抽象层                              │
│  Schedule trait, 多种调度器实现                             │
└─────────────────┬───────────────────────────────────────────┘
                  │ 调度策略抽象
                  ▼
┌─────────────────────────────────────────────────────────────┐
│                     任务抽象层                               │
│  Task<S>, Notified<S>, 状态管理                            │
└─────────────────┬───────────────────────────────────────────┘
                  │ 任务生命周期抽象
                  ▼
┌─────────────────────────────────────────────────────────────┐
│                    底层执行层                                │
│  RawTask, 内存管理, Future 轮询                            │
└─────────────────────────────────────────────────────────────┘
```

```rust
// 调度器策略抽象
pub(crate) trait Schedule: Sync + Sized + 'static {
    fn release(&self, task: &Task<Self>) -> Option<Task<Self>>;
    fn schedule(&self, task: Notified<Self>);
    fn yield_now(&self, task: Notified<Self>);
    fn unhandled_panic(&self);
}

// 多线程调度器实现
impl Schedule for MultiThread {
    fn schedule(&self, task: Notified<Self>) {
        self.shared.schedule_task(task, false);
    }
}

// 当前线程调度器实现
impl Schedule for CurrentThread {
    fn schedule(&self, task: Notified<Self>) {
        self.context.run_queue.push_back(task);
    }
}
```

```rust
// 将不同大小的 Future 适配为统一接口
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
{
    let fut_size = std::mem::size_of::<F>();
    if fut_size > BOX_FUTURE_THRESHOLD {
        // 大 Future：装箱到堆上
        spawn_inner(Box::pin(future), SpawnMeta::new_unnamed(fut_size))
    } else {
        // 小 Future：直接使用
        spawn_inner(future, SpawnMeta::new_unnamed(fut_size))
    }
}
```

```rust
// JoinHandle 作为任务的代理
pub struct JoinHandle<T> {
    raw: RawTask,
    _p: PhantomData<T>,
}

impl<T> JoinHandle<T> {
    // 代理任务取消操作
    pub fn abort(&self) {
        self.raw.remote_abort();
    }
    
    // 代理获取任务结果
    pub async fn await(self) -> Result<T, JoinError> {
        // 实际等待任务完成的逻辑
    }
}
```

```rust
// Waker 作为观察者，监听任务状态变化
impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.header().state.transition_to_notified_by_val();
        self.header().schedule();
    }
    
    fn wake_by_ref(self: &Arc<Self>) {
        self.header().state.transition_to_notified_by_ref();
        self.header().schedule();
    }
}
```

```plaintext
tokio::spawn(future)
       │
       ▼
spawn_inner(future, meta)
       │
       ▼
context::with_current(|handle| handle.spawn(future, id))
       │
       ▼
Handle::spawn(future, id)
       │
       ▼
Spawner::spawn(future, id)
       │
       ▼
new_task(future, scheduler, id)
       │
       ├─ 创建 Task<S>
       ├─ 创建 Notified<S>  
       └─ 创建 JoinHandle<T>
       │
       ▼
OwnedTasks::bind(task, scheduler, id)
       │
       ├─ 将任务添加到任务列表
       └─ 返回 (JoinHandle, Option<Notified>)
       │
       ▼
task_hooks.spawn(TaskMeta { id })  // 生成钩子
       │
       ▼
schedule_option_task_without_yield(notified)  // 调度任务
       │
       ▼
schedule_task(notified, false)
       │
       ├─ 如果在工作线程 → 本地队列/LIFO槽位
       └─ 如果不在工作线程 → 全局注入队列
```

```plaintext
Worker::run()
       │
       ▼
Worker::next_task()
       │
       ├─ 检查 LIFO 槽位
       ├─ 检查本地队列  
       ├─ 从全局队列窃取
       └─ 从其他 Worker 窃取
       │
       ▼
LocalNotified::run()
       │
       ▼
Harness::poll()
       │
       ▼
Harness::poll_inner()
       │
       ├─ state.transition_to_running()
       ├─ poll_future(core, cx)
       └─ state.transition_to_idle()
       │
       ▼
poll_future(core, cx)
       │
       ▼
Future::poll(cx)  // 用户的 Future
       │
       ├─ Poll::Ready(output) → complete_task()
       └─ Poll::Pending → 任务重新调度
```

```plaintext
tokio::time::timeout(duration, future)
       │
       ▼
Timeout::new(Delay::new(duration), future)
       │
       ▼
Timeout::poll(cx)
       │
       ├─ coop::has_budget_remaining()  // 检查预算
       ├─ inner_future.poll(cx)         // 先轮询内部 Future
       │  └─ Poll::Ready(v) → Ok(v)     // 即使超时也返回成功
       └─ delay.poll(cx)                // 再检查超时
          └─ Poll::Ready(()) → Err(Elapsed)
```

```plaintext
JoinHandle::abort()
       │
       ▼
Task::abort()
       │
       ▼
RawTask::remote_abort()
       │
       ▼
State::transition_to_notified_and_cancel()
       │
       ├─ 如果任务正在运行 → 设置 CANCELLED 位
       ├─ 如果任务空闲 → 设置 CANCELLED + NOTIFIED 位
       └─ 返回是否需要调度
       │
       ▼ (如果需要调度)
Harness::schedule()
       │
       ▼
Worker 下次轮询时检查 CANCELLED 位
       │
       ▼
cancel_task(core)
       │
       ├─ panic::catch_unwind(|| core.drop_future_or_output())
       └─ core.store_output(Err(JoinError::Cancelled))
```

