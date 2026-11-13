use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// キャッシュラインの偽共有（False Sharing）を防ぐための 128 バイトアラインメントラッパー
#[repr(align(128))]
struct CachePadded<T>(T);

/// 単一生産者・単一消費者（SPSC）ロックフリーリングバッファの内部ストレージ
struct RingBufferInner<T> {
    buffer: Box<[UnsafeCell<MaybeUninit<T>>]>,
    capacity: usize,
    mask: usize,
    head: CachePadded<AtomicUsize>, // Consumer が更新（読み出し位置）
    tail: CachePadded<AtomicUsize>, // Producer が更新（書き込み位置）
}

// 要素 T が Send であれば、Producer と Consumer を別スレッドに安全に送れる
unsafe impl<T: Send> Send for RingBufferInner<T> {}
unsafe impl<T: Send> Sync for RingBufferInner<T> {}

impl<T> Drop for RingBufferInner<T> {
    fn drop(&mut self) {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Relaxed);
        let mut idx = head;
        while idx != tail {
            let slot = idx & self.mask;
            unsafe {
                (*self.buffer[slot].get()).assume_init_drop();
            }
            idx = idx.wrapping_add(1);
        }
    }
}

/// 書き込み側ハンドル（Producer スレッド専用）
pub struct SpscProducer<T> {
    inner: Arc<RingBufferInner<T>>,
}

/// 読み出し側ハンドル（Consumer スレッド専用）
pub struct SpscConsumer<T> {
    inner: Arc<RingBufferInner<T>>,
}

/// 指定された最小容量以上の 2 のべき乗サイズで SPSC リングバッファを生成する
pub fn spsc_channel<T>(min_capacity: usize) -> (SpscProducer<T>, SpscConsumer<T>) {
    assert!(min_capacity > 0, "capacity must be greater than 0");
    let capacity = min_capacity.next_power_of_two();
    let mask = capacity - 1;
    let mut vec = Vec::with_capacity(capacity);
    for _ in 0..capacity {
        vec.push(UnsafeCell::new(MaybeUninit::uninit()));
    }

    let inner = Arc::new(RingBufferInner {
        buffer: vec.into_boxed_slice(),
        capacity,
        mask,
        head: CachePadded(AtomicUsize::new(0)),
        tail: CachePadded(AtomicUsize::new(0)),
    });

    (
        SpscProducer {
            inner: Arc::clone(&inner),
        },
        SpscConsumer { inner },
    )
}

impl<T> SpscProducer<T> {
    #[inline]
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    /// 現在の空きスロット数を返す
    #[inline]
    pub fn available_write(&self) -> usize {
        let tail = self.inner.tail.0.load(Ordering::Relaxed);
        let head = self.inner.head.0.load(Ordering::Acquire);
        self.inner.capacity - tail.wrapping_sub(head)
    }

    /// 1要素をノンブロッキングで書き込む。満杯の場合は `Err(item)` を返す
    #[inline]
    pub fn try_push(&mut self, item: T) -> Result<(), T> {
        let tail = self.inner.tail.0.load(Ordering::Relaxed);
        let head = self.inner.head.0.load(Ordering::Acquire);
        if tail.wrapping_sub(head) == self.inner.capacity {
            return Err(item);
        }
        let slot = tail & self.inner.mask;
        unsafe {
            (*self.inner.buffer[slot].get()).write(item);
        }
        self.inner
            .tail
            .0
            .store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }
}

impl<T: Copy> SpscProducer<T> {
    /// 音声PCMサンプル等のスライスを一括書き込みし、書き込めた要素数を返す
    pub fn push_slice(&mut self, items: &[T]) -> usize {
        let tail = self.inner.tail.0.load(Ordering::Relaxed);
        let head = self.inner.head.0.load(Ordering::Acquire);
        let free = self.inner.capacity - tail.wrapping_sub(head);
        let count = items.len().min(free);
        if count == 0 {
            return 0;
        }

        let start_slot = tail & self.inner.mask;
        let first_part = (self.inner.capacity - start_slot).min(count);
        unsafe {
            let dst = self.inner.buffer[start_slot].get() as *mut T;
            ptr::copy_nonoverlapping(items.as_ptr(), dst, first_part);
            if first_part < count {
                let second_part = count - first_part;
                let dst_wrap = self.inner.buffer[0].get() as *mut T;
                ptr::copy_nonoverlapping(items.as_ptr().add(first_part), dst_wrap, second_part);
            }
        }
        self.inner
            .tail
            .0
            .store(tail.wrapping_add(count), Ordering::Release);
        count
    }
}

impl<T> SpscConsumer<T> {
    /// 現在読み出し可能な要素数を返す
    #[inline]
    pub fn available_read(&self) -> usize {
        let head = self.inner.head.0.load(Ordering::Relaxed);
        let tail = self.inner.tail.0.load(Ordering::Acquire);
        tail.wrapping_sub(head)
    }

    /// 1要素をノンブロッキングで取り出す
    #[inline]
    pub fn try_pop(&mut self) -> Option<T> {
        let head = self.inner.head.0.load(Ordering::Relaxed);
        let tail = self.inner.tail.0.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        let slot = head & self.inner.mask;
        let item = unsafe { (*self.inner.buffer[slot].get()).assume_init_read() };
        self.inner
            .head
            .0
            .store(head.wrapping_add(1), Ordering::Release);
        Some(item)
    }

    /// 溜まっている古い要素を破棄して最新状態へ追いつく（遅延リカバリ用）
    pub fn drain_all(&mut self) -> usize {
        let mut dropped = 0;
        while self.try_pop().is_some() {
            dropped += 1;
        }
        dropped
    }
}

impl<T: Copy> SpscConsumer<T> {
    /// 音声PCMサンプル等をバッファへ一括読み出しし、読み出せた要素数を返す
    pub fn pop_slice(&mut self, out: &mut [T]) -> usize {
        let head = self.inner.head.0.load(Ordering::Relaxed);
        let tail = self.inner.tail.0.load(Ordering::Acquire);
        let avail = tail.wrapping_sub(head);
        let count = out.len().min(avail);
        if count == 0 {
            return 0;
        }

        let start_slot = head & self.inner.mask;
        let first_part = (self.inner.capacity - start_slot).min(count);
        unsafe {
            let src = self.inner.buffer[start_slot].get() as *const T;
            ptr::copy_nonoverlapping(src, out.as_mut_ptr(), first_part);
            if first_part < count {
                let second_part = count - first_part;
                let src_wrap = self.inner.buffer[0].get() as *const T;
                ptr::copy_nonoverlapping(src_wrap, out.as_mut_ptr().add(first_part), second_part);
            }
        }
        self.inner
            .head
            .0
            .store(head.wrapping_add(count), Ordering::Release);
        count
    }
}
