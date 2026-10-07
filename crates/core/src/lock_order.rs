//! 五层锁与叶子锁的 debug 顺序检查；release 只保留 parking_lot 的锁与守卫。
use std::ops::{Deref, DerefMut};

#[derive(Debug)]
struct Order {
    #[cfg(debug_assertions)]
    key: usize,
}
#[cfg(debug_assertions)]
thread_local! {
    static HELD: std::cell::RefCell<Vec<(usize,u8)>> = const { std::cell::RefCell::new(Vec::new()) };
}
impl Order {
    fn before<const R: u8>(key: usize) {
        #[cfg(debug_assertions)]
        HELD.with(|held| {
            let held = held.borrow();
            assert!(
                !held.iter().any(|(id, rank)| *id == key || *rank >= R),
                "lock order violation: acquiring rank {R} while holding {held:?}"
            );
        });
        #[cfg(not(debug_assertions))]
        let _ = key;
    }
    fn acquired<const R: u8>(key: usize) -> Self {
        #[cfg(debug_assertions)]
        HELD.with(|held| held.borrow_mut().push((key, R)));
        #[cfg(not(debug_assertions))]
        let _ = key;
        Self {
            #[cfg(debug_assertions)]
            key,
        }
    }
}
impl Drop for Order {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        HELD.with(|held| {
            let mut held = held.borrow_mut();
            // 守卫可提前释放；按身份移除，而不假设 Drop 严格后进先出。
            held.retain(|(key, _)| *key != self.key);
        });
    }
}

#[derive(Debug)]
pub struct Mutex<T, const R: u8>(parking_lot::Mutex<T>);
pub struct MutexGuard<'a, T> {
    guard: parking_lot::MutexGuard<'a, T>,
    _order: Order,
}
impl<T, const R: u8> Mutex<T, R> {
    pub(crate) fn assert_held(&self) {
        #[cfg(debug_assertions)]
        HELD.with(|held| {
            let key = self as *const Self as usize;
            assert!(
                held.borrow().iter().any(|(held_key, _)| *held_key == key),
                "required transaction lock is not held"
            );
        });
    }
    // 测试只观察其他线程是否持锁，不把短暂的非阻塞探测作为业务临界区。
    #[cfg(all(test, feature = "fault-injection"))]
    pub(crate) fn available(&self) -> bool {
        self.0.try_lock().is_some()
    }
    pub fn new(value: T) -> Self {
        Self(parking_lot::Mutex::new(value))
    }
    pub fn lock(&self) -> MutexGuard<'_, T> {
        let key = self as *const Self as usize;
        Order::before::<R>(key);
        MutexGuard {
            guard: self.0.lock(),
            _order: Order::acquired::<R>(key),
        }
    }
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        let key = self as *const Self as usize;
        self.0.try_lock().map(|guard| {
            Order::before::<R>(key);
            MutexGuard {
                guard,
                _order: Order::acquired::<R>(key),
            }
        })
    }
}
impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}
impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

#[derive(Debug)]
pub struct RwLock<T, const R: u8>(parking_lot::RwLock<T>);
pub struct ReadGuard<'a, T> {
    guard: parking_lot::RwLockReadGuard<'a, T>,
    _order: Order,
}
pub struct WriteGuard<'a, T> {
    guard: parking_lot::RwLockWriteGuard<'a, T>,
    _order: Order,
}
impl<T, const R: u8> RwLock<T, R> {
    #[cfg(all(test, feature = "fault-injection"))]
    pub(crate) fn available(&self) -> bool {
        self.0.try_write().is_some()
    }
    pub fn new(value: T) -> Self {
        Self(parking_lot::RwLock::new(value))
    }
    pub fn read(&self) -> ReadGuard<'_, T> {
        let key = self as *const Self as usize;
        Order::before::<R>(key);
        ReadGuard {
            guard: self.0.read(),
            _order: Order::acquired::<R>(key),
        }
    }
    pub fn write(&self) -> WriteGuard<'_, T> {
        let key = self as *const Self as usize;
        Order::before::<R>(key);
        WriteGuard {
            guard: self.0.write(),
            _order: Order::acquired::<R>(key),
        }
    }
    pub fn try_write(&self) -> Option<WriteGuard<'_, T>> {
        let key = self as *const Self as usize;
        self.0.try_write().map(|guard| {
            Order::before::<R>(key);
            WriteGuard {
                guard,
                _order: Order::acquired::<R>(key),
            }
        })
    }
}
impl<T> Deref for ReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}
impl<T> Deref for WriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}
impl<T> DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn held_check_requires_the_exact_mutex_and_clears_after_unwind() {
        let aliases = Mutex::<_, 6>::new(());
        let other = Mutex::<_, 6>::new(());
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| aliases.assert_held()))
                .is_err()
        );
        let held = other.lock();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| aliases.assert_held()))
                .is_err()
        );
        drop(held);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = aliases.lock();
            aliases.assert_held();
            panic!("transaction unwind");
        }));
        assert!(result.is_err());
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| aliases.assert_held()))
                .is_err()
        );
        let _guard = aliases.lock();
        aliases.assert_held();
    }
    #[test]
    fn rejects_reverse_order_reentry_and_leaf_to_parent_without_leaking_state() {
        let outer = RwLock::<_, 1>::new(false);
        let inner = Mutex::<_, 5>::new(());
        let leaf = Mutex::<_, 6>::new(());
        for reverse in [false, true] {
            let held = if reverse { inner.lock() } else { leaf.lock() };
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| outer.write())).is_err()
            );
            drop(held);
        }
        let read = outer.read();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| outer.write())).is_err());
        drop(read);
        let first = outer.write();
        let last = inner.lock();
        drop(first);
        drop(last);
        assert!(outer.read().deref() == &false);
    }
}
