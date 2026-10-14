//! `edit` and `write` of one file run one at a time (upstream Durable's
//! `withFileMutationQueue`): calls on the same file system id and canonical
//! path wait for each other in the order they arrived, whichever environment
//! object they got. Other files, and other file systems, never wait. Not a
//! lock against `bash` or other processes.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use noel_env::{Context, ExecutionEnv, FileErrorCode};

type Queue = Arc<tokio::sync::Mutex<()>>;

/// The queue of each file with mutations running or waiting.
static QUEUES: Mutex<BTreeMap<String, Queue>> = Mutex::new(BTreeMap::new());

fn queues() -> MutexGuard<'static, BTreeMap<String, Queue>> {
    QUEUES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A place in one file's queue; the queue goes once nobody holds one.
struct Slot {
    key: String,
    queue: Queue,
}

impl Slot {
    fn take(key: String) -> Self {
        let queue = queues().entry(key.clone()).or_default().clone();
        Self { key, queue }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut queues = queues();
        // The map's reference and this one.
        if Arc::strong_count(&self.queue) == 2 {
            queues.remove(&self.key);
        }
    }
}

/// The canonical path; for a file that does not exist yet, its canonical
/// parent joined with its name, so a `write` that creates a file and a later
/// mutation of it share one key even under a symlinked directory.
fn canonical<'a>(
    env: &'a dyn ExecutionEnv,
    absolute: &'a str,
    cx: &'a Context,
) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
    Box::pin(async move {
        let error = match env.canonical_path(absolute, cx).await {
            Ok(path) => return path,
            Err(error) => error,
        };
        // Upstream fails the call when the path cannot be inspected; noel
        // keys it by its absolute path and lets the mutation report the
        // failure in its own words.
        if error.code != FileErrorCode::NotFound {
            return absolute.to_string();
        }
        let Ok(parent) = env.join_path(&[absolute, ".."], cx).await else {
            return absolute.to_string();
        };
        if parent == absolute || !absolute.starts_with(&parent) {
            return absolute.to_string();
        }
        let name = &absolute[parent.len() + usize::from(!parent.ends_with('/'))..];
        let parent = canonical(env, &parent, cx).await;
        env.join_path(&[&parent, name], cx)
            .await
            .unwrap_or_else(|_| absolute.to_string())
    })
}

/// Runs `work`, a mutation of the file at the absolute `path` in `env`,
/// after the earlier mutations of that file.
pub(crate) async fn with_file_mutation_queue<T>(
    env: &dyn ExecutionEnv,
    path: &str,
    cx: &Context,
    work: impl Future<Output = T>,
) -> T {
    let key = format!("{}\0{}", env.id(), canonical(env, path, cx).await);
    let slot = Slot::take(key);
    let _turn = slot.queue.lock().await;
    work.await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use noel_env::LocalEnv;

    use super::*;

    #[tokio::test]
    async fn mutations_of_one_file_wait_for_each_other_and_others_do_not() {
        let dir = std::env::temp_dir().join(format!("noel-mutation-queue-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let env = LocalEnv::new(&dir);
        let cx = Context::background();
        let real = dir.join("real/new.txt").display().to_string();
        let linked = dir.join("link/new.txt").display().to_string();
        let other = dir.join("real/other.txt").display().to_string();
        let order = Mutex::new(Vec::new());
        let mutate = |path: String, name: &'static str, pause: u64| {
            let (env, cx, order) = (&env, &cx, &order);
            async move {
                with_file_mutation_queue(env, &path, cx, async {
                    order.lock().unwrap().push(format!("{name} start"));
                    tokio::time::sleep(Duration::from_millis(pause)).await;
                    order.lock().unwrap().push(format!("{name} end"));
                })
                .await
            }
        };
        // The first holds the file; the second names the same file through
        // a symlinked directory and waits; another file does not wait.
        let first = mutate(real, "first", 150);
        let second = async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            mutate(linked, "second", 0).await
        };
        let third = async {
            tokio::time::sleep(Duration::from_millis(60)).await;
            mutate(other, "other", 0).await
        };
        tokio::join!(first, second, third);
        assert_eq!(
            *order.lock().unwrap(),
            [
                "first start",
                "other start",
                "other end",
                "first end",
                "second start",
                "second end"
            ]
        );
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            !queues().keys().any(|key| key.contains(&name)),
            "queues are dropped once unused"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
