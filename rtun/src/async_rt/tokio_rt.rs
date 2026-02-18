use std::{
    future::Future,
    sync::atomic::{AtomicUsize, Ordering},
};

use anyhow::Result;

// use super::{on_thread_start, on_thread_stop};

pub fn run_multi_thread<F: Future>(future: F) -> Result<F::Output> {
    run_with_builder(tokio::runtime::Builder::new_multi_thread(), future)
}

// pub fn run_single_thread<F: Future>(future: F) -> Result<F::Output> {
//     run_with_builder(
//         tokio::runtime::Builder::new_current_thread(),
//         future,
//     )
// }

fn run_with_builder<F: Future>(
    mut builder: tokio::runtime::Builder,
    future: F,
) -> Result<F::Output> {
    let rt = builder
        .enable_all()
        // .on_thread_start(on_thread_start)
        // .on_thread_stop(on_thread_stop)
        .thread_name_fn(|| {
            static ATOMIC_ID: AtomicUsize = AtomicUsize::new(1);
            let id = ATOMIC_ID.fetch_add(1, Ordering::SeqCst);
            format!("tokio{:02}", id)
        })
        .build()?;

    Ok(rt.block_on(future))
}

#[inline]
pub fn spawn_with_name<I, T>(name: I, fut: T) -> tokio::task::JoinHandle<T::Output>
where
    I: Into<String>,
    T: std::future::Future + Send + 'static,
    T::Output: Send + 'static,
{
    let name: String = name.into();
    let span = tracing::span!(parent:None, tracing::Level::INFO, "", s = &name[..]);
    tokio::spawn(tracing::Instrument::instrument(fut, span))
}

#[inline]
pub fn spawn_with_inherit<I, T>(name: I, fut: T) -> tokio::task::JoinHandle<T::Output>
where
    I: Into<String>,
    T: std::future::Future + Send + 'static,
    T::Output: Send + 'static,
{
    let name: String = name.into();
    let span = tracing::span!(tracing::Level::INFO, "", s = &name[..]);
    tokio::spawn(tracing::Instrument::instrument(fut, span))
}
