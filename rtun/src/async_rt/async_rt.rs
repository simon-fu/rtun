// use std::thread::Thread;
// use parking_lot::Mutex;

// #[derive(Debug, Clone)]
// pub struct RThread {
//     pub thread: Thread,
//     pub os_id: libc::pthread_t,
// }

// // pub fn threads() -> Vec<RThread> {
// //     RT_CTX.lock().threads.clone()
// // }

// #[derive(Debug, Default)]
// pub(super) struct RuntimeContext {
//     threads: Vec<RThread>,
// }

// lazy_static::lazy_static! {
//     static ref RT_CTX: Mutex<RuntimeContext> = Default::default();
// }

// fn current_tid() -> libc::pthread_t {
//     unsafe {libc::pthread_self()}
//     // thread_id::get()
// }

// pub(super) fn on_thread_start() {
//     // println!("on_thread_start: pthread id [{:?}], thread id [{:?}]", current_pid(), std::thread::current().id());

//     RT_CTX.lock().threads.push(RThread {
//         thread: std::thread::current(),
//         os_id: current_tid() ,
//     });
// }

// pub(super) fn on_thread_stop() {
//     let current = std::thread::current();
//     let mut ctx = RT_CTX.lock();
//     if let Some(index) = ctx.threads.iter().position(|t| t.thread.id() == current.id()) {
//         ctx.threads.swap_remove(index);
//     }
// }
