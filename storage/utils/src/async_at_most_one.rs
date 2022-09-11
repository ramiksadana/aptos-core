// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;
use aptos_logger::info;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::thread::JoinHandle;

pub struct AsyncAtMostOne {
    thread: Option<JoinHandle<()>>,
    task_sender: Option<SyncSender<Box<dyn FnOnce() -> Result<()> + Send + Sync + 'static>>>,
    result_receiver: Receiver<Result<()>>,
    has_pending: bool,
}

impl AsyncAtMostOne {
    pub fn new(name: String) -> Self {
        let (task_sender, task_receiver) =
            sync_channel::<Box<dyn FnOnce() -> Result<()> + Send + Sync + 'static>>(1);
        let (result_sender, result_receiver) = sync_channel(1);

        let thread = std::thread::Builder::new()
            .name(name.clone())
            .spawn(move || {
                info!(thread_name = &name, "Task running thread started.");
                while let Ok(task) = task_receiver.recv() {
                    result_sender.try_send(task()).unwrap();
                }
                info!(thread_name = &name, "Task running thread quitting.")
            })
            .expect("Failed to spawn async worker thread.");

        Self {
            thread: Some(thread),
            task_sender: Some(task_sender),
            result_receiver,
            has_pending: false,
        }
    }

    pub fn async_run(
        &mut self,
        op: impl FnOnce() -> Result<()> + Send + Sync + 'static,
    ) -> Result<()> {
        self.sync()?;

        self.has_pending = true;
        self.task_sender.as_ref().unwrap().try_send(Box::new(op))?;
        Ok(())
    }

    pub fn sync(&mut self) -> Result<()> {
        if self.has_pending {
            self.has_pending = false;
            self.result_receiver.recv()??;
        }
        Ok(())
    }
}

impl Drop for AsyncAtMostOne {
    fn drop(&mut self) {
        self.sync().unwrap();
        drop(self.task_sender.take().unwrap());
        self.thread.take().unwrap().join().unwrap();
    }
}

#[cfg(test)]
mod tests {
    use crate::async_at_most_one::AsyncAtMostOne;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_async_run() {
        let mut async_at_most_one = AsyncAtMostOne::new("pass through".to_owned());
        let shared_state = Arc::new(AtomicU8::new(0));
        let state = shared_state.clone();
        async_at_most_one
            .async_run(move || {
                state.store(1, Ordering::Relaxed);
                Ok(())
            })
            .unwrap();
        let state = shared_state.clone();
        async_at_most_one
            .async_run(move || {
                state.store(2, Ordering::Relaxed);
                Ok(())
            })
            .unwrap();
        let state = shared_state.clone();
        async_at_most_one
            .async_run(move || {
                state.store(3, Ordering::Relaxed);
                Ok(())
            })
            .unwrap();
        assert!(shared_state.load(Ordering::Relaxed) >= 2);
        async_at_most_one.sync().unwrap();
        assert_eq!(shared_state.load(Ordering::Relaxed), 3);
        async_at_most_one.sync().unwrap();
        async_at_most_one.sync().unwrap();
        async_at_most_one.sync().unwrap();
        let state = shared_state.clone();
        async_at_most_one
            .async_run(move || {
                state.store(4, Ordering::Relaxed);
                Ok(())
            })
            .unwrap();
        async_at_most_one.sync().unwrap();
        assert_eq!(shared_state.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn test_no_work() {
        let mut async_at_most_one = AsyncAtMostOne::new("pass through".to_owned());
        async_at_most_one.sync().unwrap();
    }

    #[test]
    fn test_not_used_and_drop() {
        let _async_at_most_one = AsyncAtMostOne::new("pass through".to_owned());
    }
}
