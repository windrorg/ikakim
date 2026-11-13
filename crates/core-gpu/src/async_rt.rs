//! ゼロ依存 Future ブロッキングランタイム（wgpu adapter / device 初期化用）

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::thread::{self, Thread};

fn thread_waker(thread: Thread) -> Waker {
    let ptr = Box::into_raw(Box::new(thread)) as *const ();
    unsafe { Waker::from_raw(RawWaker::new(ptr, &VTABLE)) }
}

unsafe fn clone_waker(ptr: *const ()) -> RawWaker {
    let thread = &*(ptr as *const Thread);
    let cloned = Box::into_raw(Box::new(thread.clone())) as *const ();
    RawWaker::new(cloned, &VTABLE)
}

unsafe fn wake_waker(ptr: *const ()) {
    let thread = Box::from_raw(ptr as *mut Thread);
    thread.unpark();
}

unsafe fn wake_by_ref_waker(ptr: *const ()) {
    let thread = &*(ptr as *const Thread);
    thread.unpark();
}

unsafe fn drop_waker(ptr: *const ()) {
    let _ = Box::from_raw(ptr as *mut Thread);
}

static VTABLE: RawWakerVTable = RawWakerVTable::new(
    clone_waker,
    wake_waker,
    wake_by_ref_waker,
    drop_waker,
);

/// 外部ランタイム（tokio/pollster等）なしで Future を同期完了させる
pub fn block_on<F: Future>(fut: F) -> F::Output {
    let mut pinned = pin!(fut);
    let waker = thread_waker(thread::current());
    let mut cx = Context::from_waker(&waker);
    loop {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(val) => return val,
            Poll::Pending => thread::park(),
        }
    }
}
