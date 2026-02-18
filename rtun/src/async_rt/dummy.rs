use std::{
    sync::Arc,
    task::{self, Waker},
};

pub fn context<'a>(waker: &'a Waker) -> task::Context<'a> {
    task::Context::from_waker(waker)
}

pub fn waker() -> Waker {
    Arc::new(DummyWaker).into()
}

struct DummyWaker;
impl task::Wake for DummyWaker {
    fn wake(self: Arc<Self>) {}
}
