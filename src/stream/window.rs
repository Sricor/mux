use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};

const DEFAULT_WINDOW_SIZE: usize = 65_535;
const MAX_WINDOW_SIZE: usize = 1_073_741_823; // 2^30 - 1

pub(super) struct WindowControl {
    pub(super) send_window: AtomicUsize, // 发送窗口的大小
    pub(super) recv_window: AtomicUsize, // 接收窗口的大小
    pub(super) max_window_size: usize,   // 最大允许的窗口大小
}

impl WindowControl {
    pub(super) fn new() -> Self {
        Self {
            send_window: AtomicUsize::new(DEFAULT_WINDOW_SIZE),
            recv_window: AtomicUsize::new(DEFAULT_WINDOW_SIZE),
            max_window_size: MAX_WINDOW_SIZE,
        }
    }

    pub(super) fn update_send_window(&self, increment: usize) -> io::Result<()> {
        let current = self.send_window.load(Ordering::Relaxed);
        let new_size = current
            .checked_add(increment)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "window size overflow"))?;

        if new_size > self.max_window_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "window size exceeds maximum allowed",
            ));
        }

        self.send_window.store(new_size, Ordering::Relaxed);
        Ok(())
    }

    pub(super) fn consume_send_window(&self, size: usize) -> io::Result<()> {
        let current = self.send_window.load(Ordering::Relaxed);
        if size > current {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "insufficient window size",
            ));
        }

        self.send_window.fetch_sub(size, Ordering::Relaxed);
        Ok(())
    }

    pub(super) fn should_update_recv_window(&self, consumed: usize) -> Option<usize> {
        let current = self.recv_window.fetch_sub(consumed, Ordering::Relaxed);
        if current - consumed <= DEFAULT_WINDOW_SIZE / 2 {
            Some(DEFAULT_WINDOW_SIZE - (current - consumed))
        } else {
            None
        }
    }
}

// 添加测试
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_window_overflow() {
        let control = WindowControl::new();
        let max_increment = usize::MAX - DEFAULT_WINDOW_SIZE + 1;

        // 测试溢出情况
        assert!(control.update_send_window(max_increment).is_err());
    }
}
